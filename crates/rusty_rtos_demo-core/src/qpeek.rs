//! `QPeek` — four priorities peeking at one queue, and the order they wake in.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/QPeek.c`. A low priority
//! task writes a value; the three tasks blocked above it wake in priority
//! order, and each *peeks* rather than receives, so the value stays on the
//! queue and the next one down wakes too. Each suspends itself when it is
//! done, which lets the writer run again. The last pass is the point of the
//! test: the high priority task receives instead of peeking, the value
//! leaves the queue, and the medium priority task must therefore never see
//! it at all.
//!
//! It is the scenario that proves a peek wakes a *receiver* rather than a
//! sender, that `uxQueueMessagesWaiting` agrees the item is still there,
//! and that self-suspension and resume land a task exactly where the C
//! leaves it.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `qpeekQUEUE_LENGTH`.
pub const QUEUE_LENGTH: usize = 5;
/// `qpeekNO_BLOCK`.
pub const NO_BLOCK: u64 = 0;
/// `qpeekSHORT_DELAY`.
pub const SHORT_DELAY: u64 = 10;

/// `qpeekLOW_PRIORITY`.
pub const LOW_PRIORITY: u8 = 0;
/// `qpeekMEDIUM_PRIORITY`.
pub const MEDIUM_PRIORITY: u8 = 1;
/// `qpeekHIGH_PRIORITY`.
pub const HIGH_PRIORITY: u8 = 2;
/// `qpeekHIGHEST_PRIORITY`.
pub const HIGHEST_PRIORITY: u8 = 3;

/// The first value the writer posts, which only the highest priority task
/// ever sees.
pub const FIRST_VALUE: u64 = 0x1122_3344;
/// The second, which all three peekers see in turn.
pub const SECOND_VALUE: u64 = 0x0123_4567;
/// The third, sent to the front, which the high priority task removes.
pub const THIRD_VALUE: u64 = 0xaabb_aabb;

/// `QPeek.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// The queue the four tasks share.
    pub queue: QueueHandle,
    /// `xMediumPriorityTask`.
    pub medium: TaskHandle,
    /// `xHighPriorityTask`.
    pub high: TaskHandle,
    /// `xHighestPriorityTask`.
    pub highest: TaskHandle,
    /// `xErrorDetected`.
    pub error: bool,
    /// `ulLoopCounter`.
    pub loops: u32,
    /// `ulLastLoopCounter`, a static inside the check function.
    pub last_loops: u32,
}

impl State {
    /// `xAreQueuePeekTasksStillRunning`: the medium priority task's counter
    /// must have moved, and nothing may have latched an error.
    pub fn still_running(&mut self) -> bool {
        if self.last_loops == self.loops {
            self.error = true;
        }
        self.last_loops = self.loops;
        !self.error
    }
}

/// One of the scenario's four tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvLowPriorityPeekTask`.
    Low(Low),
    /// `prvMediumPriorityPeekTask`.
    Medium(Medium),
    /// `prvHighPriorityPeekTask`.
    High(High),
    /// `prvHighestPriorityPeekTask`.
    Highest(Highest),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::QPeek(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Low(b) => b.step(k, state),
            Self::Medium(b) => b.step(k, state),
            Self::High(b) => b.step(k, state),
            Self::Highest(b) => b.step(k, state),
        }
    }
}

/// `prvLowPriorityPeekTask`: the writer, and the only task that ever
/// delays.
#[derive(Debug, Clone, Copy, Default)]
pub struct Low {
    pc: u8,
    /// `ulValue`.
    value: u64,
}

impl Low {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // ulValue = 0x11223344;
            // if( xQueueSendToBack( ..., qpeekNO_BLOCK ) != pdPASS ) { error }
            0 => {
                self.value = FIRST_VALUE;
                if !matches!(
                    k.queue_send(s.queue, self.value, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 1;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 0 ) { error }
            1 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 2;
            }
            // ulValue = 0x01234567;
            // if( xQueueSendToBack( ..., qpeekNO_BLOCK ) != pdPASS ) { error }
            2 => {
                self.value = SECOND_VALUE;
                if !matches!(
                    k.queue_send(s.queue, self.value, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 3;
            }
            // ulValue = 0;
            // if( xQueueReceive( ..., qpeekNO_BLOCK ) != pdPASS ) { error }
            3 => {
                self.value = 0;
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => self.value = value,
                    Ok(Wait::Blocked) | Err(_) => s.error = true,
                }
                self.pc = 4;
            }
            // if( ulValue != 0x01234567 ) { error }
            4 => {
                if self.value != SECOND_VALUE {
                    s.error = true;
                }
                self.pc = 5;
            }
            // vTaskDelay( qpeekSHORT_DELAY );
            5 => {
                let _ = k.delay(SHORT_DELAY);
                self.pc = 6;
            }
            // vTaskResume( xMediumPriorityTask );
            6 => {
                let _ = k.resume(s.medium);
                self.pc = 7;
            }
            // vTaskResume( xHighPriorityTask );
            7 => {
                let _ = k.resume(s.high);
                self.pc = 8;
            }
            // vTaskResume( xHighestPriorityTask );
            8 => {
                let _ = k.resume(s.highest);
                self.pc = 9;
            }
            // ulValue = 0xaabbaabb;
            // if( xQueueSendToFront( ..., qpeekNO_BLOCK ) != pdPASS ) { error }
            9 => {
                self.value = THIRD_VALUE;
                if !matches!(
                    k.queue_send_to_front(s.queue, self.value, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 10;
            }
            // if( xQueuePeek( ..., qpeekNO_BLOCK ) != errQUEUE_EMPTY ) { error }
            //
            // The high priority task received the value rather than peeking
            // it, so by now the queue really is empty. Anything but the
            // empty error means it is not.
            10 => {
                if !matches!(
                    k.queue_peek(s.queue, NO_BLOCK),
                    Err(rusty_rtos_core::error::Error::Empty)
                ) {
                    s.error = true;
                }
                self.pc = 11;
            }
            // vTaskResume( xHighPriorityTask );
            11 => {
                let _ = k.resume(s.high);
                self.pc = 12;
            }
            // vTaskResume( xHighestPriorityTask );
            12 => {
                let _ = k.resume(s.highest);
                self.pc = 13;
            }
            // vTaskDelay( qpeekSHORT_DELAY );
            _ => {
                let _ = k.delay(SHORT_DELAY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvMediumPriorityPeekTask`: peeks last, and is the only task that
/// counts the loop the check function watches.
#[derive(Debug, Clone, Copy, Default)]
pub struct Medium {
    pc: u8,
    /// `ulValue`.
    value: u64,
}

impl Medium {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xQueuePeek( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            0 => match k.queue_peek(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 1;
                }
            },
            // if( ulValue != 0x01234567 ) { error }
            1 => {
                if self.value != SECOND_VALUE {
                    s.error = true;
                }
                self.pc = 2;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            2 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 3;
            }
            // ulLoopCounter++;
            3 => {
                s.loops = s.loops.wrapping_add(1);
                self.pc = 4;
            }
            // vTaskSuspend( NULL );
            _ => {
                let _ = k.suspend(None);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvHighPriorityPeekTask`: peeks on the first pass and *receives* on the
/// second, which is what starves the medium priority task of the value.
#[derive(Debug, Clone, Copy, Default)]
pub struct High {
    pc: u8,
    /// `ulValue`.
    value: u64,
}

impl High {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xQueuePeek( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            0 => match k.queue_peek(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 1;
                }
            },
            // if( ulValue != 0x01234567 ) { error }
            1 => {
                if self.value != SECOND_VALUE {
                    s.error = true;
                }
                self.pc = 2;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            2 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 3;
            }
            // vTaskSuspend( NULL );
            3 => {
                let _ = k.suspend(None);
                self.pc = 4;
            }
            // if( xQueueReceive( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            4 => match k.queue_receive(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 5;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 5;
                }
            },
            // if( ulValue != 0xaabbaabb ) { error }
            5 => {
                if self.value != THIRD_VALUE {
                    s.error = true;
                }
                self.pc = 6;
            }
            // vTaskSuspend( NULL );
            _ => {
                let _ = k.suspend(None);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvHighestPriorityPeekTask`: wakes first every time, and is the only
/// task that sees the first value.
#[derive(Debug, Clone, Copy, Default)]
pub struct Highest {
    pc: u8,
    /// `ulValue`.
    value: u64,
}

impl Highest {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xQueuePeek( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            0 => match k.queue_peek(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 1;
                }
            },
            // if( ulValue != 0x11223344 ) { error }
            1 => {
                if self.value != FIRST_VALUE {
                    s.error = true;
                }
                self.pc = 2;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            2 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 3;
            }
            // ulValue = 0;
            // if( xQueueReceive( ..., qpeekNO_BLOCK ) != pdPASS ) { error }
            3 => {
                self.value = 0;
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => self.value = value,
                    Ok(Wait::Blocked) | Err(_) => s.error = true,
                }
                self.pc = 4;
            }
            // if( ulValue != 0x11223344 ) { error }
            4 => {
                if self.value != FIRST_VALUE {
                    s.error = true;
                }
                self.pc = 5;
            }
            // if( xQueuePeek( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            5 => match k.queue_peek(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 6;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 6;
                }
            },
            // if( ulValue != 0x01234567 ) { error }
            6 => {
                if self.value != SECOND_VALUE {
                    s.error = true;
                }
                self.pc = 7;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            7 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 8;
            }
            // vTaskSuspend( NULL );
            8 => {
                let _ = k.suspend(None);
                self.pc = 9;
            }
            // if( xQueuePeek( xQueue, &ulValue, portMAX_DELAY ) != pdPASS ) { error }
            9 => match k.queue_peek(s.queue, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    self.value = value;
                    self.pc = 10;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 10;
                }
            },
            // if( ulValue != 0xaabbaabb ) { error }
            10 => {
                if self.value != THIRD_VALUE {
                    s.error = true;
                }
                self.pc = 11;
            }
            // vTaskSuspend( NULL );
            _ => {
                let _ = k.suspend(None);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartQueuePeekTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (queue, low, medium, high, highest) = {
        let mut k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let low = k.create_task("PeekL", LOW_PRIORITY)?;
        let medium = k.create_task("PeekM", MEDIUM_PRIORITY)?;
        let high = k.create_task("PeekH1", HIGH_PRIORITY)?;
        let highest = k.create_task("PeekH2", HIGHEST_PRIORITY)?;
        (queue, low, medium, high, highest)
    };
    runner.shared_mut().state = runner::State::QPeek(State {
        queue,
        medium,
        high,
        highest,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(low, runner::Body::QPeek(Body::Low(Low::default())));
    runner.attach(medium, runner::Body::QPeek(Body::Medium(Medium::default())));
    runner.attach(high, runner::Body::QPeek(Body::High(High::default())));
    runner.attach(
        highest,
        runner::Body::QPeek(Body::Highest(Highest::default())),
    );
    Ok(())
}
