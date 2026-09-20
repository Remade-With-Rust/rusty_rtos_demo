//! `TaskNotify` — every notification method, checked against itself.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/TaskNotify.c`, and it is
//! the only demo in the corpus that exercises direct-to-task notification
//! at all. One task, `Notified`, first runs `prvSingleTaskTests` — some
//! three hundred lines of notifying *itself* and checking what comes back
//! — and then loops for ever, notified by a software timer and by the tick
//! interrupt at the same time.
//!
//! Notifying yourself is not a normal thing to do, and the C says so twice.
//! It is done because it makes the whole notification state machine
//! reachable from ONE task with no scheduling to arrange: set, set-bits,
//! increment, overwrite, don't-overwrite, clear-on-entry, clear-on-exit,
//! state-clear and value-clear are all observable in a straight line.
//!
//! Three things in here exist nowhere else in the corpus:
//!
//! * `xTaskNotifyAndQuery` — a notify that also answers the value it is
//!   about to replace, from inside the same critical section. That is
//!   `Kernel::notify_and_query`, and why it is one call rather than a read
//!   followed by a notify is in its own documentation.
//! * `vTaskSuspend` on a task that is *blocked on a notification*, driven
//!   from a timer callback, and a notification delivered while it is
//!   suspended. The kernel's suspend path carries a comment naming
//!   `TaskNotify.c:498` because this is the test that finds it.
//! * `xTimerChangePeriod` and `xTimerDelete` driven from a task, which
//!   `TimerDemo` reaches only behind its own harness flag.
//!
//! The pseudo-random timer periods are pinned: upstream seeds `uxNextRand`
//! from the ADDRESS of `prvRand`, which ASLR moves every run, so the C demo
//! does not reproduce itself and cannot be diffed against anything.
//! `kairos oracle patch` replaces the seed with a constant, and [`SEED`] is
//! that constant.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{TaskHandle, TimerHandle};
use rusty_rtos_kernel::kernel::{NotifyAction, TaskState};
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// The notification slot every one of these calls uses:
/// `tskDEFAULT_INDEX_TO_NOTIFY`.
const IDX: usize = 0;

/// `notifyTASK_PRIORITY`, which is `tskIDLE_PRIORITY`.
pub const PRIORITY: u8 = 0;
/// `configMAX_PRIORITIES - 1`, the priority the task raises itself to.
const TOP_PRIORITY: u8 = 6;

/// `notifyUINT32_HIGH_BYTE`.
const HIGH_BYTE: u32 = 0xff00_0000;
/// `notifyUINT32_LOW_BYTE`.
const LOW_BYTE: u32 = 0x0000_00ff;

/// `xTicksToWait`: `pdMS_TO_TICKS( 100UL )` at this demo's 1 kHz tick.
const TICKS_TO_WAIT: u64 = 100;
/// `notifySUSPENDED_TEST_TIMER_PERIOD`.
const SUSPENDED_TEST_PERIOD: u64 = 50;
/// `xMaxPeriod` in `prvNotifiedTask`.
const MAX_PERIOD: u32 = 90;
/// `xMinPeriod`.
const MIN_PERIOD: u32 = 10;
/// `xDontBlock`.
const DONT_BLOCK: u64 = 0;
/// `ulCyclesToRaisePriority`.
const CYCLES_TO_RAISE_PRIORITY: u32 = 50;

/// `ulFirstNotifiedConst`.
const FIRST: u32 = 100_001;
/// `ulSecondNotifiedValueConst`.
const SECOND: u32 = 5_555;
/// `ulMaxLoops`.
const MAX_LOOPS: u32 = 5;
/// `ulBit0`.
const BIT0: u32 = 0x01;
/// `ulBit1`.
const BIT1: u32 = 0x02;

/// `notifyUINT32_MAX & ~notifyUINT32_HIGH_BYTE`, and the same with the low
/// byte cleared too. Both are written the C's way: masking with
/// `notifyUINT32_MAX` is an identity over a `u32`, and keeping it is what
/// makes the line diffable against `TaskNotify.c:413`.
#[allow(clippy::identity_op)]
const ALL_BUT_HIGH: u32 = u32::MAX & !HIGH_BYTE;
#[allow(clippy::identity_op)]
const ALL_BUT_HIGH_AND_LOW: u32 = u32::MAX & !HIGH_BYTE & !LOW_BYTE;

/// `xCallInterval` in `xNotifyTaskFromISR`: `pdMS_TO_TICKS( 50 )`.
const CALL_INTERVAL: i32 = 50;
/// `ulMaxSendReceiveDeviation`.
const MAX_SEND_RECEIVE_DEVIATION: u32 = 5;

/// `prvSuspendedTaskTimerTestCallback`.
const CB_SUSPENDED_TEST: u16 = 0;
/// `prvNotifyingTimer`.
const CB_NOTIFYING: u16 = 1;

/// The value `kairos oracle patch` pins `uxNextRand` to, in place of the
/// address upstream seeds it from. Changing it here without changing the
/// patch makes every timer period disagree from the first pass of the loop.
pub const SEED: u32 = 0x0dc0_ffee;

/// `uxMultiplier` in `prvRand`.
const RAND_MULTIPLIER: u32 = 0x015a_4e35;

/// `TaskNotify.c`'s file-scope variables — the half only tasks touch.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `xTaskToNotify`. The interrupt half keeps its own copy of the same
    /// variable, because a timer callback is handed the kernel and nothing
    /// else; both are written once, at creation, and never again.
    pub task: TaskHandle,
    /// `xErrorStatus`.
    pub error_status: bool,
    /// `ulNotifyCycleCount`.
    pub cycles: u32,
    /// `ulTimerNotificationsReceived`.
    pub received: u32,
    /// `uxNextRand`.
    pub next_rand: u32,
    /// `ulLastNotifyCycleCount`, a static inside the check function.
    pub last_cycles: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            task: TaskHandle::NULL,
            error_status: true,
            cycles: 0,
            received: 0,
            next_rand: 0,
            last_cycles: 0,
        }
    }
}

impl State {
    /// `prvRand`.
    fn rand(&mut self) -> u32 {
        self.next_rand = self.next_rand.wrapping_mul(RAND_MULTIPLIER).wrapping_add(1);
        (self.next_rand >> 16) & 0x7fff
    }

    /// The two lines both halves of the main loop compute together:
    /// `prvRand() % xMaxPeriod`, floored at `xMinPeriod`.
    fn period(&mut self) -> u64 {
        u64::from((self.rand() % MAX_PERIOD).max(MIN_PERIOD))
    }

    /// `xAreTaskNotificationTasksStillRunning`.
    ///
    /// The count of gives belongs to the interrupt half and the count of
    /// takes to this one, so the check needs both.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        if self.last_cycles == self.cycles {
            self.error_status = false;
        } else {
            self.last_cycles = self.cycles;
        }
        if isr.sent > self.received
            && isr.sent.wrapping_sub(self.received) > MAX_SEND_RECEIVE_DEVIATION
        {
            self.error_status = false;
        }
        self.error_status && isr.test_status
    }
}

/// `xNotifyTaskFromISR`, plus the file-scope state the two timer callbacks
/// reach — a callback is handed the kernel and nothing else, and the tick
/// hook is the only thing of the scenario's that lives inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `xTaskToNotify`.
    task: TaskHandle,
    /// `xTimer`. Null until the task creates it, which is what tells the
    /// interrupt that the single-task tests have finished.
    timer: TimerHandle,
    /// `ulTimerNotificationsSent`, incremented from the interrupt AND from
    /// `prvNotifyingTimer`.
    sent: u32,
    /// `xCallCount`, a static in `xNotifyTaskFromISR`.
    call_count: i32,
    /// `xAPIToUse`, likewise: which of the three ISR notify calls is next.
    api_to_use: i32,
    /// `ulCallCount`, a static in `prvSuspendedTaskTimerTestCallback`.
    suspend_calls: u32,
    /// Whether a `configASSERT` inside a timer callback has held. It lives
    /// here rather than in [`State`] for the same reason everything else
    /// in this struct does: a callback cannot reach `State`.
    test_status: bool,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            task: TaskHandle::NULL,
            timer: TimerHandle::NULL,
            sent: 0,
            call_count: 0,
            api_to_use: 0,
            suspend_calls: 0,
            test_status: true,
        }
    }
}

impl Isr {
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        // The task runs its own tests before creating the timer that gives
        // the notification from here, so until that timer exists this does
        // nothing at all.
        if self.timer == TimerHandle::NULL {
            return self;
        }
        self.call_count = self.call_count.wrapping_add(1);
        if self.call_count < CALL_INTERVAL {
            return self;
        }
        self.call_count = 0;
        // All three are an `eIncrement` of zero; what differs is how much
        // of the answer the C API hands back. None of them traces --
        // `traceTASK_NOTIFY_FROM_ISR` is not hooked -- so what this
        // exercises is the kernel path, not the line.
        match self.api_to_use {
            // vTaskNotifyGiveFromISR( xTaskToNotify, NULL );
            0 => {
                let _ = k.notify_from_isr(self.task, IDX, 0, NotifyAction::Increment);
                self.api_to_use = 1;
            }
            // xTaskNotifyFromISR( xTaskToNotify, 0, eIncrement, NULL );
            1 => {
                let _ = k.notify_from_isr(self.task, IDX, 0, NotifyAction::Increment);
                self.api_to_use = 2;
            }
            // xTaskNotifyAndQueryFromISR( ..., &ulPreviousValue, NULL ), whose
            // answer the C asserts is not the 0xff it seeded the out-parameter
            // with -- that is, that the call wrote it at all.
            _ => {
                let _ = k.notify_and_query_from_isr(self.task, IDX, 0, NotifyAction::Increment);
                self.api_to_use = 0;
            }
        }
        self.sent = self.sent.wrapping_add(1);
        self
    }
}

/// Read or change the scenario's interrupt-half state from a timer
/// callback, which has only the kernel to reach it through.
fn with_isr<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> R {
    match k.tick_hook_mut() {
        TickIsr::TaskNotify(isr) => f(isr),
        _ => f(&mut Isr::default()),
    }
}

/// The two timer callbacks, dispatched from [`crate::runner::TickIsr`].
pub(crate) fn timer_callback<W: fmt::Write>(k: &mut SimKernel<W>, callback: u16) {
    match callback {
        // prvSuspendedTaskTimerTestCallback: the first call suspends and
        // resumes without ever notifying, the second notifies the task
        // while it is suspended.
        CB_SUSPENDED_TEST => {
            let (task, calls) = with_isr(k, |isr| (isr.task, isr.suspend_calls));
            let _ = k.suspend(Some(task));
            if calls != 0 {
                // `ulCallCount` is used only as a convenient non-zero value.
                let _ = k.notify(task, IDX, calls, NotifyAction::Overwrite);
            }
            // configASSERT( eTaskGetState( xTaskToNotify ) == eSuspended ).
            // A real call, and one that takes a critical section of its
            // own, so it is not an assertion that can be dropped: on the
            // sim an exit is when the clock can move.
            if k.task_state_get(task) != Ok(TaskState::Suspended) {
                fail(k);
            }
            let _ = k.resume(task);
            with_isr(k, |isr| {
                isr.suspend_calls = isr.suspend_calls.wrapping_add(1)
            });
        }
        // prvNotifyingTimer.
        _ => {
            let task = with_isr(k, |isr| isr.task);
            // xTaskNotifyGive( xTaskToNotify ).
            let _ = k.notify(task, IDX, 0, NotifyAction::Increment);
            // This value is also incremented from an interrupt, so the C
            // takes a critical section around it -- which costs an exit.
            k.enter_critical();
            with_isr(k, |isr| isr.sent = isr.sent.wrapping_add(1));
            k.exit_critical();
        }
    }
}

/// Record a `configASSERT` that did not hold inside a timer callback.
fn fail<W: fmt::Write>(k: &mut SimKernel<W>) {
    with_isr(k, |isr| isr.test_status = false);
}

/// `prvNotifiedTask`: `prvSingleTaskTests()`, then the main loop.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// `xReturned`.
    returned: bool,
    /// `ulNotifiedValue`.
    notified_value: u32,
    /// `ulPreviousValue`.
    previous_value: u32,
    /// `ulExpectedValue`.
    expected_value: u32,
    /// `ulNotifyingValue`.
    notifying_value: u32,
    /// `ulLoop`.
    loop_i: u32,
    /// `xTimeOnEntering`.
    time_on_entering: u64,
    /// `xSingleTaskTimer`.
    single_task_timer: TimerHandle,
    /// `xPeriod`, which the main loop computes once and then uses twice.
    period: u64,
}

/// The first `pc` of `prvNotifiedTask`'s `for( ;; )`.
const LOOP_BASE: u8 = 100;

impl Body {
    #[inline(never)]
    #[allow(clippy::too_many_lines)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::TaskNotify(s) = &mut s.state else {
            return Step::Finish(false);
        };
        let task = s.task;
        let max = SimKernel::<W>::MAX_DELAY;
        match self.pc {
            // ---- prvSingleTaskTests: blocking with no notification ----
            // xTimeOnEntering = xTaskGetTickCount();
            0 => {
                self.time_on_entering = k.tick_count();
                self.pc = 1;
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, xTicksToWait );
            1 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, TICKS_TO_WAIT)
                {
                    self.returned = ok;
                    self.notified_value = value;
                    self.pc = 2;
                }
            }
            // if( ( xTaskGetTickCount() - xTimeOnEntering ) < xTicksToWait ) xErrorStatus = pdFAIL;
            2 => {
                if k.tick_count().wrapping_sub(self.time_on_entering) < TICKS_TO_WAIT {
                    s.error_status = false;
                }
                self.pc = 3;
            }
            // ---- no blocking when a notification is pending ----
            // xReturned = xTaskNotifyAndQuery( xTaskToNotify, ulFirstNotifiedConst,
            //                                  eSetValueWithoutOverwrite, &ulPreviousValue );
            3 => {
                if let Ok((ok, previous)) =
                    k.notify_and_query(task, IDX, FIRST, NotifyAction::NoOverwrite)
                {
                    self.returned = ok;
                    self.previous_value = previous;
                }
                self.pc = 4;
            }
            // xTimeOnEntering = xTaskGetTickCount();
            4 => {
                self.time_on_entering = k.tick_count();
                self.pc = 5;
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, xTicksToWait );
            5 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, TICKS_TO_WAIT)
                {
                    self.returned = ok;
                    self.notified_value = value;
                    self.pc = 6;
                }
            }
            // if( ( xTaskGetTickCount() - xTimeOnEntering ) >= xTicksToWait ) xErrorStatus = pdFAIL;
            // ulNotifyCycleCount++;
            6 => {
                if k.tick_count().wrapping_sub(self.time_on_entering) >= TICKS_TO_WAIT {
                    s.error_status = false;
                }
                s.cycles = s.cycles.wrapping_add(1);
                self.pc = 7;
            }
            // ---- the non-overwriting functionality ----
            // xReturned = xTaskNotify( xTaskToNotify, ulFirstNotifiedConst, eSetValueWithoutOverwrite );
            7 => {
                self.returned = k
                    .notify(task, IDX, FIRST, NotifyAction::NoOverwrite)
                    .unwrap_or(false);
                self.pc = 8;
            }
            // xReturned = xTaskNotify( xTaskToNotify, ulSecondNotifiedValueConst, eSetValueWithoutOverwrite );
            8 => {
                self.returned = k
                    .notify(task, IDX, SECOND, NotifyAction::NoOverwrite)
                    .unwrap_or(false);
                self.pc = 9;
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            9 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 10;
            }
            // ---- the overwriting version ----
            // xReturned = xTaskNotify( xTaskToNotify, ulFirstNotifiedConst, eSetValueWithOverwrite );
            10 => {
                self.returned = k
                    .notify(task, IDX, FIRST, NotifyAction::Overwrite)
                    .unwrap_or(false);
                self.pc = 11;
            }
            // xReturned = xTaskNotify( xTaskToNotify, ulSecondNotifiedValueConst, eSetValueWithOverwrite );
            11 => {
                self.returned = k
                    .notify(task, IDX, SECOND, NotifyAction::Overwrite)
                    .unwrap_or(false);
                self.pc = 12;
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            12 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 13;
            }
            // ---- eNoAction passes without updating the value ----
            // xReturned = xTaskNotify( xTaskToNotify, ulFirstNotifiedConst, eNoAction );
            13 => {
                self.returned = k
                    .notify(task, IDX, FIRST, NotifyAction::None)
                    .unwrap_or(false);
                self.pc = 14;
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            14 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.loop_i = 0;
                self.pc = 15;
            }
            // ---- incrementing values ----
            // for( ulLoop = 0; ulLoop < ulMaxLoops; ulLoop++ )
            //     xReturned = xTaskNotify( xTaskToNotify, 0, eIncrement );
            15 => {
                if self.loop_i < MAX_LOOPS {
                    self.returned = k
                        .notify(task, IDX, 0, NotifyAction::Increment)
                        .unwrap_or(false);
                    self.loop_i = self.loop_i.wrapping_add(1);
                } else {
                    self.pc = 16;
                }
            }
            // xReturned = xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            16 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 17;
            }
            // xReturned = xTaskNotifyWait( 0, 0, &ulNotifiedValue, 0 );  -- should fail
            17 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 18;
            }
            // ---- every bit can be set, one per pass ----
            // ulNotifyingValue = 0x01; ulLoop = 0;
            // xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            18 => {
                self.notifying_value = 0x01;
                self.loop_i = 0;
                if let Ok(Wait::Ready((_, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.notified_value = value;
                }
                self.pc = 19;
            }
            // xTaskNotify( xTaskToNotify, ulNotifyingValue, eSetBits );
            19 => {
                let _ = k.notify(task, IDX, self.notifying_value, NotifyAction::SetBits);
                self.pc = 20;
            }
            // xReturned = xTaskNotifyWait( 0, 0, &ulNotifiedValue, 0 );
            // ulLoop++; ulNotifyingValue <<= 1UL;
            // } while( ulNotifiedValue != notifyUINT32_MAX );
            20 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.loop_i = self.loop_i.wrapping_add(1);
                self.notifying_value = self.notifying_value.wrapping_shl(1);
                self.pc = if self.notified_value == u32::MAX {
                    21
                } else {
                    19
                };
            }
            // ---- cleared on entry but not on exit when nothing arrives ----
            // xReturned = xTaskNotifyWait( ulBit0, ulBit1, &ulNotifiedValue, xTicksToWait );
            21 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, BIT0, BIT1, TICKS_TO_WAIT)
                {
                    self.returned = ok;
                    self.notified_value = value;
                    self.pc = 22;
                }
            }
            // xTaskNotify( xTaskToNotify, notifyUINT32_MAX, eNoAction );
            22 => {
                let _ = k.notify(task, IDX, u32::MAX, NotifyAction::None);
                self.pc = 23;
            }
            // xReturned = xTaskNotifyWait( 0x00UL, 0x00UL, &ulNotifiedValue, 0 );
            23 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 24;
            }
            // ---- now clear the bit on exit, which needs a notification ----
            // xTaskNotify( xTaskToNotify, 0, eNoAction );
            24 => {
                let _ = k.notify(task, IDX, 0, NotifyAction::None);
                self.pc = 25;
            }
            // xTaskNotifyWait( 0x00, ulBit1, &ulNotifiedValue, 0 );
            25 => {
                if let Ok(Wait::Ready((_, value))) = k.notify_wait(IDX, 0, BIT1, 0) {
                    self.notified_value = value;
                }
                self.pc = 26;
            }
            // xReturned = xTaskNotifyWait( 0x00, 0x00, &ulNotifiedValue, 0 );
            26 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, 0) {
                    self.returned = ok;
                    self.notified_value = value;
                }
                self.pc = 27;
            }
            // ---- querying the previous value while notifying ----
            // xTaskNotifyAndQuery( xTaskToNotify, 0x00, eSetBits, &ulPreviousValue );
            27 => {
                if let Ok((_, previous)) = k.notify_and_query(task, IDX, 0, NotifyAction::SetBits) {
                    self.previous_value = previous;
                }
                self.pc = 28;
            }
            // xTaskNotifyWait( 0x00, notifyUINT32_MAX, &ulNotifiedValue, 0 );  -- clear all bits
            28 => {
                if let Ok(Wait::Ready((_, value))) = k.notify_wait(IDX, 0, u32::MAX, 0) {
                    self.notified_value = value;
                }
                self.pc = 29;
            }
            // xTaskNotifyAndQuery( xTaskToNotify, 0x00, eSetBits, &ulPreviousValue );
            // ulExpectedValue = 0;
            29 => {
                if let Ok((_, previous)) = k.notify_and_query(task, IDX, 0, NotifyAction::SetBits) {
                    self.previous_value = previous;
                    if previous != 0 {
                        s.error_status = false;
                    }
                }
                self.expected_value = 0;
                self.loop_i = 0x01;
                self.pc = 30;
            }
            // for( ulLoop = 0x01; ulLoop < 0x80UL; ulLoop <<= 1UL )
            //     xTaskNotifyAndQuery( xTaskToNotify, ulLoop, eSetBits, &ulPreviousValue );
            30 => {
                if self.loop_i < 0x80 {
                    if let Ok((_, previous)) =
                        k.notify_and_query(task, IDX, self.loop_i, NotifyAction::SetBits)
                    {
                        self.previous_value = previous;
                        if self.expected_value != previous {
                            s.error_status = false;
                        }
                    }
                    self.expected_value |= self.loop_i;
                    self.loop_i = self.loop_i.wrapping_shl(1);
                } else {
                    self.pc = 31;
                }
            }
            // ---- clearing the notification STATE ----
            // xTaskNotifyWait( notifyUINT32_MAX, 0, &ulNotifiedValue, 0 );
            31 => {
                if let Ok(Wait::Ready((_, value))) = k.notify_wait(IDX, u32::MAX, 0, 0) {
                    self.notified_value = value;
                }
                self.pc = 32;
            }
            // configASSERT( xTaskNotifyStateClear( NULL ) == pdFALSE );
            32 => {
                if k.notify_state_clear(None, IDX) != Ok(false) {
                    s.error_status = false;
                }
                self.pc = 33;
            }
            // xTaskNotifyAndQuery( xTaskToNotify, ulFirstNotifiedConst,
            //                      eSetValueWithoutOverwrite, &ulPreviousValue );
            33 => {
                if let Ok((_, previous)) =
                    k.notify_and_query(task, IDX, FIRST, NotifyAction::NoOverwrite)
                {
                    self.previous_value = previous;
                }
                self.pc = 34;
            }
            // configASSERT( xTaskNotifyStateClear( NULL ) == pdTRUE );
            34 => {
                if k.notify_state_clear(None, IDX) != Ok(true) {
                    s.error_status = false;
                }
                self.pc = 35;
            }
            // configASSERT( xTaskNotifyStateClear( NULL ) == pdFALSE );
            35 => {
                if k.notify_state_clear(None, IDX) != Ok(false) {
                    s.error_status = false;
                }
                self.pc = 36;
            }
            // ---- clearing bits in the notification VALUE ----
            // xTaskNotify( xTaskToNotify, notifyUINT32_MAX, eSetBits );
            36 => {
                let _ = k.notify(task, IDX, u32::MAX, NotifyAction::SetBits);
                self.pc = 37;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, notifyUINT32_HIGH_BYTE )
            //               == notifyUINT32_MAX );
            37 => {
                if k.notify_value_clear(Some(task), IDX, HIGH_BYTE) != Ok(u32::MAX) {
                    s.error_status = false;
                }
                self.pc = 38;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, notifyUINT32_LOW_BYTE )
            //               == ( notifyUINT32_MAX & ~notifyUINT32_HIGH_BYTE ) );
            38 => {
                if k.notify_value_clear(Some(task), IDX, LOW_BYTE) != Ok(ALL_BUT_HIGH) {
                    s.error_status = false;
                }
                self.pc = 39;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, notifyUINT32_MAX )
            //               == ( notifyUINT32_MAX & ~HIGH_BYTE & ~LOW_BYTE ) );
            39 => {
                if k.notify_value_clear(Some(task), IDX, u32::MAX) != Ok(ALL_BUT_HIGH_AND_LOW) {
                    s.error_status = false;
                }
                self.pc = 40;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, notifyUINT32_MAX ) == 0 );
            40 => {
                if k.notify_value_clear(Some(task), IDX, u32::MAX) != Ok(0) {
                    s.error_status = false;
                }
                self.pc = 41;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, 0UL ) == 0 );
            41 => {
                if k.notify_value_clear(Some(task), IDX, 0) != Ok(0) {
                    s.error_status = false;
                }
                self.pc = 42;
            }
            // configASSERT( ulTaskNotifyValueClear( xTaskToNotify, notifyUINT32_MAX ) == 0 );
            42 => {
                if k.notify_value_clear(Some(task), IDX, u32::MAX) != Ok(0) {
                    s.error_status = false;
                }
                self.pc = 43;
            }
            // configASSERT( xTaskNotifyStateClear( NULL ) == pdTRUE );
            43 => {
                if k.notify_state_clear(None, IDX) != Ok(true) {
                    s.error_status = false;
                }
                self.pc = 44;
            }
            // configASSERT( xTaskNotifyStateClear( NULL ) == pdFALSE );
            44 => {
                if k.notify_state_clear(None, IDX) != Ok(false) {
                    s.error_status = false;
                }
                self.pc = 45;
            }
            // ---- a timer that notifies this task while it is suspended ----
            // xSingleTaskTimer = xTimerCreate( "SingleNotify", notifySUSPENDED_TEST_TIMER_PERIOD,
            //                                  pdFALSE, NULL, prvSuspendedTaskTimerTestCallback );
            // ulNotifyCycleCount++;
            45 => {
                match k.timer_create(
                    "SingleNotify",
                    SUSPENDED_TEST_PERIOD,
                    false,
                    0,
                    CB_SUSPENDED_TEST,
                ) {
                    Ok(t) => self.single_task_timer = t,
                    Err(_) => s.error_status = false,
                }
                s.cycles = s.cycles.wrapping_add(1);
                self.pc = 46;
            }
            // xTaskNotifyWait( notifyUINT32_MAX, 0, NULL, 0 );
            46 => {
                let _ = k.notify_wait(IDX, u32::MAX, 0, 0);
                self.pc = 47;
            }
            // vTaskPrioritySet( NULL, configMAX_PRIORITIES - 1 );
            47 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 48;
            }
            // ulNotifiedValue = 0; xTimerStart( xSingleTaskTimer, portMAX_DELAY );
            48 => {
                self.notified_value = 0;
                if matches!(
                    k.timer_start(self.single_task_timer, max),
                    Ok(Wait::Ready(_))
                ) {
                    self.pc = 49;
                }
            }
            // xReturned = xTaskNotifyWait( 0, 0, &ulNotifiedValue, portMAX_DELAY );
            // The callback suspends and resumes without notifying, so this
            // comes back pdFALSE with the value untouched.
            // ulNotifyCycleCount++;
            49 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, max) {
                    self.returned = ok;
                    self.notified_value = value;
                    if ok || value != 0 {
                        s.error_status = false;
                    }
                    s.cycles = s.cycles.wrapping_add(1);
                    self.pc = 50;
                }
            }
            // xTimerStart( xSingleTaskTimer, portMAX_DELAY );
            50 => {
                if matches!(
                    k.timer_start(self.single_task_timer, max),
                    Ok(Wait::Ready(_))
                ) {
                    self.pc = 51;
                }
            }
            // xReturned = xTaskNotifyWait( 0, 0, &ulNotifiedValue, portMAX_DELAY );
            // This time the callback notifies while the task is suspended.
            51 => {
                if let Ok(Wait::Ready((ok, value))) = k.notify_wait(IDX, 0, 0, max) {
                    self.returned = ok;
                    self.notified_value = value;
                    if !ok || value == 0 {
                        s.error_status = false;
                    }
                    self.pc = 52;
                }
            }
            // vTaskPrioritySet( NULL, notifyTASK_PRIORITY );
            52 => {
                let _ = k.set_priority(None, PRIORITY);
                self.pc = 53;
            }
            // xTimerDelete( xSingleTaskTimer, portMAX_DELAY );
            // ulNotifyCycleCount++;
            53 => {
                if matches!(
                    k.timer_delete(self.single_task_timer, max),
                    Ok(Wait::Ready(_))
                ) {
                    s.cycles = s.cycles.wrapping_add(1);
                    self.pc = 54;
                }
            }
            // xTaskNotifyWait( notifyUINT32_MAX, 0, NULL, 0 );  -- leave all bits cleared
            54 => {
                let _ = k.notify_wait(IDX, u32::MAX, 0, 0);
                self.pc = 55;
            }
            // ---- back in prvNotifiedTask ----
            // xTimer = xTimerCreate( "Notifier", xMaxPeriod, pdFALSE, NULL, prvNotifyingTimer );
            // Publishing it to the interrupt half is what lets the tick start
            // notifying: `xNotifyTaskFromISR` does nothing until it is set.
            55 => {
                match k.timer_create("Notifier", u64::from(MAX_PERIOD), false, 0, CB_NOTIFYING) {
                    Ok(t) => with_isr(k, |isr| isr.timer = t),
                    Err(_) => s.error_status = false,
                }
                self.pc = LOOP_BASE;
            }
            // ---- the main loop ----
            // xPeriod = prvRand() % xMaxPeriod; if( xPeriod < xMinPeriod ) xPeriod = xMinPeriod;
            100 => {
                self.period = s.period();
                self.pc = 101;
            }
            // xTimerChangePeriod( xTimer, xPeriod, portMAX_DELAY );
            101 => {
                let timer = with_isr(k, |isr| isr.timer);
                if matches!(
                    k.timer_change_period(timer, self.period, max),
                    Ok(Wait::Ready(_))
                ) {
                    self.pc = 102;
                }
            }
            // xPeriod = prvRand() % xMaxPeriod; if( xPeriod < xMinPeriod ) xPeriod = xMinPeriod;
            102 => {
                self.period = s.period();
                self.pc = 103;
            }
            // if( ulTaskNotifyTake( pdFALSE, xPeriod ) != 0 ) ulTimerNotificationsReceived++;
            103 => {
                if let Ok(Wait::Ready(value)) = k.notify_take(IDX, false, self.period) {
                    if value != 0 {
                        s.received = s.received.wrapping_add(1);
                    }
                    self.pc = 104;
                }
            }
            // if( ulTaskNotifyTake( pdFALSE, xDontBlock ) != 0 ) ulTimerNotificationsReceived++;
            104 => {
                if let Ok(Wait::Ready(value)) = k.notify_take(IDX, false, DONT_BLOCK) {
                    if value != 0 {
                        s.received = s.received.wrapping_add(1);
                    }
                }
                self.pc = 105;
            }
            // ulTimerNotificationsReceived += ulTaskNotifyTake( pdTRUE, xPeriod );
            105 => {
                if let Ok(Wait::Ready(value)) = k.notify_take(IDX, true, self.period) {
                    s.received = s.received.wrapping_add(value);
                    self.pc = 106;
                }
            }
            // if( ( ulNotifyCycleCount % ulCyclesToRaisePriority ) == 0 )
            //     vTaskPrioritySet( xTaskToNotify, configMAX_PRIORITIES - 1 );
            106 => {
                if s.cycles % CYCLES_TO_RAISE_PRIORITY == 0 {
                    let _ = k.set_priority(Some(task), TOP_PRIORITY);
                    self.pc = 107;
                } else {
                    self.pc = 109;
                }
            }
            // ulTimerNotificationsReceived += ulTaskNotifyTake( pdTRUE, portMAX_DELAY );
            107 => {
                if let Ok(Wait::Ready(value)) = k.notify_take(IDX, true, max) {
                    s.received = s.received.wrapping_add(value);
                    self.pc = 108;
                }
            }
            // vTaskPrioritySet( xTaskToNotify, notifyTASK_PRIORITY );
            // ulNotifyCycleCount++;
            108 => {
                let _ = k.set_priority(Some(task), PRIORITY);
                s.cycles = s.cycles.wrapping_add(1);
                self.pc = LOOP_BASE;
            }
            // The `else` arm: the same take with no priority change.
            // ulNotifyCycleCount++;
            _ => {
                if let Ok(Wait::Ready(value)) = k.notify_take(IDX, true, max) {
                    s.received = s.received.wrapping_add(value);
                    s.cycles = s.cycles.wrapping_add(1);
                    self.pc = LOOP_BASE;
                }
            }
        }
        Step::Continue
    }
}

/// `vStartTaskNotifyTask`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let task = runner.kernel_mut().create_task("Notified", PRIORITY)?;
    runner.shared_mut().state = runner::State::TaskNotify(State {
        task,
        // Where the C seeds from the address of `prvRand`.
        next_rand: SEED,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::TaskNotify(Isr {
        task,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(task, runner::Body::TaskNotify(Body::default()));
    Ok(())
}
