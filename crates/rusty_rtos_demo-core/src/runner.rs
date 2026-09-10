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
//!
//! A **blocking** call costs one thing more. `xQueueReceive( q, &v, 100 )`
//! stops the C task inside the call; here it returns `Blocked` and the body
//! makes the same call again, at the same `pc`, when it next runs — which
//! is exactly when the C thread would have resumed. See
//! `rusty_rtos_kernel::queue`.

use core::array;
use core::fmt;

use rusty_rtos_core::config::{Config, PosixDemoConfig};
use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_kernel::{Kernel, items_for, lists_for};
use rusty_rtos_port::SimPort;

use crate::trace::LineTrace;
use crate::{
    blockq, blocktim, countsem, dynamic, eventgroups, genqtest, intsem, mbamp, pollq, pollq_typed,
    qoverwrite, qpeek, qsetpoll, recmutex, sbint, semtest, timerdemo,
};

/// How many tasks a scenario may create, idle and timer included.
pub const TASKS: usize = 24;
/// How many queues, semaphores and mutexes a scenario may create, the timer
/// command queue included.
pub const QUEUES: usize = 12;
/// Shared queue storage, in items.
pub const SLOTS: usize = 128;

/// How many stream and message buffers the corpus needs at once.
///
/// `MessageBufferAMP` is the greediest: two message buffers plus the ones
/// `StreamBufferDemo` makes and remakes.
pub const BUFFERS: usize = 8;

/// The byte arena every stream buffer's ring comes out of.
pub const BYTES: usize = 2048;

/// How many software timers the corpus needs at once. `TimerDemo` is the
/// greediest: one auto-reload timer per test plus the one-shots.
pub const TIMERS: usize = 32;

/// How many event groups the corpus needs at once. `EventGroupsDemo` makes
/// three: the one its own tasks share and the two the rendezvous test uses.
pub const GROUPS: usize = 4;

/// The kernel every scenario runs on: the `Posix_GCC` demo's configuration
/// (the one the oracle runs), the deterministic sim port, and a sink that
/// writes the contract's lines.
pub type SimKernel<W> = Kernel<
    PosixDemoConfig,
    SimPort,
    LineTrace<W>,
    TickIsr,
    TASKS,
    { items_for(TASKS, TIMERS) },
    { lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES, GROUPS) },
    QUEUES,
    SLOTS,
    BUFFERS,
    BYTES,
    TIMERS,
    GROUPS,
>;

/// `vApplicationTickHook`: the interrupt half of whichever scenario is
/// running.
///
/// Upstream's own Posix demo runs every scenario at once and its tick hook
/// calls all of their periodic ISR functions in turn
/// (`vFullDemoTickHookFunction` in `Demo/Posix_GCC/main_full.c`). The
/// Kairos harness runs one scenario per trace, so this dispatches to that
/// one and no other — which is what keeps a trace attributable.
///
/// It is `Copy` because the kernel owns it and copies it out to run it:
/// that is how the hook gets `&mut` the kernel it lives in without
/// borrowing itself twice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TickIsr {
    /// No interrupt half — the scenario has none, or none is installed.
    #[default]
    None,
    /// `vQueueOverwritePeriodicISRDemo`.
    QueueOverwrite(qoverwrite::Isr),
    /// `vQueueSetPollingInterruptAccess`.
    QueueSetPolling(qsetpoll::Isr),
    /// `vInterruptSemaphorePeriodicTest`.
    IntSem(intsem::Isr),
    /// `vBasicStreamBufferSendFromISR`.
    StreamBufferInterrupt(sbint::Isr),
    /// `vTimerPeriodicISRTests`, and the four timer callbacks with it.
    TimerDemo(timerdemo::Isr),
    /// `vPeriodicEventGroupsProcessing`.
    EventGroups(eventgroups::Isr),
    /// `MessageBufferAMP`'s replaced `sbSEND_COMPLETED`. It has no tick
    /// half at all — the seam it uses is the send, not the timer.
    MessageBufferAmp(mbamp::Isr),
}

impl<W: fmt::Write> TickHook<SimKernel<W>> for TickIsr {
    fn timer(
        kernel: &mut SimKernel<W>,
        timer: rusty_rtos_core::handle::TimerHandle,
        callback: u16,
        _id: u64,
    ) {
        if matches!(kernel.tick_hook(), Self::TimerDemo(_)) {
            timerdemo::timer_callback(kernel, timer, callback);
        }
    }

    fn tick(self, kernel: &mut SimKernel<W>) -> Self {
        match self {
            Self::None => self,
            Self::QueueOverwrite(isr) => Self::QueueOverwrite(isr.tick(kernel)),
            Self::QueueSetPolling(isr) => Self::QueueSetPolling(isr.tick(kernel)),
            Self::IntSem(isr) => Self::IntSem(isr.tick(kernel)),
            Self::StreamBufferInterrupt(isr) => Self::StreamBufferInterrupt(isr.tick(kernel)),
            Self::TimerDemo(isr) => Self::TimerDemo(isr.tick(kernel)),
            Self::EventGroups(isr) => Self::EventGroups(isr.tick(kernel)),
            Self::MessageBufferAmp(_) => self,
        }
    }

    /// `PendedFunction_t`: what the daemon task runs on behalf of an
    /// interrupt. The two event-group deferrals are the kernel's own, so
    /// they go straight back to it.
    fn send_completed(
        kernel: &mut SimKernel<W>,
        buffer: rusty_rtos_core::handle::StreamBufferHandle,
    ) -> bool {
        mbamp::send_completed(kernel, buffer)
    }

    fn pended(kernel: &mut SimKernel<W>, function: u16, param1: u64, param2: u64) {
        if matches!(
            function,
            rusty_rtos_kernel::events::PENDED_SET_BITS
                | rusty_rtos_kernel::events::PENDED_CLEAR_BITS
        ) {
            let _ = kernel.event_group_pended_call(function, param1, param2);
        }
    }
}

/// What one step of a task body reports back to the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Keep going; the kernel says who runs next.
    Continue,
    /// The scenario is over, with this verdict.
    Finish(bool),
}

/// The statics of whichever scenario is running.
///
/// A C demo file keeps its state in file-scope variables; this is that, one
/// variant per file, so a scenario cannot accidentally read another's.
#[derive(Debug, Clone, Copy)]
pub enum State {
    /// Nothing started yet.
    None,
    /// `dynamic.c`.
    Dynamic(dynamic::State),
    /// `PollQ.c`.
    PollQ(pollq::State),
    /// `BlockQ.c`.
    BlockQ(blockq::State),
    /// `semtest.c`.
    SemTest(semtest::State),
    /// `countsem.c`.
    CountSem(countsem::State),
    /// `recmutex.c`.
    RecMutex(recmutex::State),
    /// `blocktim.c`.
    BlockTim(blocktim::State),
    /// `QPeek.c`.
    QPeek(qpeek::State),
    /// `GenQTest.c`.
    GenQTest(genqtest::State),
    /// `QueueOverwrite.c`.
    QOverwrite(qoverwrite::State),
    /// `QueueSetPolling.c`.
    QSetPoll(qsetpoll::State),
    /// `IntSemTest.c`.
    IntSem(intsem::State),
    /// `StreamBufferInterrupt.c`.
    SbInt(sbint::State),
    /// `TimerDemo.c`.
    TimerDemo(timerdemo::State),
    /// `EventGroupsDemo.c`.
    EventGroups(eventgroups::State),
    /// `MessageBufferAMP.c`.
    MbAmp(mbamp::State),
    /// `PollQ.c` again, against the Rust face (K2.1).
    PollQTyped(pollq_typed::State),
}

/// What every task body can reach: the scenario's statics, plus the two
/// things the harness owns.
#[derive(Debug, Clone, Copy)]
pub struct Shared {
    /// When the check task ends the run.
    pub max_ticks: u64,
    /// The timer service task's command queue.
    pub timer_queue: QueueHandle,
    /// The running scenario's statics.
    pub state: State,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            max_ticks: 0,
            timer_queue: QueueHandle::NULL,
            state: State::None,
        }
    }
}

impl Shared {
    /// The scenario's own `xAre...StillRunning()`.
    pub fn still_running(&mut self, isr: TickIsr) -> bool {
        if let (State::QOverwrite(s), TickIsr::QueueOverwrite(isr)) = (&mut self.state, isr) {
            return s.still_running(isr);
        }
        if let (State::TimerDemo(s), TickIsr::TimerDemo(isr)) = (&mut self.state, isr) {
            return s.still_running(isr, Check::PERIOD);
        }
        if let (State::EventGroups(s), TickIsr::EventGroups(isr)) = (&mut self.state, isr) {
            return s.still_running(isr);
        }
        match &mut self.state {
            State::None => false,
            State::Dynamic(s) => s.still_running(),
            State::PollQ(s) => s.still_running(),
            State::BlockQ(s) => s.still_running(),
            State::SemTest(s) => s.still_running(),
            State::CountSem(s) => s.still_running(),
            State::RecMutex(s) => s.still_running(),
            State::BlockTim(s) => s.still_running(),
            State::QPeek(s) => s.still_running(),
            State::GenQTest(s) => s.still_running(),
            // Reached only when the hook is not the matching one,
            // which means the interrupt half never ran.
            State::QOverwrite(s) => s.still_running(qoverwrite::Isr::default()),
            State::QSetPoll(s) => s.still_running(),
            State::IntSem(s) => s.still_running(),
            State::SbInt(s) => s.still_running(),
            // Reached only when the hook is not the matching one.
            State::TimerDemo(s) => s.still_running(timerdemo::Isr::default(), Check::PERIOD),
            // As above.
            State::EventGroups(s) => s.still_running(eventgroups::Isr::default()),
            State::MbAmp(s) => s.still_running(),
            State::PollQTyped(s) => s.still_running(),
        }
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
    /// `dynamic.c`'s five tasks.
    Dynamic(dynamic::Body),
    /// `PollQ.c`'s two.
    PollQ(pollq::Body),
    /// `BlockQ.c`'s six.
    BlockQ(blockq::Body),
    /// `semtest.c`'s four.
    SemTest(semtest::Body),
    /// `countsem.c`'s two.
    CountSem(countsem::Body),
    /// `recmutex.c`'s three.
    RecMutex(recmutex::Body),
    /// `blocktim.c`'s two.
    BlockTim(blocktim::Body),
    /// `QPeek.c`'s four.
    QPeek(qpeek::Body),
    /// `GenQTest.c`'s five.
    GenQTest(genqtest::Body),
    /// `QueueOverwrite.c`'s one.
    QOverwrite(qoverwrite::Body),
    /// `QueueSetPolling.c`'s one.
    QSetPoll(qsetpoll::Body),
    /// `IntSemTest.c`'s three.
    IntSem(intsem::Body),
    /// `StreamBufferInterrupt.c`'s one.
    SbInt(sbint::Body),
    /// `TimerDemo.c`'s one.
    TimerDemo(timerdemo::Body),
    /// `EventGroupsDemo.c`'s master.
    EventGroupsMaster(eventgroups::Master),
    /// Its slave.
    EventGroupsSlave(eventgroups::Slave),
    /// And its two rendezvous tasks.
    EventGroupsSync(eventgroups::Sync),
    /// `MessageBufferAMP.c`'s writer.
    MbAmpCoreA(mbamp::CoreA),
    /// And its two readers.
    MbAmpCoreB(mbamp::CoreB),
    /// `PollQ.c`'s two, against the Rust face.
    PollQTyped(pollq_typed::Body),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self {
            Self::Empty => Step::Finish(false),
            Self::Idle(b) => b.step(k),
            Self::Timer(b) => b.step(k, s),
            Self::Check(b) => b.step(k, s),
            Self::Dynamic(b) => b.step(k, s),
            Self::PollQ(b) => b.step(k, s),
            Self::BlockQ(b) => b.step(k, s),
            Self::SemTest(b) => b.step(k, s),
            Self::CountSem(b) => b.step(k, s),
            Self::RecMutex(b) => b.step(k, s),
            Self::BlockTim(b) => b.step(k, s),
            Self::QPeek(b) => b.step(k, s),
            Self::GenQTest(b) => b.step(k, s),
            Self::QOverwrite(b) => b.step(k, s),
            Self::QSetPoll(b) => b.step(k, s),
            Self::IntSem(b) => b.step(k, s),
            Self::SbInt(b) => b.step(k, s),
            Self::TimerDemo(b) => b.step(k, s),
            Self::EventGroupsMaster(b) => b.step(k, s),
            Self::EventGroupsSlave(b) => b.step(k, s),
            Self::EventGroupsSync(b) => b.step(k, s),
            Self::MbAmpCoreA(b) => b.step(k, s),
            Self::MbAmpCoreB(b) => b.step(k, s),
            Self::PollQTyped(b) => b.step(k, s),
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
    /// `xNextExpireTime`.
    next_expire: u64,
    /// `xListWasEmpty`.
    list_was_empty: bool,
    /// `xTimeNow`.
    now: u64,
}

impl Timer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self.pc {
            // xNextExpireTime = prvGetNextExpireTime( &xListWasEmpty );
            0 => {
                let (next, empty) = k.timer_next_expire();
                self.next_expire = next;
                self.list_was_empty = empty;
                self.pc = 1;
            }
            // prvProcessTimerOrBlockTask opens with vTaskSuspendAll().
            1 => {
                k.suspend_all();
                self.pc = 2;
            }
            // xTimeNow = prvSampleTimeNow( &xTimerListsWereSwitched );
            //
            // The sample and the wait that uses it are one arm, not two.
            // Neither the tick read nor the list walk takes a critical
            // section, so no tick can land between them in the C — and a
            // step boundary here would let one land in ours, which costs
            // the wait a tick and moves every line after it.
            2 => {
                let (now, switched) = k.timer_sample_time_now().unwrap_or((0, false));
                self.now = now;
                if switched {
                    // The lists were swapped under us, so this pass is over.
                    self.pc = 7;
                } else if !self.list_was_empty && self.next_expire <= now {
                    self.pc = 3;
                } else {
                    if self.list_was_empty {
                        self.list_was_empty = k.overflow_timer_list_is_empty();
                    }
                    let wait = self.next_expire.wrapping_sub(self.now);
                    let _ = k.wait_for_message_restricted(s.timer_queue, wait, self.list_was_empty);
                    self.pc = 6;
                }
            }
            // ( void ) xTaskResumeAll(); prvProcessExpiredTimer( ... );
            3 => {
                let _ = k.resume_all();
                self.pc = 4;
            }
            4 => {
                let _ = k.process_expired_timer(self.next_expire, self.now);
                self.pc = 8;
            }
            // The other arm: block on the queue until the head is due.
            6 => {
                if !k.resume_all() {
                    k.task_yield();
                }
                self.pc = 8;
            }
            // The switched-lists arm's resume.
            7 => {
                let _ = k.resume_all();
                self.pc = 8;
            }
            // prvProcessReceivedCommands(): the C loops until the queue is
            // empty, and one command per step is that loop unrolled — a
            // callback in the middle of it can block, and the daemon has to
            // be able to come back.
            _ => match k.process_one_timer_command() {
                Ok(rusty_rtos_kernel::queue::Wait::Ready(true)) => {}
                _ => self.pc = 0,
            },
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
    /// The C `harnessCHECK_TASK_PRIORITY`.
    pub const PRIORITY: u8 = 5;

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
                    // A scenario's `xAre...StillRunning()` reads what its
                    // interrupt half latched as well as what its task half
                    // did, so the hook comes along for the ride.
                    let isr = *k.tick_hook();
                    Step::Finish(s.still_running(isr))
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

    /// Create the check task and start the scheduler — the part every
    /// scenario does identically, in the order `oracle/harness/main.c` does
    /// it: the scenario's own tasks first, then `CHECK`, then
    /// `vTaskStartScheduler`.
    ///
    /// # Errors
    /// As the kernel's create calls.
    pub fn start_common(&mut self, max_ticks: u64) -> Result<()> {
        let check = self.kernel.create_task("CHECK", Check::PRIORITY)?;
        let started = self.kernel.start_scheduler()?;
        self.shared.max_ticks = max_ticks;
        self.shared.timer_queue = started.timer_queue;
        self.attach(check, Body::Check(Check::default()));
        self.attach(started.idle, Body::Idle(Idle::default()));
        self.attach(started.timer, Body::Timer(Timer::default()));
        Ok(())
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

    /// The sink's writer, so a deliverable can flush it once at the end.
    pub fn into_writer(self) -> W {
        self.kernel.into_trace().into_writer()
    }

    /// Run until a body finishes the scenario or the step limit is hit.
    ///
    /// The limit is the runaway guard the first oracle run taught us to
    /// want: an 8 GB trace is not a diagnosis.
    pub fn run(&mut self, step_limit: u64) -> Verdict {
        let mut steps: u64 = 0;
        let mut pass = false;
        let mut runaway = false;
        loop {
            if steps >= step_limit {
                runaway = true;
                break;
            }
            steps = steps.wrapping_add(1);
            match self.step_once() {
                Step::Continue => {}
                Step::Finish(verdict) => {
                    pass = verdict;
                    break;
                }
            }
        }
        self.verdict(steps, pass, runaway)
    }

    /// Write the verdict line the C harness writes.
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
        writeln!(
            self.kernel.trace_mut().writer_mut(),
            "KAIROS_RESULT {scenario} {outcome} ticks={ticks} yields={yields} exits={exits} lines={lines}"
        )
    }
}
