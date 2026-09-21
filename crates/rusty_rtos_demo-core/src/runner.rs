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

use core::cell::{RefCell, RefMut};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use rusty_rtos_core::config::{Config, PosixDemoConfig};
use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_core::hooks::TickHook;
use rusty_rtos_kernel::{Kernel, list_slots_for, lists_for};
use rusty_rtos_port::SimPort;

use crate::trace::LineTrace;
use crate::{
    abortdelay, apisweep, blockq, blocktim, countsem, death, dynamic, eventgroups, genqtest,
    intqueue, intsem, mbamp, messagebuffer, pollq, pollq_typed, qoverwrite, qpeek, qset, qsetpoll,
    recmutex, sbint, semtest, streambuffer, tasknotify, timerdemo,
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
    {
        list_slots_for(
            TASKS,
            TIMERS,
            lists_for(<PosixDemoConfig as Config>::MAX_PRIORITIES, QUEUES, GROUPS),
        )
    },
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
#[allow(
    clippy::large_enum_variant,
    reason = "IntQueue's two 200-byte logs are the C's own ucNormallyEmptyReceivedValues               and ucNormallyFullReceivedValues, and both halves of that scenario write               them -- so they have to live in the arm the tick hook owns. Boxing is not               available to a no_std crate without alloc, and 400 bytes of static is a               price a firmware cell can pay."
)]
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
    /// `vPeriodicStreamBufferProcessing`.
    StreamBuffer(streambuffer::Isr),
    /// `xNotifyTaskFromISR`, and the two timer callbacks with it.
    TaskNotify(tasknotify::Isr),
    /// `vTimerPeriodicISRTests`, and the four timer callbacks with it.
    TimerDemo(timerdemo::Isr),
    /// `vPeriodicEventGroupsProcessing`.
    EventGroups(eventgroups::Isr),
    /// `IntQueue`'s two timer handlers, run first-then-second per tick.
    IntQueue(intqueue::Isr),
    /// `vQueueSetAccessQueueSetFromISR`.
    QueueSet(qset::Isr),
    /// `vApiSweepAccessFromISR`.
    ApiSweep(apisweep::Isr),
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
        if matches!(kernel.tick_hook(), Self::TaskNotify(_)) {
            tasknotify::timer_callback(kernel, callback);
        }
    }

    fn tick(self, kernel: &mut SimKernel<W>) -> Self {
        match self {
            Self::None => self,
            Self::QueueOverwrite(isr) => Self::QueueOverwrite(isr.tick(kernel)),
            Self::QueueSetPolling(isr) => Self::QueueSetPolling(isr.tick(kernel)),
            Self::IntSem(isr) => Self::IntSem(isr.tick(kernel)),
            Self::StreamBufferInterrupt(isr) => Self::StreamBufferInterrupt(isr.tick(kernel)),
            Self::StreamBuffer(isr) => Self::StreamBuffer(isr.tick(kernel)),
            Self::TaskNotify(isr) => Self::TaskNotify(isr.tick(kernel)),
            Self::TimerDemo(isr) => Self::TimerDemo(isr.tick(kernel)),
            Self::EventGroups(isr) => Self::EventGroups(isr.tick(kernel)),
            Self::IntQueue(isr) => Self::IntQueue(isr.tick(kernel)),
            Self::QueueSet(isr) => Self::QueueSet(isr.tick(kernel)),
            Self::ApiSweep(isr) => Self::ApiSweep(isr.tick(kernel)),
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
        if function == apisweep::PENDED_SWEEP {
            apisweep::pended_call(kernel, param2);
        }
    }
}

/// What one dispatch of a body reports back to the runner.
///
/// `Spawned` exists so that only `death.c`'s creator -- the one body that
/// adds a task mid-run -- pays for the question. The runner used to ask
/// every body on every step.
/// A body a running task has just created, waiting to be attached.
///
/// Two scenarios create tasks with the scheduler already running --
/// `death.c`'s creator and `StreamBufferDemo.c`'s echo servers -- and
/// [`Shared`] is `Copy`, so it cannot hold a [`Body`] (one arm of which
/// borrows a future). This carries the `Copy` part across the gap.
#[derive(Debug, Clone, Copy)]
pub enum Spawn {
    /// `death.c`'s suicidal pair.
    Death(death::Body),
    /// `StreamBufferDemo.c`'s echo clients.
    StreamBuffer(streambuffer::Body),
    /// `MessageBufferDemo.c`'s echo clients.
    MessageBuffer(messagebuffer::Body),
}

impl Spawn {
    /// The body to put in the runner's table.
    fn into_body<'a>(self) -> Body<'a> {
        match self {
            Self::Death(b) => Body::Death(b),
            Self::StreamBuffer(b) => Body::StreamBuffer(b),
            Self::MessageBuffer(b) => Body::MessageBuffer(b),
        }
    }
}

pub(crate) enum Stepped {
    /// The body is a future: it reaches the kernel through the cell the
    /// runner is holding, so the runner has to let go and poll it.
    Poll,
    /// It took a step.
    Ran(Step),
    /// It took a step, and left a task for the runner to adopt.
    Spawned(Step),
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
    /// `AbortDelay.c`.
    AbortDelay(abortdelay::State),
    /// `death.c`.
    Death(death::State),
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
    /// `StreamBufferDemo.c`.
    StreamBuffer(streambuffer::State),
    /// `MessageBufferDemo.c`.
    MessageBuffer(messagebuffer::State),
    /// `IntQueue.c`.
    IntQueue(intqueue::State),
    /// `QueueSet.c`.
    QueueSet(qset::State),
    /// `ApiSweep`, ours rather than a port.
    ApiSweep(apisweep::State),
    /// `TaskNotify.c`.
    TaskNotify(tasknotify::State),
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
    /// A task a body has just created, and the body to attach to it.
    ///
    /// `death` is the only scenario whose tasks are created with the
    /// scheduler already running, and a body cannot reach `Runner::bodies`
    /// itself — it is handed the kernel and this, nothing else. So it leaves
    /// the request here and [`Runner::step_once`] drains it the moment the
    /// step returns, which is before any other task can run.
    pub spawn: Option<(TaskHandle, Spawn)>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            max_ticks: 0,
            timer_queue: QueueHandle::NULL,
            state: State::None,
            spawn: None,
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
        if let (State::TaskNotify(s), TickIsr::TaskNotify(isr)) = (&mut self.state, isr) {
            return s.still_running(isr);
        }
        if let (State::IntQueue(s), TickIsr::IntQueue(isr)) = (&mut self.state, isr) {
            return s.still_running(isr);
        }
        if let (State::QueueSet(s), TickIsr::QueueSet(isr)) = (&mut self.state, isr) {
            return s.still_running(isr);
        }
        if let (State::ApiSweep(s), TickIsr::ApiSweep(isr)) = (&mut self.state, isr) {
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
            State::AbortDelay(s) => s.still_running(),
            // UNREACHABLE by construction: `xIsCreateTaskStillRunning`
            // needs the LIVE `uxTaskGetNumberOfTasks()`, which is the
            // kernel's and not `Shared`'s, so `Check` answers `death`
            // before it ever calls this. Returning `false` rather than
            // something plausible is deliberate — were the interception
            // ever removed, the scenario must FAIL rather than quietly
            // pass a comparison of a number against itself.
            State::Death(_) => false,
            State::QPeek(s) => s.still_running(),
            State::GenQTest(s) => s.still_running(),
            // Reached only when the hook is not the matching one,
            // which means the interrupt half never ran.
            State::QOverwrite(s) => s.still_running(qoverwrite::Isr::default()),
            State::QSetPoll(s) => s.still_running(),
            State::IntSem(s) => s.still_running(),
            State::SbInt(s) => s.still_running(),
            State::StreamBuffer(s) => s.still_running(),
            State::MessageBuffer(s) => s.still_running(),
            // Reached only when the hook is not the matching one, which
            // means the interrupt half never ran.
            State::IntQueue(s) => s.still_running(intqueue::Isr::default()),
            // As above.
            State::QueueSet(s) => s.still_running(qset::Isr::default()),
            // As above.
            State::ApiSweep(s) => s.still_running(apisweep::Isr::default()),
            // Reached only when the hook is not the matching one, which
            // means the interrupt half never ran.
            State::TaskNotify(s) => s.still_running(tasknotify::Isr::default()),
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
/// It is not `Copy`, and cannot be: [`Body::Async`] holds a pinned
/// borrow of a future, and a future is exactly the thing that must not be
/// duplicated. Nothing needed it to be.
pub enum Body<'a> {
    /// A task body written as an `async fn` (mission plan, K2.2).
    ///
    /// The future is pinned by the caller and lent to the runner, because
    /// an `async fn`'s type is anonymous and cannot be named as a field.
    /// It reaches the kernel through the same [`RefCell`] the runner holds,
    /// which is why the kernel is not the runner's to own.
    ///
    /// One poll is one step, for the reason [`crate::pollq_async`] sets out
    /// at length: an `.await` has to be worth exactly one arm of a `pc`
    /// machine, so every kernel call yields after it, not only the ones
    /// that block.
    Async(Pin<&'a mut dyn Future<Output = ()>>),
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
    /// `AbortDelay.c`'s two.
    AbortDelay(abortdelay::Body),
    /// `death.c`'s two.
    Death(death::Body),
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
    /// `StreamBufferDemo.c`'s echo pairs and trigger-level test.
    StreamBuffer(streambuffer::Body),
    /// `MessageBufferDemo.c`'s echo pairs and non-blocking pair.
    MessageBuffer(messagebuffer::Body),
    /// `IntQueue.c`'s six tasks.
    IntQueue(intqueue::Body),
    /// `QueueSet.c`'s Tx and Rx pair.
    QueueSet(qset::Body),
    /// `ApiSweep`'s one task.
    ApiSweep(apisweep::Body),
    /// `TaskNotify.c`'s one.
    TaskNotify(tasknotify::Body),
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

impl fmt::Debug for Body<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A future has no `Debug`, so the variant name is the whole of it.
        f.write_str(match self {
            Self::Empty => "Empty",
            Self::Async(_) => "Async",
            Self::Idle(_) => "Idle",
            Self::Timer(_) => "Timer",
            Self::Check(_) => "Check",
            _ => "Body",
        })
    }
}

impl Body<'_> {
    /// Poll an `async` body once. Kept apart from [`Body::step`] because it
    /// must *not* be handed the kernel: the future borrows the same
    /// `RefCell`, and holding a borrow across the poll would be a
    /// double-borrow at the first kernel call it makes.
    fn poll_once(&mut self) -> Step {
        match self {
            Self::Async(future) => {
                let waker = Waker::noop();
                let mut cx = Context::from_waker(waker);
                match future.as_mut().poll(&mut cx) {
                    Poll::Pending => Step::Continue,
                    // A task body is an infinite loop in the C and must be
                    // one here; returning is the scenario's bug.
                    Poll::Ready(()) => Step::Finish(false),
                }
            }
            _ => Step::Finish(false),
        }
    }

    /// One step of this body, or `None` if it is a future and has to be
    /// polled instead.
    ///
    /// The caller used to ask `matches!(self, Body::Async(_))` before calling
    /// this, which read the discriminant a second time to answer what the
    /// table below was about to answer anyway.
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Stepped {
        Stepped::Ran(match self {
            Self::Empty => Step::Finish(false),
            // An `async` body reaches the kernel through the same cell that
            // is borrowed to get here, so it cannot be stepped from inside
            // that borrow. The caller drops it and polls.
            Self::Async(_) => return Stepped::Poll,
            // `death.c`'s two — and the only body that creates a task while
            // a run is going, so the only one asked whether it did.
            Self::Death(b) => {
                let stepped = b.step(k, s);
                return if s.spawn.is_some() {
                    Stepped::Spawned(stepped)
                } else {
                    Stepped::Ran(stepped)
                };
            }
            Self::StreamBuffer(b) => {
                let stepped = b.step(k, s);
                return if s.spawn.is_some() {
                    Stepped::Spawned(stepped)
                } else {
                    Stepped::Ran(stepped)
                };
            }
            Self::MessageBuffer(b) => {
                let stepped = b.step(k, s);
                return if s.spawn.is_some() {
                    Stepped::Spawned(stepped)
                } else {
                    Stepped::Ran(stepped)
                };
            }
            Self::IntQueue(b) => b.step(k, s),
            Self::QueueSet(b) => b.step(k, s),
            Self::ApiSweep(b) => b.step(k, s),
            Self::Idle(b) => b.step(k),
            Self::Timer(b) => b.step(k, s),
            Self::Check(b) => b.step(k, s),
            Self::Dynamic(b) => b.step(k, s),
            Self::PollQ(b) => b.step(k, s),
            Self::AbortDelay(b) => b.step(k, s),
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
            Self::TaskNotify(b) => b.step(k, s),
            Self::TimerDemo(b) => b.step(k, s),
            Self::EventGroupsMaster(b) => b.step(k, s),
            Self::EventGroupsSlave(b) => b.step(k, s),
            Self::EventGroupsSync(b) => b.step(k, s),
            Self::MbAmpCoreA(b) => b.step(k, s),
            Self::MbAmpCoreB(b) => b.step(k, s),
            Self::PollQTyped(b) => b.step(k, s),
        })
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
    #[inline(never)]
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) -> Step {
        match self.pc {
            0 => {
                // `prvCheckTasksWaitingTermination()`, which takes no
                // critical section and pays no exit while nothing has been
                // deleted — so wiring it in leaves every scenario that never
                // deletes a task byte-identical.
                k.check_tasks_waiting_termination();
                // configIDLE_SHOULD_YIELD.
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
    #[inline(never)]
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
            _ => match k.process_one_timer_command(0) {
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

    #[inline(never)]
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
                    // `xIsCreateTaskStillRunning` also calls
                    // `uxTaskGetNumberOfTasks()`, which is kernel state
                    // rather than scenario state — and it is the half of
                    // that check which proves the deletions happened.
                    if let State::Death(state) = &mut s.state {
                        let now = k.task_count();
                        return Step::Finish(state.still_running(now));
                    }
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
pub struct Runner<'a, W: fmt::Write> {
    kernel: &'a RefCell<SimKernel<W>>,
    bodies: [Body<'a>; TASKS],
    /// Borrowed for the same reason the kernel is: an `async` body reaches
    /// the scenario's statics, and cannot reach a field of the struct that
    /// polls it. `PollQ`'s counters are the case that forces it — the
    /// tasks write them and the check task reads them.
    shared: &'a RefCell<Shared>,
}

impl<'a, W: fmt::Write> Runner<'a, W> {
    /// A runner with no tasks yet.
    ///
    /// # Errors
    /// As [`Kernel::new`]: an invalid configuration or a geometry that does
    /// not add up.
    pub fn kernel_for(out: W) -> Result<RefCell<SimKernel<W>>> {
        Self::kernel_with_sink(LineTrace::new(out))
    }

    /// The kernel a runner will borrow, over a sink the caller configured.
    ///
    /// It is the caller's and not the runner's because an `async` body is a
    /// future that borrows it, and a future cannot borrow a field of the
    /// struct that polls it.
    ///
    /// # Errors
    /// As [`Kernel::new`].
    pub fn kernel_with_sink(sink: LineTrace<W>) -> Result<RefCell<SimKernel<W>>> {
        Ok(RefCell::new(Kernel::new(SimPort::new(), sink)?))
    }

    /// A runner over a kernel and a set of statics the caller is holding.
    #[must_use]
    pub fn new(kernel: &'a RefCell<SimKernel<W>>, shared: &'a RefCell<Shared>) -> Self {
        Self {
            kernel,
            bodies: array::from_fn(|_| Body::Empty),
            shared,
        }
    }

    /// The kernel, for a scenario that is building itself.
    ///
    /// A guard rather than a reference now that the kernel is shared: hold
    /// it for the length of the setup and let it go before running.
    #[must_use]
    pub fn kernel_mut(&mut self) -> RefMut<'_, SimKernel<W>> {
        self.kernel.borrow_mut()
    }

    /// The scenario's shared state.
    #[must_use]
    pub fn shared_mut(&mut self) -> RefMut<'_, Shared> {
        self.shared.borrow_mut()
    }

    /// The kernel, read-only — what a stepper reports between steps.
    #[must_use]
    pub fn kernel(&self) -> core::cell::Ref<'_, SimKernel<W>> {
        self.kernel.borrow()
    }

    /// Attach a body to a task.
    ///
    /// A handle outside the runner's table is ignored rather than
    /// panicking; the run then ends at [`Body::Empty`] with a failed
    /// verdict, which is visible where a panic would not be.
    pub fn attach(&mut self, task: TaskHandle, body: Body<'a>) {
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
        let (check, started) = {
            let mut k = self.kernel.borrow_mut();
            let check = k.create_task("CHECK", Check::PRIORITY)?;
            let started = k.start_scheduler()?;
            (check, started)
        };
        {
            let mut shared = self.shared.borrow_mut();
            shared.max_ticks = max_ticks;
            shared.timer_queue = started.timer_queue;
        }
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
        // One borrow, held across everything that does not need the cell
        // free. Picking the body reads `bodies`, which is a different field,
        // so the only arm that has to give the borrow up is the async one.
        let mut k = kernel.borrow_mut();
        if k.resume_pending() {
            return Step::Continue;
        }
        let index = usize::from(k.current().index());
        let Some(body) = bodies.get_mut(index) else {
            return Step::Finish(false);
        };
        let (step, spawned) = {
            let mut s = shared.borrow_mut();
            // An `async` body reaches the kernel through the same cell, so it
            // must be polled with no borrow outstanding. `step` says which
            // ones those are, out of the dispatch it was making anyway.
            let step = match body.step(&mut k, &mut s) {
                Stepped::Ran(step) | Stepped::Spawned(step) => step,
                Stepped::Poll => {
                    drop(s);
                    drop(k);
                    return body.poll_once();
                }
            };
            // A body that created a task leaves the handle here; this is the
            // only path by which `bodies` gains an entry after the run
            // started. Read inside the borrow that is already open: this used
            // to re-borrow `shared` a SECOND time on every step, and
            // `take()` writes `None` back even when the slot is empty, which
            // it almost always is.
            let spawned = if s.spawn.is_some() {
                s.spawn.take()
            } else {
                None
            };
            (step, spawned)
        };
        if let Some((task, spawned)) = spawned {
            if let Some(slot) = bodies.get_mut(usize::from(task.index())) {
                *slot = spawned.into_body();
            }
        }
        step
    }

    /// The verdict for a run driven by [`Runner::step_once`].
    #[must_use]
    pub fn verdict(&self, steps: u64, pass: bool, runaway: bool) -> Verdict {
        let k = self.kernel.borrow();
        Verdict {
            pass: pass && !runaway && !k.trace().failed(),
            ticks: k.tick_count(),
            yields: k.port().yields(),
            exits: k.port().exits(),
            lines: k.trace().lines(),
            steps,
            runaway,
        }
    }

    /// The sink's writer, so a deliverable can flush it once at the end.
    ///
    /// Takes the kernel rather than the runner, because the runner only
    /// ever borrowed it: drop the runner, then call this.
    pub fn into_writer(kernel: RefCell<SimKernel<W>>) -> W {
        kernel.into_inner().into_trace().into_writer()
    }

    /// Run until a body finishes the scenario or the step limit is hit.
    ///
    /// The limit is the runaway guard the first oracle run taught us to
    /// want: an 8 GB trace is not a diagnosis.
    pub fn run(&mut self, step_limit: u64) -> Verdict {
        let steps: u64;
        let mut pass = false;
        let mut runaway = false;
        {
            let Self {
                kernel,
                bodies,
                shared,
            } = self;
            // One pair of borrows for the whole run rather than one pair per
            // step. [`Runner::step_once`] gives them back on every call
            // because a conformance session drives it by hand and has to be
            // able to reach the cells between steps; this loop does not stop,
            // so it does not have to let go -- except around an `async` body,
            // which reaches the kernel through the same cell and must find it
            // free. Everything below happens in the order `step_once` does it.
            let mut k = kernel.borrow_mut();
            let mut s = shared.borrow_mut();
            // Counting down rather than up: the decrement sets the flag the
            // test reads, so the limit costs one instruction a step instead
            // of an increment and a compare. `steps` is recovered at the end
            // for the verdict.
            let mut left = step_limit;
            // Whether the step just taken made no kernel call, and so cannot
            // have left the kernel anything to settle. See [`Stepped::Quiet`].
            loop {
                let Some(next) = left.checked_sub(1) else {
                    runaway = true;
                    break;
                };
                left = next;

                let step = 'step: {
                    if k.resume_pending() {
                        break 'step Step::Continue;
                    }
                    let index = usize::from(k.current().index());
                    let Some(body) = bodies.get_mut(index) else {
                        break 'step Step::Finish(false);
                    };
                    match body.step(&mut k, &mut s) {
                        Stepped::Ran(stepped) => stepped,
                        // The handle the creator left behind; this is the
                        // only path by which `bodies` gains an entry after
                        // the run started.
                        Stepped::Spawned(stepped) => {
                            if let Some((task, spawned)) = s.spawn.take() {
                                if let Some(slot) = bodies.get_mut(usize::from(task.index())) {
                                    *slot = spawned.into_body();
                                }
                            }
                            stepped
                        }
                        // A future: it reaches the kernel through the cell
                        // this loop is holding, so let go and poll it.
                        Stepped::Poll => {
                            drop(k);
                            drop(s);
                            let polled = body.poll_once();
                            k = kernel.borrow_mut();
                            s = shared.borrow_mut();
                            polled
                        }
                    }
                };

                match step {
                    Step::Continue => {}
                    Step::Finish(verdict) => {
                        pass = verdict;
                        break;
                    }
                }
            }
            steps = step_limit.wrapping_sub(left);
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
            self.kernel.borrow_mut().trace_mut().writer_mut(),
            "KAIROS_RESULT {scenario} {outcome} ticks={ticks} yields={yields} exits={exits} lines={lines}"
        )
    }
}
