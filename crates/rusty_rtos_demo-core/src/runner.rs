//! The sim runner: what plays the part the pthreads play in the C harness.
//!
//! On the C oracle each task is a thread, and the port switches stacks. A
//! `forbid(unsafe)` kernel cannot switch a stack, so here each task is a
//! **resumable state machine**: one `step` advances it by one C statement
//! and returns, and the runner always steps whichever task the kernel says
//! is current. The two produce the same sequence of kernel calls, which is
//! all the trace records — and the trace is the gate.
//!
//! This is not a shortcut around the hard part. A task that could be
//! preempted between two C statements can be preempted between two steps
//! here, because the sim contract puts every tick inside a kernel call
//! (`ORACLES.md`, rule 3): both sides only ever switch inside the kernel.
//! What the model does cost is that a scenario must be written as a state
//! machine, which is why `rusty_rtos_demo` remakes the demo tasks rather
//! than linking them.

use core::array;
use core::fmt;

use rusty_rtos_core::config::{Config, PosixDemoConfig};
use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::{Kernel, items_for, lists_for};
use rusty_rtos_port::SimPort;

use crate::dynamic;
use crate::trace::LineTrace;

/// How many tasks a scenario may create, idle and timer included.
pub const TASKS: usize = 16;
/// How many queues a scenario may create, the timer queue included.
pub const QUEUES: usize = 4;
/// Shared queue storage, in items.
pub const SLOTS: usize = 64;

/// The kernel every scenario runs on: the `Posix_GCC` demo's configuration
/// (the one the oracle runs), the deterministic sim port, and a sink that
/// writes the contract's lines.
pub type SimKernel<W> = Kernel<
    PosixDemoConfig,
    SimPort,
    LineTrace<W>,
    TASKS,
    { items_for(TASKS) },
    { lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES) },
    QUEUES,
    SLOTS,
>;

/// What one step of a task body reports back to the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Keep going; the kernel says who runs next.
    Continue,
    /// The scenario is over, with this verdict.
    Finish(bool),
}

/// The demo scenario's shared state — the C file's statics, by name.
#[derive(Debug, Clone, Copy, Default)]
pub struct Shared {
    /// `ulCounter`.
    pub counter: u32,
    /// `usCheckVariable`.
    pub check_variable: u16,
    /// `ulExpectedValue`.
    pub expected_value: u32,
    /// `xSuspendedQueueSendError`.
    pub send_error: bool,
    /// `xSuspendedQueueReceiveError`.
    pub receive_error: bool,
    /// `xContinuousIncrementHandle`.
    pub cnt_inc: TaskHandle,
    /// `xLimitedIncrementHandle`.
    pub lim_inc: TaskHandle,
    /// `xSuspendedTestQueue`.
    pub queue: QueueHandle,
    /// The timer service task's command queue.
    pub timer_queue: QueueHandle,
    /// `usLastTaskCheck`, a static inside the check function.
    pub last_task_check: u16,
    /// `ulLastExpectedValue`, likewise.
    pub last_expected_value: u32,
    /// When the check task ends the run.
    pub max_ticks: u64,
}

impl Shared {
    /// `xAreDynamicPriorityTasksStillRunning`, statics and all.
    pub fn dynamic_still_running(&mut self) -> bool {
        let mut running = true;
        if self.check_variable == self.last_task_check {
            running = false;
        }
        if self.expected_value == self.last_expected_value {
            running = false;
        }
        if self.send_error || self.receive_error {
            running = false;
        }
        self.last_task_check = self.check_variable;
        self.last_expected_value = self.expected_value;
        running
    }
}

/// One task body. An enum rather than a `dyn` object so the runner needs no
/// allocator and a firmware can hold the whole corpus in `.bss`.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// No body attached — reaching one is a runner bug, not a scenario's.
    Empty,
    /// `prvIdleTask`.
    Idle(Idle),
    /// `prvTimerTask` with no timers registered.
    Timer(Timer),
    /// The harness's check task.
    Check(Check),
    /// `dynamic`'s five tasks.
    Dynamic(dynamic::Body),
}

impl Body {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self {
            Self::Empty => Step::Finish(false),
            Self::Idle(b) => b.step(k),
            Self::Timer(b) => b.step(k, s),
            Self::Check(b) => b.step(k, s),
            Self::Dynamic(b) => b.step(k, s),
        }
    }
}

/// `prvIdleTask`: check for terminated tasks, yield if another task shares
/// priority 0, then run the application idle hook — which on the sim is the
/// tick of rule 2.
#[derive(Debug, Clone, Copy, Default)]
pub struct Idle {
    pc: u8,
}

impl Idle {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) -> Step {
        match self.pc {
            0 => {
                // prvCheckTasksWaitingTermination is a no-op while nothing
                // has been deleted, then: configIDLE_SHOULD_YIELD.
                if k.ready_len(0).unwrap_or(0) > 1 {
                    k.task_yield();
                }
                self.pc = 1;
            }
            _ => {
                k.idle_hook_tick();
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvTimerTask` with an empty timer list: block on the command queue for
/// ever, which is the whole of its trace in the K1 corpus.
#[derive(Debug, Clone, Copy, Default)]
pub struct Timer {
    pc: u8,
}

impl Timer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self.pc {
            // prvProcessTimerOrBlockTask opens with vTaskSuspendAll().
            0 => {
                k.suspend_all();
                self.pc = 1;
            }
            // Both timer lists are empty, so the wait is indefinite.
            1 => {
                let _ = k.wait_for_message_restricted(s.timer_queue, 0, true);
                self.pc = 2;
            }
            _ => {
                if !k.resume_all() {
                    k.task_yield();
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// The harness's check task: wake every 100 ticks and end the run at
/// `max_ticks`, exactly as `oracle/harness/main.c` does.
#[derive(Debug, Clone, Copy, Default)]
pub struct Check {
    pc: u8,
}

impl Check {
    /// The C `harnessCHECK_PERIOD_TICKS`.
    pub const PERIOD: u64 = 100;

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self.pc {
            0 => {
                let _ = k.delay(Self::PERIOD);
                self.pc = 1;
                Step::Continue
            }
            _ => {
                self.pc = 0;
                if k.tick_count() >= s.max_ticks {
                    Step::Finish(s.dynamic_still_running())
                } else {
                    Step::Continue
                }
            }
        }
    }
}

/// What a finished run reports — the C harness's `KAIROS_RESULT` line.
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    /// Whether the scenario's own check passed.
    pub pass: bool,
    /// `xTaskGetTickCount()` at the end.
    pub ticks: u64,
    /// `ulKairosYields`.
    pub yields: u64,
    /// `ulKairosExits`.
    pub exits: u64,
    /// Trace lines written.
    pub lines: u64,
    /// Steps the runner took, its own safety counter.
    pub steps: u64,
    /// Set when the run hit the step limit instead of finishing.
    pub runaway: bool,
}

/// The runner: a kernel, one body per task, and the scenario's statics.
pub struct Runner<W: fmt::Write> {
    kernel: SimKernel<W>,
    bodies: [Body; TASKS],
    shared: Shared,
}

impl<W: fmt::Write> Runner<W> {
    /// A runner with no tasks yet.
    ///
    /// # Errors
    /// As [`Kernel::new`]: an invalid configuration or a geometry that does
    /// not add up.
    pub fn new(out: W) -> Result<Self> {
        Self::with_sink(LineTrace::new(out))
    }

    /// A runner over a sink the caller configured — a conformance session
    /// turns on the exit column.
    ///
    /// # Errors
    /// As [`Kernel::new`].
    pub fn with_sink(sink: LineTrace<W>) -> Result<Self> {
        Ok(Self {
            kernel: Kernel::new(SimPort::new(), sink)?,
            bodies: array::from_fn(|_| Body::Empty),
            shared: Shared::default(),
        })
    }

    /// The kernel, for a scenario that is building itself.
    pub const fn kernel_mut(&mut self) -> &mut SimKernel<W> {
        &mut self.kernel
    }

    /// The scenario's shared state.
    pub const fn shared_mut(&mut self) -> &mut Shared {
        &mut self.shared
    }

    /// The kernel, read-only — what a stepper reports between steps.
    pub const fn kernel(&self) -> &SimKernel<W> {
        &self.kernel
    }

    /// Advance the scenario by one step: pay any debt the running task owes
    /// from a call it was preempted inside, or run one statement of its
    /// body.
    ///
    /// [`Runner::run`] is this in a loop. A conformance session drives it by
    /// hand instead, printing the kernel's counters between steps, because a
    /// divergence from the oracle is almost never a disagreement about an
    /// event — it is a disagreement about how many critical sections have
    /// been left, which is where sim time comes from.
    pub fn step_once(&mut self) -> Step {
        let Self {
            kernel,
            bodies,
            shared,
        } = self;
        if kernel.resume_pending() {
            return Step::Continue;
        }
        let index = usize::from(kernel.current().index());
        match bodies.get_mut(index) {
            Some(body) => body.step(kernel, shared),
            None => Step::Finish(false),
        }
    }

    /// The verdict for a run driven by [`Runner::step_once`].
    #[must_use]
    pub fn verdict(&self, steps: u64, pass: bool, runaway: bool) -> Verdict {
        Verdict {
            pass: pass && !runaway && !self.kernel.trace().failed(),
            ticks: self.kernel.tick_count(),
            yields: self.kernel.port().yields(),
            exits: self.kernel.port().exits(),
            lines: self.kernel.trace().lines(),
            steps,
            runaway,
        }
    }

    /// Attach a body to a task.
    ///
    /// A handle outside the runner's table is ignored rather than
    /// panicking; the run then ends at [`Body::Empty`] with a failed
    /// verdict, which is visible where a panic would not be.
    pub fn attach(&mut self, task: TaskHandle, body: Body) {
        if let Some(slot) = self.bodies.get_mut(usize::from(task.index())) {
            *slot = body;
        }
    }

    /// Run until a body finishes the scenario or the step limit is hit.
    ///
    /// The limit is the runaway guard the first oracle run taught us to
    /// want: an 8 GB trace is not a diagnosis.
    pub fn run(&mut self, step_limit: u64) -> Verdict {
        let Self {
            kernel,
            bodies,
            shared,
        } = self;
        let mut steps: u64 = 0;
        let mut pass = false;
        let mut runaway = false;
        loop {
            if steps >= step_limit {
                runaway = true;
                break;
            }
            steps = steps.wrapping_add(1);
            // A task switched out mid-call resumes inside that call, not at
            // its next statement.
            if kernel.resume_pending() {
                continue;
            }
            let index = usize::from(kernel.current().index());
            let Some(body) = bodies.get_mut(index) else {
                break;
            };
            match body.step(kernel, shared) {
                Step::Continue => {}
                Step::Finish(verdict) => {
                    pass = verdict;
                    break;
                }
            }
        }
        Verdict {
            pass: pass && !runaway && !self.kernel.trace().failed(),
            ticks: self.kernel.tick_count(),
            yields: self.kernel.port().yields(),
            exits: self.kernel.port().exits(),
            lines: self.kernel.trace().lines(),
            steps,
            runaway,
        }
    }

    /// The sink's writer, so a deliverable can flush it once at the end.
    pub fn into_writer(self) -> W {
        self.kernel.into_trace().into_writer()
    }

    /// Write the verdict line the C harness writes, and hand back the
    /// writer.
    ///
    /// # Errors
    /// Propagates a write failure from the sink.
    pub fn finish(&mut self, scenario: &str, verdict: &Verdict) -> fmt::Result {
        let outcome = if verdict.pass { "pass" } else { "fail" };
        let Verdict {
            ticks,
            yields,
            exits,
            lines,
            ..
        } = *verdict;
        // The C prints `lines=` counting every trace line but not itself.
        writeln!(
            self.kernel.trace_mut().writer_mut(),
            "KAIROS_RESULT {scenario} {outcome} ticks={ticks} yields={yields} exits={exits} lines={lines}"
        )
    }
}
