//! `AbortDelay` — `xTaskAbortDelay` against every blocking surface.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/AbortDelay.c`. It is the
//! only scenario in the corpus whose subject is *unblocking a task early*,
//! and it is thorough about it: a controlling task walks a blocking task
//! through **eight** different ways to be blocked — a notification wait, a
//! notification take, `vTaskDelay`, `xTaskDelayUntil`, a semaphore, an
//! event group, a queue send and a stream-buffer receive — and aborts each
//! one half way through.
//!
//! Each test has the same three-part shape, and the shape is the check:
//!
//! 1. block for `MAX_BLOCK_TIME` and **time out** — proving the call blocks;
//! 2. block again and be **aborted** at `HALF_MAX_BLOCK_TIME` — proving
//!    `abort_delay` reaches that surface;
//! 3. block a third time and **time out** again — proving the abort left
//!    the task able to block normally afterwards.
//!
//! Step 3 is the one worth naming. An abort that corrupted the task's
//! delayed-list membership would still pass steps 1 and 2.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.
//! The eight helpers are laid out as numbered subroutine blocks — a `for`
//! loop over a blocking call cannot be written as a loop here, because the
//! call returns `Blocked` and the same `pc` runs again.

use core::fmt;

use rusty_rtos_core::error::{Error, Result};
use rusty_rtos_core::handle::{EventGroupHandle, QueueHandle, StreamBufferHandle, TaskHandle};
use rusty_rtos_kernel::kernel::NotifyAction;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `abtCONTROLLING_PRIORITY`: `configMAX_PRIORITIES - 3`.
pub const CONTROLLING_PRIORITY: u8 = 4;
/// `abtBLOCKING_PRIORITY`: `configMAX_PRIORITIES - 2`.
pub const BLOCKING_PRIORITY: u8 = 5;
/// `xMaxBlockTime`: `pdMS_TO_TICKS( 100 )` at 1000 Hz.
pub const MAX_BLOCK_TIME: u64 = 100;
/// `xHalfMaxBlockTime`: `pdMS_TO_TICKS( 50 )`.
pub const HALF_MAX_BLOCK_TIME: u64 = 50;
/// `xAllowableMargin`: `pdMS_TO_TICKS( 7 )`.
pub const ALLOWABLE_MARGIN: u64 = 7;
/// `xStartMargin`, the two ticks the controlling task allows for the
/// blocking task entering the Blocked state after being told to.
const START_MARGIN: u64 = 2;
/// `portMAX_DELAY` for this config's 64-bit `TickType_t`.
const MAX_DELAY: u64 = u64::MAX;

/// The tests, in the order the controlling task walks them.
const NOTIFY_WAIT_ABORTS: u32 = 0;
const NOTIFY_TAKE_ABORTS: u32 = 1;
const DELAY_ABORTS: u32 = 2;
const DELAY_UNTIL_ABORTS: u32 = 3;
const SEMAPHORE_TAKE_ABORTS: u32 = 4;
const EVENT_GROUP_ABORTS: u32 = 5;
const QUEUE_SEND_ABORTS: u32 = 6;
const STREAM_BUFFER_RECEIVE: u32 = 7;
/// `abtMAX_TESTS`.
const MAX_TESTS: u32 = 8;

/// The bit `prvTestAbortingEventGroupWait` waits for.
const BITS_TO_WAIT_FOR: u32 = 0x01;
/// The queue the send test fills, and the stream buffer's size and trigger.
const QUEUE_LENGTH: usize = 1;
const STREAM_BYTES: usize = 1;
const STREAM_TRIGGER: usize = 1;

/// `AbortDelay.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// The handle the C recovers with `xTaskGetHandle( pcControllingTaskName )`.
    ///
    /// Looked up by name in the C and held here instead, which changes no
    /// trace line: `xTaskGetHandle` walks the task lists and emits nothing.
    pub controlling: TaskHandle,
    /// As above, for `pcBlockingTaskName`.
    pub blocking: TaskHandle,
    /// `xControllingCycles`.
    pub controlling_cycles: i32,
    /// `xBlockingCycles`.
    pub blocking_cycles: i32,
    /// `xErrorOccurred`. The C stores `__LINE__`; a bool is enough here
    /// because the trace says where.
    pub error: bool,
    /// The semaphore `prvTestAbortingSemaphoreTake` blocks on.
    pub semaphore: QueueHandle,
    /// The queue `prvTestAbortingQueueSend` fills and then blocks on.
    pub queue: QueueHandle,
    /// The event group `prvTestAbortingEventGroupWait` waits on.
    pub group: EventGroupHandle,
    /// The stream buffer `prvTestAbortingStreamBufferReceive` reads.
    pub stream: StreamBufferHandle,
    /// The cycle counts the previous check saw.
    last_controlling: i32,
    last_blocking: i32,
    /// The FIRST margin failure seen: `(expected, blocked, pc)`.
    ///
    /// `error` alone says a block came back wrong and not HOW, and the two
    /// directions are different findings: `blocked < expected` is what an
    /// abort firing early looks like, which is the interesting one;
    /// `blocked > expected + ALLOWABLE_MARGIN` is an overrun. Recording the
    /// first one costs three words of RAM and turns "AbortDelay fails the
    /// hour" into a defect with a number.
    ///
    /// Only the first is kept: once the scenario is off its expected
    /// schedule every later check is downstream of that, and the first is
    /// the one with a cause.
    pub first_margin_failure: Option<(u64, u64, u8)>,
    /// How the first non-`Blocked` queue send at `pc 72` came back:
    /// `"completed"` or `"refused: <kind>"`. See [`State::note_send_outcome`].
    pub first_send_outcome: Option<&'static str>,
    /// Set when `queue_create` is REFUSED, which `unwrap_or_default` would
    /// otherwise turn silently into an invalid handle.
    pub queue_create_refused: bool,
}

impl State {
    /// Remember the first margin failure and nothing after it.
    fn note_margin_failure(&mut self, expected: u64, blocked: u64, pc: u8) {
        if self.first_margin_failure.is_none() {
            self.first_margin_failure = Some((expected, blocked, pc));
        }
    }

    /// Remember how the first non-`Blocked` send came back.
    ///
    /// `pc 72`'s arm is `_`, which catches a completed send AND a refused
    /// one, and both present at the margin check as `blocked 0` because no
    /// time passes either way. They are different defects — a wrong queue
    /// fill against a wrong handle — so the two are told apart here rather
    /// than left to be guessed from the margin alone.
    ///
    /// A `&'static str` and not the `Error`: this is a scenario's state, it
    /// lives in `.bss` on a microcontroller, and the name is what a reader
    /// wants.
    fn note_send_outcome(&mut self, outcome: &'static str) {
        if self.first_send_outcome.is_none() {
            self.first_send_outcome = Some(outcome);
        }
    }

    /// `xAreAbortDelayTestTasksStillRunning`: both tasks must have moved
    /// on since the last check, and neither may have flagged an error.
    pub fn still_running(&mut self) -> bool {
        let mut running = true;
        if self.controlling_cycles == self.last_controlling {
            running = false;
        }
        if self.blocking_cycles == self.last_blocking {
            running = false;
        }
        if self.error {
            running = false;
        }
        self.last_controlling = self.controlling_cycles;
        self.last_blocking = self.blocking_cycles;
        running
    }
}

/// One of the scenario's two tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvControllingTask`.
    Controlling(Controlling),
    /// `prvBlockingTask`.
    Blocking(Blocking),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::AbortDelay(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Controlling(b) => b.step(k, state),
            Self::Blocking(b) => b.step(k, state),
        }
    }
}

/// `prvCheckExpectedTimeIsWithinAnAcceptableMargin`.
///
/// The C fails if the block was SHORTER than expected, or longer by more
/// than the margin. A blocked-for time that is too short is the interesting
/// direction: it is what an abort firing early would look like.
fn outside_margin(start: u64, now: u64, expected: u64) -> bool {
    let blocked = now.wrapping_sub(start);
    blocked < expected || blocked > expected.saturating_add(ALLOWABLE_MARGIN)
}

/// `prvControllingTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Controlling {
    pc: u8,
    /// `ulTestToPerform`.
    test: u32,
    /// `xTimeAtStart`.
    time_at_start: u64,
}

impl Controlling {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // xBlockingTask = xTaskGetHandle( pcBlockingTaskName );
            //
            // Not free: the lookup emits no event but costs one
            // critical-section exit, which is one unit of sim time. Holding
            // the handle from creation instead diverged the trace.
            0 => {
                if let Ok(found) = k.task_get_handle("AbtBlk") {
                    s.blocking = found;
                }
                self.pc = 8;
            }
            // xTimeAtStart = xTaskGetTickCount();
            8 => {
                self.time_at_start = k.tick_count();
                self.pc = 1;
            }
            // xTaskNotify( xBlockingTask, ulTestToPerform, eSetValueWithOverwrite );
            1 => {
                let _ = k.notify(s.blocking, 0, self.test, NotifyAction::Overwrite);
                self.pc = 2;
            }
            // vTaskPrioritySet( NULL, abtBLOCKING_PRIORITY );
            2 => {
                let _ = k.set_priority(None, BLOCKING_PRIORITY);
                self.pc = 3;
            }
            // vTaskDelay( xMaxBlockTime + xHalfMaxBlockTime + xStartMargin );
            3 => {
                let _ = k.delay(MAX_BLOCK_TIME + HALF_MAX_BLOCK_TIME + START_MARGIN);
                self.pc = 4;
            }
            // if( xTaskAbortDelay( xBlockingTask ) != pdPASS ) { error }
            4 => {
                if k.abort_delay(s.blocking) != Ok(true) {
                    s.error = true;
                }
                self.pc = 5;
            }
            // vTaskPrioritySet( NULL, abtCONTROLLING_PRIORITY );
            5 => {
                let _ = k.set_priority(None, CONTROLLING_PRIORITY);
                self.pc = 6;
            }
            // ulTaskNotifyTake( pdTRUE, portMAX_DELAY );
            6 => match k.notify_take(0, true, MAX_DELAY) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.pc = 7,
            },
            // prvCheckExpectedTimeIsWithinAnAcceptableMargin( xTimeAtStart,
            //     xMaxBlockTime + xMaxBlockTime + xHalfMaxBlockTime );
            7 => {
                let expected = MAX_BLOCK_TIME + MAX_BLOCK_TIME + HALF_MAX_BLOCK_TIME;
                let now = k.tick_count();
                if outside_margin(self.time_at_start, now, expected) {
                    s.note_margin_failure(expected, now.wrapping_sub(self.time_at_start), 7);
                    s.error = true;
                }
                self.test = self.test.wrapping_add(1);
                if self.test >= MAX_TESTS {
                    self.test = 0;
                }
                s.controlling_cycles = s.controlling_cycles.wrapping_add(1);
                self.pc = 8;
            }
            _ => return Step::Finish(false),
        }
        Step::Continue
    }
}

/// `prvBlockingTask`, with the eight `prvTestAborting*` helpers inlined as
/// subroutine blocks at `pc` 10, 20, 30 … 80. Each returns to `pc` 3.
#[derive(Debug, Clone, Copy, Default)]
pub struct Blocking {
    pc: u8,
    /// `xTimeAtStart`, shared by every helper.
    time_at_start: u64,
    /// `xLastBlockTime`, the `xTaskDelayUntil` cursor.
    last_block_time: u64,
}

impl Blocking {
    /// The three-part shape every helper has: block, check, advance. `pc`
    /// arms call this so the pattern is written once.
    fn checked(&mut self, k: &SimKernel<impl fmt::Write>, s: &mut State, expected: u64, next: u8) {
        let now = k.tick_count();
        if outside_margin(self.time_at_start, now, expected) {
            s.note_margin_failure(expected, now.wrapping_sub(self.time_at_start), self.pc);
            s.error = true;
        }
        self.pc = next;
    }

    #[allow(clippy::too_many_lines)]
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // --- prvPerformSingleTaskTests() -------------------------------
            // xTaskAbortDelay on a task that is NOT blocked must answer false.
            0 => {
                let me = k.current();
                if k.abort_delay(me) != Ok(false) {
                    s.error = true;
                }
                self.pc = 1;
            }

            // xControllingTask = xTaskGetHandle( pcControllingTaskName );
            // As above: no event, one exit.
            1 => {
                if let Ok(found) = k.task_get_handle("AbtCtrl") {
                    s.controlling = found;
                }
                self.pc = 2;
            }

            // --- for( ;; ) -------------------------------------------------
            // xTaskNotifyWait( 0, ulMax, &ulNotificationValue, portMAX_DELAY );
            2 => match k.notify_wait(0, 0, u32::MAX, MAX_DELAY) {
                Ok(Wait::Blocked) => return Step::Continue,
                Ok(Wait::Ready((_, value))) => {
                    self.pc = match value {
                        NOTIFY_WAIT_ABORTS => 10,
                        NOTIFY_TAKE_ABORTS => 20,
                        DELAY_ABORTS => 30,
                        DELAY_UNTIL_ABORTS => 40,
                        SEMAPHORE_TAKE_ABORTS => 50,
                        EVENT_GROUP_ABORTS => 60,
                        QUEUE_SEND_ABORTS => 70,
                        STREAM_BUFFER_RECEIVE => 80,
                        // "Should not get here", and the C does not error.
                        _ => 3,
                    };
                }
                Err(_) => self.pc = 3,
            },
            // xTaskNotifyGive( xControllingTask );
            //
            // The C defines this as xTaskGenericNotify with eIncrement, so
            // it is the ordinary notify and traces as one.
            3 => {
                let _ = k.notify(s.controlling, 0, 0, NotifyAction::Increment);
                s.blocking_cycles = s.blocking_cycles.wrapping_add(1);
                self.pc = 2;
            }

            // --- prvTestAbortingTaskNotifyWait ----------------------------
            10 => {
                self.time_at_start = k.tick_count();
                self.pc = 11;
            }
            11 => match k.notify_wait(0, 0, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, MAX_BLOCK_TIME, 12),
            },
            12 => {
                self.time_at_start = k.tick_count();
                self.pc = 13;
            }
            // portMAX_DELAY, not xMaxBlockTime. The block that is going to
            // be aborted waits FOREVER, so nothing but the abort can end
            // it — a timeout would prove nothing. It also means the task
            // goes to the SUSPENDED list rather than a delayed one, which
            // emits no `MOVED_TASK_TO_DELAYED_LIST`; writing this as a
            // 100-tick wait put an extra line in the trace, and that is
            // how the difference was found.
            13 => match k.notify_wait(0, 0, 0, MAX_DELAY) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, HALF_MAX_BLOCK_TIME, 14),
            },
            14 => {
                self.time_at_start = k.tick_count();
                self.pc = 15;
            }
            15 => match k.notify_wait(0, 0, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, MAX_BLOCK_TIME, 3),
            },

            // --- prvTestAbortingTaskNotifyTake ----------------------------
            20 => {
                self.time_at_start = k.tick_count();
                self.pc = 21;
            }
            21 => match k.notify_take(0, false, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                Ok(Wait::Ready(v)) => {
                    if v != 0 {
                        s.error = true;
                    }
                    self.checked(k, s, MAX_BLOCK_TIME, 22);
                }
                Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 22),
            },
            22 => {
                self.time_at_start = k.tick_count();
                self.pc = 23;
            }
            23 => match k.notify_take(0, false, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                Ok(Wait::Ready(v)) => {
                    if v != 0 {
                        s.error = true;
                    }
                    self.checked(k, s, HALF_MAX_BLOCK_TIME, 24);
                }
                Err(_) => self.checked(k, s, HALF_MAX_BLOCK_TIME, 24),
            },
            24 => {
                self.time_at_start = k.tick_count();
                self.pc = 25;
            }
            25 => match k.notify_take(0, false, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                Ok(Wait::Ready(v)) => {
                    if v != 0 {
                        s.error = true;
                    }
                    self.checked(k, s, MAX_BLOCK_TIME, 3);
                }
                Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 3),
            },

            // --- prvTestAbortingTaskDelay ---------------------------------
            30 => {
                self.time_at_start = k.tick_count();
                self.pc = 31;
            }
            31 => {
                let _ = k.delay(MAX_BLOCK_TIME);
                self.pc = 32;
            }
            32 => self.checked(k, s, MAX_BLOCK_TIME, 33),
            33 => {
                self.time_at_start = k.tick_count();
                self.pc = 34;
            }
            34 => {
                let _ = k.delay(MAX_BLOCK_TIME);
                self.pc = 35;
            }
            35 => self.checked(k, s, HALF_MAX_BLOCK_TIME, 36),
            36 => {
                self.time_at_start = k.tick_count();
                self.pc = 37;
            }
            37 => {
                let _ = k.delay(MAX_BLOCK_TIME);
                self.pc = 38;
            }
            38 => self.checked(k, s, MAX_BLOCK_TIME, 3),

            // --- prvTestAbortingTaskDelayUntil ----------------------------
            40 => {
                self.time_at_start = k.tick_count();
                self.last_block_time = self.time_at_start;
                self.pc = 41;
            }
            41 => {
                let _ = k.delay_until(&mut self.last_block_time, MAX_BLOCK_TIME);
                self.pc = 42;
            }
            42 => self.checked(k, s, MAX_BLOCK_TIME, 43),
            43 => {
                self.time_at_start = k.tick_count();
                self.last_block_time = self.time_at_start;
                self.pc = 44;
            }
            44 => {
                let _ = k.delay_until(&mut self.last_block_time, MAX_BLOCK_TIME);
                self.pc = 45;
            }
            45 => self.checked(k, s, HALF_MAX_BLOCK_TIME, 46),
            46 => {
                self.time_at_start = k.tick_count();
                self.last_block_time = self.time_at_start;
                self.pc = 47;
            }
            47 => {
                let _ = k.delay_until(&mut self.last_block_time, MAX_BLOCK_TIME);
                self.pc = 48;
            }
            48 => self.checked(k, s, MAX_BLOCK_TIME, 3),

            // --- prvTestAbortingSemaphoreTake -----------------------------
            // The C creates the semaphore INSIDE the helper, every time the
            // test runs, and the trace records each creation. Creating it
            // once up front diverged at line 1.
            50 => {
                s.semaphore = k.semaphore_create_binary().unwrap_or_default();
                self.time_at_start = k.tick_count();
                self.pc = 51;
            }
            51 => match k.queue_receive(s.semaphore, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, MAX_BLOCK_TIME, 52),
            },
            52 => {
                self.time_at_start = k.tick_count();
                self.pc = 53;
            }
            // portMAX_DELAY here too — the other helper that makes the
            // aborted block an infinite one.
            53 => match k.queue_receive(s.semaphore, MAX_DELAY) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, HALF_MAX_BLOCK_TIME, 54),
            },
            54 => {
                self.time_at_start = k.tick_count();
                self.pc = 55;
            }
            55 => match k.queue_receive(s.semaphore, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, MAX_BLOCK_TIME, 56),
            },
            // vSemaphoreDelete( xSemaphore ). Traces nothing and costs one
            // exit, because heap_N wraps free in suspend/resume.
            56 => {
                let _ = k.queue_delete(s.semaphore);
                self.pc = 3;
            }

            // --- prvTestAbortingEventGroupWait ----------------------------
            60 => {
                s.group = k.event_group_create().unwrap_or_default();
                self.time_at_start = k.tick_count();
                self.pc = 61;
            }
            61 => {
                match k.event_group_wait_bits(s.group, BITS_TO_WAIT_FOR, true, true, MAX_BLOCK_TIME)
                {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(bits)) => {
                        if bits != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, MAX_BLOCK_TIME, 62);
                    }
                    Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 62),
                }
            }
            62 => {
                self.time_at_start = k.tick_count();
                self.pc = 63;
            }
            63 => {
                match k.event_group_wait_bits(s.group, BITS_TO_WAIT_FOR, true, true, MAX_BLOCK_TIME)
                {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(bits)) => {
                        if bits != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, HALF_MAX_BLOCK_TIME, 64);
                    }
                    Err(_) => self.checked(k, s, HALF_MAX_BLOCK_TIME, 64),
                }
            }
            64 => {
                self.time_at_start = k.tick_count();
                self.pc = 65;
            }
            65 => {
                match k.event_group_wait_bits(s.group, BITS_TO_WAIT_FOR, true, true, MAX_BLOCK_TIME)
                {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(bits)) => {
                        if bits != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, MAX_BLOCK_TIME, 66);
                    }
                    Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 66),
                }
            }
            // vEventGroupDelete( xEventGroup ).
            66 => {
                let _ = k.event_group_delete(s.group);
                self.pc = 3;
            }

            // --- prvTestAbortingQueueSend ---------------------------------
            70 => {
                match k.queue_create(QUEUE_LENGTH) {
                    Ok(q) => s.queue = q,
                    Err(_) => {
                        // unwrap_or_default() used to hide this, and an
                        // invalid handle surfaces 100 ticks later as a
                        // margin violation rather than as a refused create.
                        s.queue_create_refused = true;
                        s.queue = QueueHandle::default();
                    }
                }
                self.pc = 71;
            }
            // The queue must be FULL for a send to block, and the C fills it
            // with an ordinary blocking send that succeeds immediately.
            71 => match k.queue_send(s.queue, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => {
                    self.time_at_start = k.tick_count();
                    self.pc = 72;
                }
            },
            72 => match k.queue_send(s.queue, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                outcome => {
                    // The arm used to be `_`, which could not tell a
                    // completed send from a refused one — and both reach the
                    // margin check as `blocked 0`.
                    //
                    // The outcome is recorded only if THIS call is the one
                    // that fails the margin. `Err(Full)` is the CORRECT
                    // answer for a send that blocked and then timed out, so
                    // recording the first refusal seen would record a
                    // healthy one and say nothing.
                    let fresh = s.first_margin_failure.is_none();
                    let described = match outcome {
                        Ok(_) => "completed",
                        Err(Error::Timeout) => "refused: Timeout",
                        Err(Error::InvalidHandle) => "refused: InvalidHandle",
                        Err(Error::Gone) => "refused: Gone",
                        Err(Error::Full) => "refused: Full",
                        Err(Error::Empty) => "refused: Empty",
                        Err(Error::Busy) => "refused: Busy",
                        Err(Error::NotActive) => "refused: NotActive",
                        Err(Error::SchedulerSuspended) => "refused: SchedulerSuspended",
                        Err(Error::NoMemory) => "refused: NoMemory",
                        Err(_) => "refused: other",
                    };
                    self.checked(k, s, MAX_BLOCK_TIME, 73);
                    if fresh && s.first_margin_failure.is_some() {
                        s.note_send_outcome(described);
                    }
                }
            },
            73 => {
                self.time_at_start = k.tick_count();
                self.pc = 74;
            }
            74 => match k.queue_send(s.queue, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, HALF_MAX_BLOCK_TIME, 75),
            },
            75 => {
                self.time_at_start = k.tick_count();
                self.pc = 76;
            }
            76 => match k.queue_send(s.queue, 0, MAX_BLOCK_TIME) {
                Ok(Wait::Blocked) => return Step::Continue,
                _ => self.checked(k, s, MAX_BLOCK_TIME, 77),
            },
            // vQueueDelete( xQueue ).
            77 => {
                let _ = k.queue_delete(s.queue);
                self.pc = 3;
            }

            // --- prvTestAbortingStreamBufferReceive -----------------------
            80 => {
                s.stream = k
                    .stream_buffer_create(STREAM_BYTES, STREAM_TRIGGER)
                    .unwrap_or_default();
                self.time_at_start = k.tick_count();
                self.pc = 81;
            }
            81 => {
                let mut rx = [0u8; STREAM_BYTES];
                match k.stream_buffer_receive(s.stream, &mut rx, MAX_BLOCK_TIME) {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(n)) => {
                        if n != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, MAX_BLOCK_TIME, 82);
                    }
                    Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 82),
                }
            }
            82 => {
                self.time_at_start = k.tick_count();
                self.pc = 83;
            }
            83 => {
                let mut rx = [0u8; STREAM_BYTES];
                match k.stream_buffer_receive(s.stream, &mut rx, MAX_BLOCK_TIME) {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(n)) => {
                        if n != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, HALF_MAX_BLOCK_TIME, 84);
                    }
                    Err(_) => self.checked(k, s, HALF_MAX_BLOCK_TIME, 84),
                }
            }
            84 => {
                self.time_at_start = k.tick_count();
                self.pc = 85;
            }
            85 => {
                let mut rx = [0u8; STREAM_BYTES];
                match k.stream_buffer_receive(s.stream, &mut rx, MAX_BLOCK_TIME) {
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(n)) => {
                        if n != 0 {
                            s.error = true;
                        }
                        self.checked(k, s, MAX_BLOCK_TIME, 86);
                    }
                    Err(_) => self.checked(k, s, MAX_BLOCK_TIME, 86),
                }
            }
            // vStreamBufferDelete( xStreamBuffer ).
            86 => {
                let _ = k.stream_buffer_delete(s.stream);
                self.pc = 3;
            }

            _ => return Step::Finish(false),
        }
        Step::Continue
    }
}

/// `vCreateAbortDelayTasks`.
///
/// # Errors
/// Propagates any creation failure from the sim kernel.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    // `vCreateAbortDelayTasks` creates the two tasks and NOTHING else. Every
    // object a helper blocks on is created inside that helper, each time it
    // runs, and the trace records each creation in order.
    let (controlling, blocking) = {
        let mut k = runner.kernel_mut();
        let controlling = k.create_task("AbtCtrl", CONTROLLING_PRIORITY)?;
        let blocking = k.create_task("AbtBlk", BLOCKING_PRIORITY)?;
        (controlling, blocking)
    };
    runner.shared_mut().state = runner::State::AbortDelay(State {
        controlling,
        blocking,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        controlling,
        runner::Body::AbortDelay(Body::Controlling(Controlling::default())),
    );
    runner.attach(
        blocking,
        runner::Body::AbortDelay(Body::Blocking(Blocking::default())),
    );
    Ok(())
}
