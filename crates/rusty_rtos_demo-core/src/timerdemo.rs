//! `TimerDemo` — software timers, checked against the tick they fire on.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/TimerDemo.c`, and it is
//! the most demanding scenario in the corpus because almost everything it
//! asserts is about *when*. Twenty-one auto-reload timers with periods one
//! base period apart; a one-shot; and two more that only the tick
//! interrupt ever touches.
//!
//! Six tests run in a loop. They check that timers created before the
//! scheduler started are already running, that each fires at its own rate
//! over a fixed window, that stopping one really stops it, that a one-shot
//! fires once and then reports itself inactive, and — the sharp one — that
//! resetting a timer repeatedly, each time before it is due, keeps it from
//! ever firing.
//!
//! Test 1 is the other sharp one, and it happens before the scheduler
//! runs: it starts exactly `configTIMER_QUEUE_LENGTH` timers, filling the
//! command queue, and then requires the next start to **fail**. Nothing is
//! draining that queue yet, because the daemon is a task and no task is
//! running.
//!
//! Everything the timers count lives in the interrupt half's state rather
//! than the scenario's, because a timer callback runs on the daemon and
//! reaches the kernel, not the runner. The tasks read and write it through
//! the kernel's hook, the way `IntSemTest` reads its two permission flags.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::TimerHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `xBasePeriod`, as `oracle/harness/main.c` passes it.
pub const BASE_PERIOD: u64 = 50;
/// `configTIMER_QUEUE_LENGTH`.
pub const TIMER_QUEUE_LENGTH: usize = 20;
/// `tmrdemoONE_SHOT_TIMER_PERIOD`.
pub const ONE_SHOT_PERIOD: u64 = BASE_PERIOD * 3;
/// `tmrdemoNUM_TIMER_RESETS`.
pub const NUM_TIMER_RESETS: u8 = 10;
/// `tmrdemoDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;
/// `configMAX_PRIORITIES - 1`.
pub const TOP_PRIORITY: u8 = 6;
/// The priority the C creates the test task at: `configTIMER_TASK_PRIORITY - 1`.
pub const PRIORITY: u8 = 5;
/// One more than the queue length: the last auto-reload timer is created
/// but never successfully started.
const AUTO_RELOAD_TIMERS: usize = TIMER_QUEUE_LENGTH + 1;

/// Which callback a timer runs, as the hook switches on it.
const CB_AUTO_RELOAD: u16 = 0;
const CB_ONE_SHOT: u16 = 1;
const CB_ISR_AUTO_RELOAD: u16 = 2;
const CB_ISR_ONE_SHOT: u16 = 3;

/// `xMargin` in `vTimerPeriodicISRTests`, for a build whose timer task is
/// at the top priority and is not Windows.
const MARGIN: u64 = 4;

/// `TimerDemo.c`'s file-scope variables — the half only tasks touch.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `ulLastLoopCounter`, a static inside the check function.
    pub last_loop_counter: u32,
    /// `xIterationsWithoutCounterIncrement`, likewise.
    pub iterations_without_increment: u64,
    /// `xLastCycleFrequency`, likewise.
    pub last_cycle_frequency: u64,
}

impl State {
    /// `xAreTimerDemoTasksStillRunning`.
    ///
    /// It does not demand progress on every call: these tests block for up
    /// to the whole command queue's worth of base periods, so it allows
    /// that many check cycles to pass without the loop counter moving.
    pub fn still_running(&mut self, isr: Isr, cycle_frequency: u64) -> bool {
        let mut status = isr.test_status;
        if self.last_cycle_frequency != cycle_frequency {
            self.iterations_without_increment = 0;
            self.last_cycle_frequency = cycle_frequency;
        }
        if self.last_loop_counter == isr.loop_counter {
            let max_block = (TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD);
            let allowed = max_block
                .checked_div(cycle_frequency)
                .unwrap_or(0)
                .saturating_add(1);
            self.iterations_without_increment = self.iterations_without_increment.saturating_add(1);
            if self.iterations_without_increment > allowed {
                status = false;
            }
        } else {
            self.iterations_without_increment = 0;
        }
        self.last_loop_counter = isr.loop_counter;
        status
    }
}

/// Everything a timer callback touches, plus `vTimerPeriodicISRTests`.
///
/// It lives here rather than in the scenario's statics because a callback
/// runs on the daemon task, through the kernel's hook, and a hook cannot
/// reach the runner. The test task reaches *it* instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `xTestStatus`.
    pub test_status: bool,
    /// `ulLoopCounter`.
    pub loop_counter: u32,
    /// `ucAutoReloadTimerCounters`.
    pub auto_counters: [u8; AUTO_RELOAD_TIMERS],
    /// `ucOneShotTimerCounter`.
    pub one_shot_counter: u8,
    /// `ucISRAutoReloadTimerCounter`.
    pub isr_auto_counter: u8,
    /// `ucISROneShotTimerCounter`.
    pub isr_one_shot_counter: u8,
    /// `uxCallCount`, the static inside the one-shot callback.
    pub one_shot_calls: u64,
    /// `ucIsStopNeededInTimerZeroCallback`.
    pub stop_needed_in_timer_zero: bool,
    /// `uxTick`, the static that drives the periodic ISR tests. It starts
    /// at `( TickType_t ) -1` and the first thing the function does is
    /// increment it, so the first pass sees zero.
    pub tick: u64,
    /// `xAutoReloadTimers`.
    pub auto_timers: [TimerHandle; AUTO_RELOAD_TIMERS],
    /// `xOneShotTimer`.
    pub one_shot: TimerHandle,
    /// `xISRAutoReloadTimer`.
    pub isr_auto: TimerHandle,
    /// `xISROneShotTimer`.
    pub isr_one_shot: TimerHandle,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            test_status: true,
            loop_counter: 0,
            auto_counters: [0; AUTO_RELOAD_TIMERS],
            one_shot_counter: 0,
            isr_auto_counter: 0,
            isr_one_shot_counter: 0,
            one_shot_calls: 0,
            stop_needed_in_timer_zero: false,
            tick: u64::MAX,
            auto_timers: [TimerHandle::NULL; AUTO_RELOAD_TIMERS],
            one_shot: TimerHandle::NULL,
            isr_auto: TimerHandle::NULL,
            isr_one_shot: TimerHandle::NULL,
        }
    }
}

/// What one checkpoint of `vTimerPeriodicISRTests` does beyond checking
/// the two ISR counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IsrAction {
    /// Only the counter check.
    Check,
    /// `xTimerStartFromISR( xISROneShotTimer, NULL )`.
    StartOneShot,
    /// `xTimerStopFromISR( xISRAutoReloadTimer, NULL )`.
    StopAutoReload,
    /// `xTimerResetFromISR( xISROneShotTimer, NULL )`.
    ResetOneShot,
    /// The last one, which rolls `uxTick` round so the whole sequence runs
    /// again.
    Restart,
}

/// One row of [`CHECKPOINTS`]: the `uxTick` the arm fires on, what the two
/// ISR counters must be there — `None` where the C checks neither — and
/// what else the arm does.
type Checkpoint = (u64, Option<(u8, u8)>, IsrAction);

/// The C's `else if` chain, one row per arm.
///
/// It is a table rather than a chain because every arm's failure branch is
/// the same one line, and the ticks are all distinct, so the chain was only
/// ever a lookup.
const CHECKPOINTS: [Checkpoint; 15] = [
    (BASE_PERIOD - MARGIN, Some((0, 0)), IsrAction::Check),
    (BASE_PERIOD + MARGIN, Some((1, 1)), IsrAction::Check),
    (2 * BASE_PERIOD - MARGIN, Some((1, 1)), IsrAction::Check),
    (2 * BASE_PERIOD + MARGIN, Some((2, 1)), IsrAction::Check),
    (
        2 * BASE_PERIOD + (BASE_PERIOD >> 2),
        Some((2, 1)),
        IsrAction::Check,
    ),
    (3 * BASE_PERIOD, None, IsrAction::StartOneShot),
    (
        3 * BASE_PERIOD + MARGIN,
        Some((3, 1)),
        IsrAction::StopAutoReload,
    ),
    (4 * (BASE_PERIOD - MARGIN), Some((3, 1)), IsrAction::Check),
    (4 * BASE_PERIOD + MARGIN, Some((3, 2)), IsrAction::Check),
    (8 * BASE_PERIOD, Some((3, 2)), IsrAction::ResetOneShot),
    (
        9 * BASE_PERIOD - MARGIN,
        Some((3, 2)),
        IsrAction::ResetOneShot,
    ),
    (
        10 * BASE_PERIOD - 2 * MARGIN,
        Some((3, 2)),
        IsrAction::ResetOneShot,
    ),
    (
        11 * BASE_PERIOD - 3 * MARGIN,
        Some((3, 2)),
        IsrAction::ResetOneShot,
    ),
    (
        12 * BASE_PERIOD - 2 * MARGIN,
        Some((3, 3)),
        IsrAction::Check,
    ),
    (15 * BASE_PERIOD, Some((3, 3)), IsrAction::Restart),
];

impl Isr {
    /// `vTimerPeriodicISRTests`, called from the tick hook.
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        self.tick = self.tick.wrapping_add(1);
        let t = self.tick;

        // `if( uxTick == 0 )`: the sequence starts by giving the two timers
        // the interrupt owns a real period. Either both commands reach the
        // daemon and the clock starts, or `uxTick` stays at -1 and the
        // whole thing is tried again on the next tick.
        if t == 0 {
            self.isr_auto_counter = 0;
            self.isr_one_shot_counter = 0;
            self.tick = u64::MAX;
            if matches!(
                k.timer_change_period_from_isr(self.isr_auto, BASE_PERIOD),
                Ok((true, _))
            ) {
                if matches!(
                    k.timer_change_period_from_isr(self.isr_one_shot, BASE_PERIOD),
                    Ok((true, _))
                ) {
                    self.tick = 0;
                } else {
                    let _ = k.timer_stop_from_isr(self.isr_auto);
                }
            }
            return self;
        }

        let Some(&(_, expected, action)) = CHECKPOINTS.iter().find(|row| row.0 == t) else {
            return self;
        };
        if let Some((auto, one_shot)) = expected {
            if self.isr_auto_counter != auto || self.isr_one_shot_counter != one_shot {
                self.test_status = false;
            }
        }
        match action {
            IsrAction::Check => {}
            IsrAction::StartOneShot => {
                let _ = k.timer_start_from_isr(self.isr_one_shot);
            }
            IsrAction::StopAutoReload => {
                let _ = k.timer_stop_from_isr(self.isr_auto);
            }
            IsrAction::ResetOneShot => {
                let _ = k.timer_reset_from_isr(self.isr_one_shot);
            }
            IsrAction::Restart => self.tick = u64::MAX,
        }
        self
    }
}

/// The four `TimerCallbackFunction_t`s, which the daemon task runs.
///
/// Every one of these lines is written the way the C's callbacks read their
/// `static`s: a kernel call, then a short read-modify-write of the hook,
/// never a copy of the hook held across a kernel call. The tick hook runs
/// inside the critical sections these calls take — `pvTimerGetTimerID` is
/// one — and a copy held across one would be stored back over whatever the
/// tick hook had just written to itself.
pub(crate) fn timer_callback<W: fmt::Write>(
    k: &mut SimKernel<W>,
    timer: TimerHandle,
    callback: u16,
) {
    match callback {
        // prvAutoReloadTimerCallback
        CB_AUTO_RELOAD => {
            let id = k.timer_id(timer).unwrap_or(u64::MAX) as usize;
            if id <= TIMER_QUEUE_LENGTH + 1 {
                let stop = update(k, |isr| {
                    if let Some(slot) = isr.auto_counters.get_mut(id) {
                        *slot = slot.wrapping_add(1);
                    }
                    id == 0 && isr.stop_needed_in_timer_zero
                });
                if stop {
                    let _ = k.timer_stop(timer, DONT_BLOCK);
                    update(k, |isr| isr.stop_needed_in_timer_zero = false);
                }
            } else {
                fail(k);
            }
        }
        // prvOneShotTimerCallback: the timer's own id counts its calls,
        // and must agree with the callback's private count.
        CB_ONE_SHOT => {
            let last = k.timer_id(timer).unwrap_or(0);
            if update(k, |isr| last != isr.one_shot_calls) {
                fail(k);
            }
            let _ = k.timer_set_id(timer, last.wrapping_add(1));
            update(k, |isr| {
                isr.one_shot_calls = isr.one_shot_calls.wrapping_add(1);
                isr.one_shot_counter = isr.one_shot_counter.wrapping_add(1);
            });
        }
        CB_ISR_AUTO_RELOAD => {
            update(k, |isr| {
                isr.isr_auto_counter = isr.isr_auto_counter.wrapping_add(1);
            });
        }
        CB_ISR_ONE_SHOT => {
            update(k, |isr| {
                isr.isr_one_shot_counter = isr.isr_one_shot_counter.wrapping_add(1);
            });
        }
        _ => {}
    }
}

/// One of `xAutoReloadTimers`, by index. Out of range answers the null
/// handle, which every kernel call refuses — the C would have indexed past
/// the array.
fn auto_timer<W: fmt::Write>(k: &mut SimKernel<W>, index: usize) -> TimerHandle {
    update(k, |isr| {
        isr.auto_timers
            .get(index)
            .copied()
            .unwrap_or(TimerHandle::NULL)
    })
}

/// One of `ucAutoReloadTimerCounters`, by index.
fn auto_counter<W: fmt::Write>(k: &mut SimKernel<W>, index: usize) -> u8 {
    update(k, |isr| isr.auto_counters.get(index).copied().unwrap_or(0))
}

/// A task's way of reading and writing the counters the callbacks own.
fn isr_of<W: fmt::Write>(k: &mut SimKernel<W>) -> Isr {
    match k.tick_hook() {
        TickIsr::TimerDemo(isr) => *isr,
        _ => Isr::default(),
    }
}

/// One read-modify-write of the counters, with no kernel call inside it —
/// which is what makes it safe against the tick hook running in the middle.
fn update<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> R {
    match k.tick_hook_mut() {
        TickIsr::TimerDemo(isr) => f(isr),
        _ => f(&mut Isr::default()),
    }
}

fn set_isr<W: fmt::Write>(k: &mut SimKernel<W>, value: Isr) {
    if let TickIsr::TimerDemo(isr) = k.tick_hook_mut() {
        *isr = value;
    }
}

fn fail<W: fmt::Write>(k: &mut SimKernel<W>) {
    if let TickIsr::TimerDemo(isr) = k.tick_hook_mut() {
        isr.test_status = false;
    }
}

fn bump_loop<W: fmt::Write>(k: &mut SimKernel<W>) {
    if let TickIsr::TimerDemo(isr) = k.tick_hook_mut() {
        if isr.test_status {
            isr.loop_counter = isr.loop_counter.wrapping_add(1);
        }
    }
}

/// `prvTimerTestTask`, with the six tests flattened into one program
/// counter. The `pc` ranges name the C function each arm belongs to.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// The loop variable every one of the tests uses.
    timer: usize,
    /// `uxOriginalPriority` in test 3.
    original_priority: u8,
    /// The reset count in test 6.
    resets: u8,
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, _s: &mut Shared) -> Step {
        match self.pc {
            // xOneShotTimer = xTimerCreate( "Oneshot Timer", ... );
            0 => {
                match k.timer_create("Oneshot Timer", ONE_SHOT_PERIOD, false, 0, CB_ONE_SHOT) {
                    Ok(t) => {
                        let mut isr = isr_of(k);
                        isr.one_shot = t;
                        set_isr(k, isr);
                    }
                    Err(_) => fail(k),
                }
                self.pc = 1;
            }
            // vTimerSetReloadMode( xOneShotTimer, pdTRUE );
            1 => {
                let one_shot = isr_of(k).one_shot;
                let _ = k.timer_set_auto_reload(one_shot, true);
                self.pc = 5;
            }
            // configASSERT( uxTimerGetReloadMode( xOneShotTimer ) == pdTRUE );
            //
            // The harness compiles `configASSERT` in, so the query is a real
            // call with a real critical section, and skipping it would give
            // the task one exit less than the C spends here.
            5 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_auto_reload(one_shot) != Ok(true) {
                    fail(k);
                }
                self.pc = 2;
            }
            // vTimerSetReloadMode( xOneShotTimer, pdFALSE );
            2 => {
                let one_shot = isr_of(k).one_shot;
                let _ = k.timer_set_auto_reload(one_shot, false);
                self.pc = 6;
            }
            // configASSERT( uxTimerGetReloadMode( xOneShotTimer ) == pdFALSE );
            6 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_auto_reload(one_shot) != Ok(false) {
                    fail(k);
                }
                self.timer = 0;
                self.pc = 3;
            }

            // -------------------- prvTest2_CheckTaskAndTimersInitialState --
            3 => {
                if self.timer < TIMER_QUEUE_LENGTH {
                    let t = auto_timer(k, self.timer);
                    if k.timer_is_active(t) != Ok(true) {
                        fail(k);
                    }
                    self.timer = self.timer.saturating_add(1);
                } else {
                    self.pc = 4;
                }
            }
            4 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH];
                if k.timer_is_active(t) != Ok(false) {
                    fail(k);
                }
                self.pc = 10;
            }

            // ------------------------ prvTest3_CheckAutoReloadExpireRates --
            10 => {
                self.original_priority = k.task_priority_get(None).unwrap_or(0);
                self.pc = 11;
            }
            11 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 12;
            }
            12 => {
                let _ = k.delay((TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD));
                self.timer = 0;
                self.pc = 13;
            }
            13 => {
                if self.timer >= TIMER_QUEUE_LENGTH {
                    self.pc = 14;
                } else {
                    let block = (TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD);
                    let period =
                        ((self.timer as u64).saturating_add(1)).saturating_mul(BASE_PERIOD);
                    let expected = block.checked_div(period).unwrap_or(0) as u8;
                    // The C's `(uint8_t)expected - (uint8_t)1` wraps when
                    // expected is zero, which is deliberate: the check then
                    // cannot fail low.
                    let min = expected.wrapping_sub(1);
                    let count = auto_counter(k, self.timer);
                    if count < min || count > expected {
                        fail(k);
                    }
                    self.timer = self.timer.saturating_add(1);
                }
            }
            14 => {
                let _ = k.set_priority(None, self.original_priority);
                self.pc = 15;
            }
            15 => {
                bump_loop(k);
                self.timer = 0;
                self.pc = 20;
            }

            // ------------------ prvTest4_CheckAutoReloadTimersCanBeStopped --
            20 => {
                if self.timer >= TIMER_QUEUE_LENGTH {
                    self.pc = 24;
                } else {
                    let t = auto_timer(k, self.timer);
                    if k.timer_is_active(t) != Ok(true) {
                        fail(k);
                    }
                    self.pc = 21;
                }
            }
            21 => {
                let t = auto_timer(k, self.timer);
                let _ = k.timer_stop(t, DONT_BLOCK);
                self.pc = 22;
            }
            22 => {
                let t = auto_timer(k, self.timer);
                if k.timer_is_active(t) != Ok(false) {
                    fail(k);
                }
                self.timer = self.timer.saturating_add(1);
                self.pc = 20;
            }
            // taskENTER_CRITICAL(); the last counter must be untouched, and
            // all of them are cleared; taskEXIT_CRITICAL().
            24 => {
                k.enter_critical();
                let mut isr = isr_of(k);
                if isr.auto_counters[TIMER_QUEUE_LENGTH] != 0 {
                    isr.test_status = false;
                }
                isr.auto_counters = [0; AUTO_RELOAD_TIMERS];
                set_isr(k, isr);
                k.exit_critical();
                self.pc = 25;
            }
            25 => {
                let _ = k.delay((TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD));
                self.timer = 0;
                self.pc = 26;
            }
            26 => {
                if self.timer >= TIMER_QUEUE_LENGTH {
                    bump_loop(k);
                    self.pc = 30;
                } else {
                    if auto_counter(k, self.timer) != 0 {
                        fail(k);
                    }
                    self.timer = self.timer.saturating_add(1);
                }
            }

            // ------------------ prvTest5_CheckBasicOneShotTimerBehaviour --
            30 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) != Ok(false) {
                    fail(k);
                }
                self.pc = 31;
            }
            31 => {
                if isr_of(k).one_shot_counter != 0 {
                    fail(k);
                }
                self.pc = 32;
            }
            32 => {
                let one_shot = isr_of(k).one_shot;
                let _ = k.timer_start(one_shot, DONT_BLOCK);
                self.pc = 33;
            }
            33 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) != Ok(true) {
                    fail(k);
                }
                self.pc = 34;
            }
            34 => {
                let _ = k.delay(ONE_SHOT_PERIOD.saturating_mul(3));
                self.pc = 35;
            }
            35 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) != Ok(false) {
                    fail(k);
                }
                self.pc = 36;
            }
            36 => {
                let mut isr = isr_of(k);
                if isr.one_shot_counter != 1 {
                    isr.test_status = false;
                } else {
                    isr.one_shot_counter = 0;
                }
                set_isr(k, isr);
                self.pc = 37;
            }
            37 => {
                bump_loop(k);
                self.pc = 40;
            }

            // ----------------------- prvTest6_CheckAutoReloadResetBehaviour --
            40 => {
                let one_shot = isr_of(k).one_shot;
                let _ = k.timer_start(one_shot, DONT_BLOCK);
                self.pc = 41;
            }
            41 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) != Ok(true) {
                    fail(k);
                }
                self.pc = 42;
            }
            42 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                let _ = k.timer_start(t, DONT_BLOCK);
                self.pc = 43;
            }
            43 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                if k.timer_is_active(t) != Ok(true) {
                    fail(k);
                }
                self.resets = 0;
                self.pc = 44;
            }
            // The reset loop: each pass waits half the one-shot period, so
            // neither timer should ever reach its own.
            44 => {
                if self.resets >= NUM_TIMER_RESETS {
                    self.pc = 51;
                } else {
                    let _ = k.delay(ONE_SHOT_PERIOD / 2);
                    self.pc = 45;
                }
            }
            45 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) != Ok(true) {
                    fail(k);
                }
                self.pc = 46;
            }
            46 => {
                if isr_of(k).one_shot_counter != 0 {
                    fail(k);
                }
                self.pc = 47;
            }
            47 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                if k.timer_is_active(t) != Ok(true) {
                    fail(k);
                }
                self.pc = 48;
            }
            48 => {
                if isr_of(k).auto_counters[TIMER_QUEUE_LENGTH - 1] != 0 {
                    fail(k);
                }
                self.pc = 49;
            }
            49 => {
                let one_shot = isr_of(k).one_shot;
                let _ = k.timer_reset(one_shot, DONT_BLOCK);
                self.pc = 50;
            }
            50 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                let _ = k.timer_reset(t, DONT_BLOCK);
                bump_loop(k);
                self.resets = self.resets.saturating_add(1);
                self.pc = 44;
            }
            // Then let them both run out.
            51 => {
                let _ = k.delay((TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD));
                self.pc = 52;
            }
            52 => {
                if isr_of(k).one_shot_counter != 1 {
                    fail(k);
                }
                self.pc = 53;
            }
            53 => {
                if isr_of(k).auto_counters[TIMER_QUEUE_LENGTH - 1] == 0 {
                    fail(k);
                }
                self.pc = 54;
            }
            54 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                if k.timer_is_active(t) != Ok(true) {
                    fail(k);
                }
                self.pc = 55;
            }
            55 => {
                let one_shot = isr_of(k).one_shot;
                if k.timer_is_active(one_shot) == Ok(true) {
                    fail(k);
                }
                self.pc = 56;
            }
            56 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                let _ = k.timer_stop(t, DONT_BLOCK);
                self.pc = 57;
            }
            57 => {
                let t = isr_of(k).auto_timers[TIMER_QUEUE_LENGTH - 1];
                if k.timer_is_active(t) != Ok(false) {
                    fail(k);
                }
                self.pc = 58;
            }
            58 => {
                let mut isr = isr_of(k);
                isr.auto_counters[TIMER_QUEUE_LENGTH - 1] = 0;
                isr.one_shot_counter = 0;
                set_isr(k, isr);
                bump_loop(k);
                self.timer = 0;
                self.pc = 60;
            }

            // ------------------ prvResetStartConditionsForNextIteration --
            60 => {
                if self.timer >= TIMER_QUEUE_LENGTH {
                    bump_loop(k);
                    // Back to test 3 for the next round.
                    self.pc = 10;
                } else {
                    let t = auto_timer(k, self.timer);
                    if k.timer_is_active(t) != Ok(false) {
                        fail(k);
                    }
                    self.pc = 61;
                }
            }
            61 => {
                let t = auto_timer(k, self.timer);
                let _ = k.timer_start(t, DONT_BLOCK);
                self.pc = 62;
            }
            _ => {
                let t = auto_timer(k, self.timer);
                if k.timer_is_active(t) != Ok(true) {
                    fail(k);
                }
                self.timer = self.timer.saturating_add(1);
                self.pc = 60;
            }
        }
        Step::Continue
    }
}

/// `vStartTimerDemoTask`, in the C's order.
///
/// `prvTest1_CreateTimersWithoutSchedulerRunning` runs here, before the
/// scheduler: it fills the timer command queue with starts that nothing is
/// draining, and requires the one after that to fail.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let mut isr = Isr::default();
    let task;
    {
        let mut k = runner.kernel_mut();
        for index in 0..TIMER_QUEUE_LENGTH {
            let period = ((index as u64).saturating_add(1)).saturating_mul(BASE_PERIOD);
            match k.timer_create("FR Timer", period, true, index as u64, CB_AUTO_RELOAD) {
                Ok(t) => {
                    if let Some(slot) = isr.auto_timers.get_mut(index) {
                        *slot = t;
                    }
                    // `!= pdPASS`. A command the queue refuses comes back
                    // as `Ready(false)`, which is a call that worked and an
                    // answer of `pdFAIL` — not an `Err`.
                    if !matches!(
                        k.timer_start(t, SimKernel::<W>::MAX_DELAY),
                        Ok(Wait::Ready(true))
                    ) {
                        isr.test_status = false;
                    }
                }
                Err(_) => isr.test_status = false,
            }
        }
        // The one past the end. The C passes the *loop variable* as its id,
        // which by now is `configTIMER_QUEUE_LENGTH`.
        let period = (TIMER_QUEUE_LENGTH as u64).saturating_mul(BASE_PERIOD);
        match k.timer_create(
            "FR Timer",
            period,
            true,
            TIMER_QUEUE_LENGTH as u64,
            CB_AUTO_RELOAD,
        ) {
            Ok(t) => {
                if let Some(slot) = isr.auto_timers.get_mut(TIMER_QUEUE_LENGTH) {
                    *slot = t;
                }
                // This start must FAIL: the command queue is full and no
                // task is running to drain it.
                if matches!(
                    k.timer_start(t, SimKernel::<W>::MAX_DELAY),
                    Ok(Wait::Ready(true))
                ) {
                    isr.test_status = false;
                }
            }
            Err(_) => isr.test_status = false,
        }
        // The two the tick interrupt owns. A period of zero is invalid, so
        // they get a placeholder until the interrupt sets a real one.
        isr.isr_auto = k.timer_create("ISR AR", 0xffff, true, 0, CB_ISR_AUTO_RELOAD)?;
        isr.isr_one_shot = k.timer_create("ISR OS", 0xffff, false, 0, CB_ISR_ONE_SHOT)?;
        task = k.create_task("Tmr Tst", PRIORITY)?;
    }
    runner.shared_mut().state = runner::State::TimerDemo(State::default());
    *runner.kernel_mut().tick_hook_mut() = TickIsr::TimerDemo(isr);
    runner.start_common(max_ticks)?;
    runner.attach(task, runner::Body::TimerDemo(Body::default()));
    Ok(())
}
