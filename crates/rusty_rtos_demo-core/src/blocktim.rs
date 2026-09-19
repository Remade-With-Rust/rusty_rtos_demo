//! `blocktim` — block times, honoured to the tick.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/blocktim.c`, and it is
//! the scenario that checks the *numbers*. Everything else in the corpus
//! asks whether the right task ran; this one asks whether it ran at the
//! right time. A task blocks on a queue for 10, 20, 40, 80 and 160 ticks
//! in turn and measures how long it actually waited: too short is a
//! failure, and so is more than fifteen ticks too long. It also walks
//! `xTaskDelayUntil` through its whole contract, including the two cases
//! where the deadline has already passed and it must not block at all.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.
//! The two long task functions are laid out as numbered blocks with the C
//! structure in the comments, because a `for` loop over a blocking call
//! cannot be written as a loop here — the call returns `Blocked` and the
//! same `pc` runs again.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `bktQUEUE_LENGTH`.
pub const QUEUE_LENGTH: usize = 5;
/// `bktSHORT_WAIT`: `pdMS_TO_TICKS( 20 )` at 1000 Hz.
pub const SHORT_WAIT: u64 = 20;
/// `bktPRIMARY_BLOCK_TIME`, shifted left once per queue slot.
pub const PRIMARY_BLOCK_TIME: u64 = 10;
/// `bktALLOWABLE_MARGIN`.
pub const ALLOWABLE_MARGIN: u64 = 15;
/// `bktTIME_TO_BLOCK`.
pub const TIME_TO_BLOCK: u64 = 175;
/// `bktDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;
/// `bktRUN_INDICATOR`.
pub const RUN_INDICATOR: u32 = 0x55;
/// `bktPRIMARY_PRIORITY`: `configMAX_PRIORITIES - 3`.
pub const PRIMARY_PRIORITY: u8 = 4;
/// `bktSECONDARY_PRIORITY`: `configMAX_PRIORITIES - 4`.
pub const SECONDARY_PRIORITY: u8 = 3;
/// The priority `prvBasicDelayTests` runs at:
/// `configTIMER_TASK_PRIORITY - 1`.
pub const DELAY_TEST_PRIORITY: u8 = 5;
/// `xPeriod` in `prvBasicDelayTests`.
const PERIOD: u64 = 75;
/// `xCycles`.
const CYCLES: u64 = 5;
/// `xAllowableMargin`: `bktALLOWABLE_MARGIN >> 1`.
const DELAY_MARGIN: u64 = ALLOWABLE_MARGIN >> 1;
/// `xHalfPeriod`.
const HALF_PERIOD: u64 = PERIOD / 2;

/// `blocktim.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xTestQueue`.
    pub queue: QueueHandle,
    /// `xSecondary`.
    pub secondary: TaskHandle,
    /// `xPrimaryCycles`.
    pub primary_cycles: i32,
    /// `xSecondaryCycles`.
    pub secondary_cycles: i32,
    /// `xErrorOccurred`.
    pub error: bool,
    /// `xRunIndicator`.
    pub run_indicator: u32,
    /// `xLastPrimaryCycleCount`, a static inside the check function.
    pub last_primary: i32,
    /// `xLastSecondaryCycleCount`, likewise.
    pub last_secondary: i32,
}

impl State {
    /// `xAreBlockTimeTestTasksStillRunning`.
    pub fn still_running(&mut self) -> bool {
        let mut running = true;
        if self.primary_cycles == self.last_primary {
            running = false;
        }
        if self.secondary_cycles == self.last_secondary {
            running = false;
        }
        if self.error {
            running = false;
        }
        self.last_secondary = self.secondary_cycles;
        self.last_primary = self.primary_cycles;
        running
    }
}

/// One of the scenario's two tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vPrimaryBlockTimeTestTask`.
    Primary(Primary),
    /// `vSecondaryBlockTimeTestTask`.
    Secondary(Secondary),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::BlockTim(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Primary(b) => b.step(k, state),
            Self::Secondary(b) => b.step(k, state),
        }
    }
}

/// How far off a measured block time may be.
fn out_of_range(blocked: u64, expected: u64, margin: u64) -> bool {
    blocked < expected || blocked > expected.saturating_add(margin)
}

/// `vPrimaryBlockTimeTestTask`, `prvBasicDelayTests` inlined as a
/// subroutine at `pc` 100.
#[derive(Debug, Clone, Copy, Default)]
pub struct Primary {
    pc: u8,
    /// `xItem`, and `x` in the delay tests.
    item: u64,
    /// `xTimeToBlock`.
    time_to_block: u64,
    /// `xTimeWhenBlocking`.
    when_blocking: u64,
    /// `xPreTime` / `xPostTime`.
    post_time: u64,
    /// `xLastUnblockTime`, the `xTaskDelayUntil` cursor.
    last_unblock: u64,
}

impl Primary {
    #[allow(clippy::too_many_lines)]
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // prvBasicDelayTests();
            0 => self.pc = 100,

            // --- for( xItem = 0; xItem < bktQUEUE_LENGTH; xItem++ ) ---
            // A receive that must time out, at 10, 20, 40, 80 and 160 ticks.
            1 => {
                self.item = 0;
                self.pc = 2;
            }
            2 => {
                self.time_to_block = PRIMARY_BLOCK_TIME
                    .checked_shl(u32::try_from(self.item).unwrap_or(0))
                    .unwrap_or(PRIMARY_BLOCK_TIME);
                self.when_blocking = k.tick_count();
                self.pc = 3;
            }
            // if( xQueueReceive( ..., xTimeToBlock ) != errQUEUE_EMPTY ) { error }
            3 => match k.queue_receive(s.queue, self.time_to_block) {
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 4,
                Ok(Wait::Ready(_)) => {
                    s.error = true;
                    self.pc = 4;
                }
            },
            // xBlockedTime = xTaskGetTickCount() - xTimeWhenBlocking; then the
            // two range checks.
            4 => {
                let blocked = k.tick_count().wrapping_sub(self.when_blocking);
                if out_of_range(blocked, self.time_to_block, ALLOWABLE_MARGIN) {
                    s.error = true;
                }
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    2
                } else {
                    5
                };
            }

            // --- Fill the queue, so the next loop's sends must block. ---
            5 => {
                self.item = 0;
                self.pc = 6;
            }
            // if( xQueueSend( ..., bktDONT_BLOCK ) != pdPASS ) { error }
            6 => {
                if !matches!(
                    k.queue_send(s.queue, self.item, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    6
                } else {
                    7
                };
            }

            // --- The same five block times, on a send to a full queue. ---
            7 => {
                self.item = 0;
                self.pc = 8;
            }
            8 => {
                self.time_to_block = PRIMARY_BLOCK_TIME
                    .checked_shl(u32::try_from(self.item).unwrap_or(0))
                    .unwrap_or(PRIMARY_BLOCK_TIME);
                self.when_blocking = k.tick_count();
                self.pc = 9;
            }
            // if( xQueueSend( ..., xTimeToBlock ) != errQUEUE_FULL ) { error }
            9 => match k.queue_send(s.queue, self.item, self.time_to_block) {
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 10,
                Ok(Wait::Ready(())) => {
                    s.error = true;
                    self.pc = 10;
                }
            },
            10 => {
                let blocked = k.tick_count().wrapping_sub(self.when_blocking);
                if out_of_range(blocked, self.time_to_block, ALLOWABLE_MARGIN) {
                    s.error = true;
                }
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    8
                } else {
                    11
                };
            }

            // --- Wake the secondary task and watch it block on a full queue. ---
            11 => {
                s.run_indicator = 0;
                self.pc = 12;
            }
            12 => {
                let _ = k.resume(s.secondary);
                self.pc = 13;
            }
            // while( xRunIndicator != bktRUN_INDICATOR ) { vTaskDelay( bktSHORT_WAIT ); }
            13 => {
                if s.run_indicator == RUN_INDICATOR {
                    self.pc = 14;
                } else {
                    let _ = k.delay(SHORT_WAIT);
                }
            }
            14 => {
                let _ = k.delay(SHORT_WAIT);
                self.pc = 15;
            }
            15 => {
                s.run_indicator = 0;
                self.item = 0;
                self.pc = 16;
            }
            // Empty and refill one slot at a time; the secondary must stay
            // blocked throughout, even when it briefly outranks us.
            16 => {
                if !matches!(k.queue_receive(s.queue, DONT_BLOCK), Ok(Wait::Ready(_))) {
                    s.error = true;
                }
                self.pc = 17;
            }
            17 => {
                if !matches!(
                    k.queue_send(s.queue, self.item, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 18;
            }
            18 => {
                if s.run_indicator == RUN_INDICATOR {
                    s.error = true;
                }
                self.pc = 19;
            }
            19 => {
                let _ = k.set_priority(Some(s.secondary), PRIMARY_PRIORITY.saturating_add(2));
                self.pc = 20;
            }
            20 => {
                if s.run_indicator == RUN_INDICATOR {
                    s.error = true;
                }
                self.pc = 21;
            }
            21 => {
                let _ = k.set_priority(Some(s.secondary), SECONDARY_PRIORITY);
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    16
                } else {
                    22
                };
            }
            22 => {
                if s.run_indicator == RUN_INDICATOR {
                    self.pc = 23;
                } else {
                    let _ = k.delay(SHORT_WAIT);
                }
            }
            23 => {
                let _ = k.delay(SHORT_WAIT);
                self.pc = 24;
            }

            // --- Drain the queue, then the same dance on an empty one. ---
            24 => {
                s.run_indicator = 0;
                self.item = 0;
                self.pc = 25;
            }
            25 => {
                if !matches!(k.queue_receive(s.queue, DONT_BLOCK), Ok(Wait::Ready(_))) {
                    s.error = true;
                }
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    25
                } else {
                    26
                };
            }
            26 => {
                let _ = k.resume(s.secondary);
                self.pc = 27;
            }
            27 => {
                if s.run_indicator == RUN_INDICATOR {
                    self.pc = 28;
                } else {
                    let _ = k.delay(SHORT_WAIT);
                }
            }
            28 => {
                let _ = k.delay(SHORT_WAIT);
                self.pc = 29;
            }
            29 => {
                s.run_indicator = 0;
                self.item = 0;
                self.pc = 30;
            }
            30 => {
                if !matches!(
                    k.queue_send(s.queue, self.item, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 31;
            }
            31 => {
                if !matches!(k.queue_receive(s.queue, DONT_BLOCK), Ok(Wait::Ready(_))) {
                    s.error = true;
                }
                self.pc = 32;
            }
            32 => {
                if s.run_indicator == RUN_INDICATOR {
                    s.error = true;
                }
                self.pc = 33;
            }
            33 => {
                let _ = k.set_priority(Some(s.secondary), PRIMARY_PRIORITY.saturating_add(2));
                self.pc = 34;
            }
            34 => {
                if s.run_indicator == RUN_INDICATOR {
                    s.error = true;
                }
                self.pc = 35;
            }
            35 => {
                let _ = k.set_priority(Some(s.secondary), SECONDARY_PRIORITY);
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < QUEUE_LENGTH as u64 {
                    30
                } else {
                    36
                };
            }
            36 => {
                if s.run_indicator == RUN_INDICATOR {
                    self.pc = 37;
                } else {
                    let _ = k.delay(SHORT_WAIT);
                }
            }
            37 => {
                let _ = k.delay(SHORT_WAIT);
                self.pc = 38;
            }
            38 => {
                s.primary_cycles = s.primary_cycles.wrapping_add(1);
                self.pc = 0;
            }

            // ----------------------------- prvBasicDelayTests ----------
            // vTaskPrioritySet( NULL, configTIMER_TASK_PRIORITY - 1 );
            100 => {
                let _ = k.set_priority(None, DELAY_TEST_PRIORITY);
                self.pc = 101;
            }
            // xPreTime = xTaskGetTickCount();
            101 => {
                self.when_blocking = k.tick_count();
                self.pc = 102;
            }
            // vTaskDelay( bktTIME_TO_BLOCK );
            102 => {
                let _ = k.delay(TIME_TO_BLOCK);
                self.pc = 103;
            }
            // if( ( xPostTime - xPreTime ) > ( bktTIME_TO_BLOCK + xAllowableMargin ) ) { error }
            103 => {
                let elapsed = k.tick_count().wrapping_sub(self.when_blocking);
                if elapsed > TIME_TO_BLOCK.saturating_add(DELAY_MARGIN) {
                    s.error = true;
                }
                self.pc = 104;
            }
            // xPostTime = xTaskGetTickCount(); xLastUnblockTime = xPostTime;
            104 => {
                self.post_time = k.tick_count();
                self.last_unblock = self.post_time;
                self.item = 0;
                self.pc = 105;
            }
            // xExpectedUnblockTime = xPostTime + ( x * xPeriod );
            // vTaskDelayUntil( &xLastUnblockTime, xPeriod );
            105 => {
                self.when_blocking = self
                    .post_time
                    .wrapping_add(self.item.saturating_mul(PERIOD));
                let _ = k.delay_until(&mut self.last_unblock, PERIOD);
                self.pc = 106;
            }
            106 => {
                let late = k.tick_count().wrapping_sub(self.when_blocking);
                if late > TIME_TO_BLOCK.saturating_add(DELAY_MARGIN) {
                    s.error = true;
                }
                s.primary_cycles = s.primary_cycles.wrapping_add(1);
                self.item = self.item.saturating_add(1);
                self.pc = if self.item < CYCLES { 105 } else { 107 };
            }
            // The deadline is still ahead, so this one blocks.
            107 => {
                if k.delay_until(&mut self.last_unblock, PERIOD) != Ok(true) {
                    s.error = true;
                }
                self.pc = 108;
            }
            108 => {
                let _ = k.delay(HALF_PERIOD);
                self.pc = 109;
            }
            // Half a period late: still ahead of the next deadline.
            109 => {
                if k.delay_until(&mut self.last_unblock, PERIOD) != Ok(true) {
                    s.error = true;
                }
                self.pc = 110;
            }
            110 => {
                let _ = k.delay(PERIOD);
                self.pc = 111;
            }
            // A whole period late: the deadline has passed and it must not
            // block at all.
            111 => {
                if k.delay_until(&mut self.last_unblock, PERIOD) != Ok(false) {
                    s.error = true;
                }
                self.pc = 112;
            }
            // ...and the very next call is back in step, so it blocks again.
            112 => {
                if k.delay_until(&mut self.last_unblock, PERIOD) != Ok(true) {
                    s.error = true;
                }
                self.pc = 113;
            }
            113 => {
                let _ = k.delay(PERIOD.saturating_add(DELAY_MARGIN));
                self.pc = 114;
            }
            114 => {
                if k.delay_until(&mut self.last_unblock, PERIOD) != Ok(false) {
                    s.error = true;
                }
                self.pc = 115;
            }
            // vTaskPrioritySet( NULL, bktPRIMARY_PRIORITY );
            _ => {
                let _ = k.set_priority(None, PRIMARY_PRIORITY);
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vSecondaryBlockTimeTestTask`: suspend, then measure two long blocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Secondary {
    pc: u8,
    /// `xTimeWhenBlocking`.
    when_blocking: u64,
}

impl Secondary {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspend( NULL );
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // xTimeWhenBlocking = xTaskGetTickCount(); xData = 0;
            // xRunIndicator = bktRUN_INDICATOR;
            1 => {
                self.when_blocking = k.tick_count();
                s.run_indicator = RUN_INDICATOR;
                self.pc = 2;
            }
            // if( xQueueSend( ..., bktTIME_TO_BLOCK ) != errQUEUE_FULL ) { error }
            2 => match k.queue_send(s.queue, 0, TIME_TO_BLOCK) {
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 3,
                Ok(Wait::Ready(())) => {
                    s.error = true;
                    self.pc = 3;
                }
            },
            3 => {
                let blocked = k.tick_count().wrapping_sub(self.when_blocking);
                if out_of_range(blocked, TIME_TO_BLOCK, ALLOWABLE_MARGIN) {
                    s.error = true;
                }
                self.pc = 4;
            }
            // xRunIndicator = bktRUN_INDICATOR; vTaskSuspend( NULL );
            4 => {
                s.run_indicator = RUN_INDICATOR;
                let _ = k.suspend(None);
                self.pc = 5;
            }
            5 => {
                self.when_blocking = k.tick_count();
                s.run_indicator = RUN_INDICATOR;
                self.pc = 6;
            }
            // if( xQueueReceive( ..., bktTIME_TO_BLOCK ) != errQUEUE_EMPTY ) { error }
            6 => match k.queue_receive(s.queue, TIME_TO_BLOCK) {
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 7,
                Ok(Wait::Ready(_)) => {
                    s.error = true;
                    self.pc = 7;
                }
            },
            7 => {
                let blocked = k.tick_count().wrapping_sub(self.when_blocking);
                if out_of_range(blocked, TIME_TO_BLOCK, ALLOWABLE_MARGIN) {
                    s.error = true;
                }
                self.pc = 8;
            }
            // xRunIndicator = bktRUN_INDICATOR; xSecondaryCycles++;
            _ => {
                s.run_indicator = RUN_INDICATOR;
                s.secondary_cycles = s.secondary_cycles.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vCreateBlockTimeTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (queue, primary, secondary) = {
        let mut k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let primary = k.create_task("BTest1", PRIMARY_PRIORITY)?;
        let secondary = k.create_task("BTest2", SECONDARY_PRIORITY)?;
        (queue, primary, secondary)
    };
    runner.shared_mut().state = runner::State::BlockTim(State {
        queue,
        secondary,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        primary,
        runner::Body::BlockTim(Body::Primary(Primary::default())),
    );
    runner.attach(
        secondary,
        runner::Body::BlockTim(Body::Secondary(Secondary::default())),
    );
    Ok(())
}
