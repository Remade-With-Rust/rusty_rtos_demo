//! The two-core corpus: the same demo bodies, on a two-core kernel, under the
//! two-core sim contract (`oracle/harness-smp/portmacro.h`, ORACLES.md).
//!
//! Built with `--features smp`, which points [`crate::runner::SimKernel`] at
//! [`PosixDemoSmpConfig`] and [`SmpPort`]. The bodies are unchanged: a body
//! is one C statement per step whatever the core count, and the contract is
//! chosen so that a turn boundary on the C side always falls between two of
//! its steps.
//!
//! # The contract, from this side
//!
//! Cores alternate turns, core 0 first. A turn ends after the step that:
//! - made a kernel call that left at least one critical section;
//! - switched this core to another task (the core's own yield, taken
//!   between steps, which is where interrupts are open);
//! - finished one pass of an idle task.
//!
//! A core whose partner holds the scheduler suspended is skipped: on silicon
//! it would spin on the task lock at its next critical section. A yield for
//! the other core waits for that core's next turn. Every second turn ends
//! with a tick on core 0, delivered between steps, never inside a call.

use core::cell::Cell;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use rusty_rtos_core::config::Config;
use rusty_rtos_core::isr::Woken;
use rusty_rtos_core::port::Port;
use rusty_rtos_core::tick::Bits64;

use crate::runner::{Body, Runner, SimKernel, Step, Stepped, Verdict};

/// `oracle/harness-smp`'s configuration: the one-core harness's, field for
/// field, on two cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PosixDemoSmpConfig;

impl Config for PosixDemoSmpConfig {
    type Tick = Bits64;
    const TICK_RATE_HZ: u32 = 1000;
    const DYNAMIC_ALLOCATION: bool = true;
    const PORT_STACK_INIT_CRITICAL: bool = true;
    const MAX_PRIORITIES: u8 = 7;
    const MINIMAL_STACK_SIZE: usize = 128;
    const MAX_TASK_NAME_LEN: usize = 12;
    const QUEUE_REGISTRY_SIZE: usize = 20;
    const TIMER_TASK_PRIORITY: u8 = 6;
    const TIMER_QUEUE_LENGTH: usize = 20;
    const TIMER_TASK_STACK_DEPTH: usize = 256;
    const CHECK_FOR_STACK_OVERFLOW: u8 = 0;
    const USE_TICK_HOOK: bool = true;
    const NOTIFICATION_ARRAY_ENTRIES: usize = 3;
    const MESSAGE_LENGTH_BYTES: usize = 8;
    const TOTAL_HEAP_SIZE: usize = 65 * 1024;
    const MAX_TASKS: usize = 64;
    const MAX_QUEUES: usize = 64;
    const MAX_TIMERS: usize = 32;
    const MAX_EVENT_GROUPS: usize = 8;
    const MAX_STREAM_BUFFERS: usize = 16;
    const NUMBER_OF_CORES: u8 = 2;
}

/// The two-core sim port: a settable core, per-core nesting, and every yield
/// RECORDED for the runner to take between steps.
#[derive(Debug, Default)]
pub struct SmpPort {
    core: Cell<u8>,
    nesting: [Cell<u32>; 2],
    exits: Cell<u64>,
    yields: Cell<u64>,
    started: Cell<bool>,
    in_isr: Cell<bool>,
    /// A yield each core asked of ITSELF, by bit.
    own: Cell<u8>,
}

impl SmpPort {
    /// A port with the scheduler not yet started.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            core: Cell::new(0),
            nesting: [Cell::new(0), Cell::new(0)],
            exits: Cell::new(0),
            yields: Cell::new(0),
            started: Cell::new(false),
            in_isr: Cell::new(false),
            own: Cell::new(0),
        }
    }

    /// `ulKairosExits`.
    #[must_use]
    pub fn exits(&self) -> u64 {
        self.exits.get()
    }

    /// The current core's critical nesting, for the diagnostic stepper.
    #[must_use]
    pub fn nesting(&self) -> u32 {
        self.nesting_of().get()
    }

    /// `ulKairosYields`.
    #[must_use]
    pub fn yields(&self) -> u64 {
        self.yields.get()
    }

    fn set_core(&self, core: u8) {
        self.core.set(core);
    }

    fn set_isr(&self, yes: bool) {
        self.in_isr.set(yes);
    }

    /// Whether `core` asked to switch itself since the last call.
    fn take_own(&self, core: u8) -> bool {
        let bit = 1u8 << core;
        let own = self.own.get();
        self.own.set(own & !bit);
        own & bit != 0
    }

    fn nesting_of(&self) -> &Cell<u32> {
        let [zero, one] = &self.nesting;
        if self.core.get() & 1 == 0 { zero } else { one }
    }
}

impl Port for SmpPort {
    const COMMITS_SWITCH: bool = true;

    fn yield_now(&self) {
        if !self.in_isr.get() {
            self.yields.set(self.yields.get().wrapping_add(1));
        }
        self.own.set(self.own.get() | (1 << self.core.get()));
    }

    fn yield_from_isr(&self, woken: Woken) {
        if woken == Woken::YES {
            self.own.set(self.own.get() | (1 << self.core.get()));
        }
    }

    fn enter_critical(&self) {
        let n = self.nesting_of();
        n.set(n.get().wrapping_add(1));
    }

    fn exit_critical(&self) {
        let n = self.nesting_of();
        let left = n.get().saturating_sub(1);
        n.set(left);
        if left == 0 && self.started.get() && !self.in_isr.get() {
            self.exits.set(self.exits.get().wrapping_add(1));
        }
    }

    fn set_interrupt_mask_from_isr(&self) -> u32 {
        0
    }

    fn clear_interrupt_mask_from_isr(&self, _saved: u32) {}

    fn in_isr(&self) -> bool {
        self.in_isr.get()
    }

    fn core_id(&self) -> u8 {
        self.core.get()
    }

    fn exits(&self) -> u64 {
        SmpPort::exits(self)
    }

    fn scheduler_started(&self) {
        self.started.set(true);
    }
}

/// `prvIdleTask` (core 0's, `active`) and `prvPassiveIdleTask` (core 1's),
/// SMP shape: a `taskYIELD()` before the loop; each pass checks for reaped
/// tasks (active only), yields if more idle-priority tasks are ready than
/// there are cores, then runs its hook(s) -- each the end of a turn. The
/// active idle task runs the idle hook and then the passive one.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdleSmp {
    /// `prvIdleTask` rather than `prvPassiveIdleTask`.
    pub active: bool,
    pc: u8,
}

impl IdleSmp {
    /// The two idle bodies.
    #[must_use]
    pub const fn new(active: bool) -> Self {
        Self { active, pc: 0 }
    }

    /// Whether the NEXT step is a hook, which ends the turn.
    #[must_use]
    pub const fn at_hook(&self) -> bool {
        self.pc >= 2
    }

    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) -> Step {
        match self.pc {
            0 => {
                k.task_yield();
                self.pc = 1;
            }
            1 => {
                if self.active {
                    k.check_tasks_waiting_termination();
                }
                if k.ready_len(0).unwrap_or(0) > usize::from(PosixDemoSmpConfig::NUMBER_OF_CORES) {
                    k.task_yield();
                }
                self.pc = 2;
            }
            // `vApplicationIdleHook()`. On SMP `prvIdleTask` then ALSO calls
            // `vApplicationPassiveIdleHook()` (tasks.c, the end of its loop),
            // so the active idle task has two hooks a pass, and the C port
            // ends a turn in each. Nothing runs here; the step is the turn.
            2 if self.active => self.pc = 3,
            _ => self.pc = 1,
        }
        Step::Continue
    }
}

/// Print a `# turn` line per turn into the trace, as the C port does under
/// `KAIROS_SMP_DEBUG`: the two sequences side by side are the diagnosis.
pub static DEBUG_TURNS: AtomicBool = AtomicBool::new(false);

/// Ticks per turn: one tick a round.
const TURNS_PER_TICK: u64 = 2;

impl<W: fmt::Write> Runner<'_, W> {
    /// [`Runner::run`] on two cores, under the two-core contract.
    pub(crate) fn run_smp(&mut self, step_limit: u64) -> Verdict {
        let mut left = step_limit;
        let mut pass = false;
        let mut runaway = false;
        let mut pending: u8 = 0;
        // The core holding the scheduler suspended, if any.
        let mut holder: Option<u8> = None;
        let mut turn: u64 = 0;
        'run: loop {
            let c = (turn & 1) as u8;
            let skipped = holder.is_some_and(|h| h != c);
            if DEBUG_TURNS.load(Ordering::Relaxed) {
                let mut k = self.kernel.borrow_mut();
                let n0 = k.name_of(k.current_on(0)).ok();
                let n1 = k.name_of(k.current_on(1)).ok();
                let lock = holder.map_or(-1, i32::from);
                let _ = writeln!(
                    k.trace_mut().writer_mut(),
                    "# turn {turn} core {c} cur0={} cur1={} pend={}{} lock={lock}",
                    n0.as_ref().map_or("?", |n| n.as_str()),
                    n1.as_ref().map_or("?", |n| n.as_str()),
                    pending & 1,
                    (pending >> 1) & 1
                );
            }
            if !skipped {
                let mut k = self.kernel.borrow_mut();
                k.port().set_core(c);
                if pending & (1 << c) != 0 {
                    pending &= !(1 << c);
                    k.switch_context();
                }
                drop(k);
                loop {
                    let Some(next) = left.checked_sub(1) else {
                        runaway = true;
                        break 'run;
                    };
                    left = next;
                    let mut k = self.kernel.borrow_mut();
                    k.port().set_core(c);
                    let task = k.current();
                    let index = task.index() as usize;
                    let before = k.port().exits();
                    let mut s = self.shared.borrow_mut();
                    let Some(body) = self.bodies.get_mut(index) else {
                        break 'run;
                    };
                    let idle_pass = matches!(body, Body::IdleSmp(i) if i.at_hook());
                    let step = match body.step(&mut k, &mut s) {
                        Stepped::Ran(step) => step,
                        Stepped::Spawned(step) => {
                            if let Some((t, spawned)) = s.spawn.take() {
                                if let Some(slot) = self.bodies.get_mut(t.index() as usize) {
                                    *slot = spawned.into_body();
                                }
                            }
                            step
                        }
                        // No async bodies in the two-core corpus.
                        Stepped::Poll => Step::Finish(false),
                    };
                    drop(s);
                    if let Step::Finish(verdict) = step {
                        pass = verdict;
                        break 'run;
                    }
                    pending |= k.take_core_yields();
                    holder = if k.scheduler_suspended() != 0 {
                        Some(c)
                    } else {
                        None
                    };
                    let made = k.port().exits() > before;
                    let mut ended = made || idle_pass;
                    if k.port().take_own(c) {
                        k.switch_context();
                        pending |= k.take_core_yields();
                        if k.current() != task {
                            ended = true;
                        }
                    }
                    if ended {
                        break;
                    }
                }
            }
            turn = turn.wrapping_add(1);
            if turn % TURNS_PER_TICK == 0 {
                let mut k = self.kernel.borrow_mut();
                k.port().set_core(0);
                k.port().set_isr(true);
                if k.increment_tick() {
                    pending |= 1;
                }
                pending |= k.take_core_yields();
                // A yield the tick asked of core 0 through the port.
                if k.port().take_own(0) {
                    pending |= 1;
                }
                k.port().set_isr(false);
            }
        }
        self.verdict(step_limit.wrapping_sub(left), pass, runaway)
    }
}
