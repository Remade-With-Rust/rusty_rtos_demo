//! `QueueOverwrite` — a queue of one, written to whether or not it is full.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/QueueOverwrite.c`.
//! `xQueueOverwrite` is the one send that never fails and never blocks: on
//! a length-one queue it replaces whatever is there and leaves the message
//! count at one. The task half proves that — write, peek, and find the
//! value you just wrote with exactly one item still queued — five times a
//! loop.
//!
//! The other half runs in the tick interrupt. It is the first scenario in
//! the corpus with an interrupt half at all, and it walks a three-state
//! machine across three ticks: overwrite an empty queue and peek it,
//! overwrite the value that is now there, then receive and check the
//! *second* value came back. That is `xQueueOverwriteFromISR`,
//! `xQueuePeekFromISR` and `xQueueReceiveFromISR`, none of which any task
//! may call.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `qoDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;
/// `qoLOOPS`.
pub const LOOPS: u32 = 5;
/// The length both queues have. `xQueueOverwrite` is only for this one.
pub const QUEUE_LENGTH: usize = 1;
/// The priority `oracle/harness/main.c` starts this scenario at.
pub const PRIORITY: u8 = 1;

/// The two values the interrupt half writes.
const ISR_TX1: u64 = 10;
const ISR_TX2: u64 = 20;
/// How many cases the interrupt half cycles through.
const ISR_CASES: u32 = 3;

/// `QueueOverwrite.c`'s task-half file-scope variables.
///
/// `xISRTestStatus` is not here: it belongs to the interrupt half, which
/// lives in the kernel so that it can reach the kernel. [`State::still_running`]
/// takes it as an argument for exactly that reason.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `ulLoopCounter`.
    pub loops: u32,
}

impl State {
    /// `xIsQueueOverwriteTaskStillRunning`: the interrupt half must not have
    /// latched an error, and the task half must have gone round at least
    /// once since the last check.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        let running = isr.status && self.loops > 0;
        self.loops = 0;
        running
    }
}

/// `vQueueOverwritePeriodicISRDemo`: the half that runs in the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `ulCallCount`, the switch selector.
    call_count: u32,
    /// `xISRQueue`, created before the scheduler starts.
    queue: QueueHandle,
    /// `xISRTestStatus`, latched false on the first surprise.
    status: bool,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            call_count: 0,
            queue: QueueHandle::NULL,
            status: true,
        }
    }
}

impl Isr {
    /// One tick's worth of the C's `switch( ulCallCount )`.
    ///
    /// Unlike a task body this is not split across steps: an interrupt runs
    /// to completion, so the whole case runs in one call, exactly as the C
    /// function does inside `vApplicationTickHook`.
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        match self.call_count {
            // The queue is empty. Write ulTx1, then peek to check it landed.
            0 => {
                let _ = k.queue_overwrite_from_isr(self.queue, ISR_TX1);
                if k.queue_peek_from_isr(self.queue) != Ok(ISR_TX1) {
                    self.status = false;
                }
            }
            // The queue holds ulTx1. Overwrite it with ulTx2.
            1 => {
                let _ = k.queue_overwrite_from_isr(self.queue, ISR_TX2);
            }
            // Empty it again; what comes back must be the *second* value.
            2 => match k.queue_receive_from_isr(self.queue) {
                Ok((value, _woken)) => {
                    if value != ISR_TX2 {
                        self.status = false;
                    }
                }
                Err(_) => self.status = false,
            },
            _ => {}
        }
        self.call_count = self.call_count.wrapping_add(1);
        if self.call_count >= ISR_CASES {
            self.call_count = 0;
        }
        self
    }
}

/// `prvQueueOverwriteTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// The queue this task creates for itself, once it is running.
    queue: QueueHandle,
    /// `ulValue`.
    value: u64,
    /// `ulStatus`, latched on the first surprise and never reset.
    status: bool,
    /// `x`, the inner loop.
    x: u32,
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::QOverwrite(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self.pc {
            // xTaskQueue = xQueueCreate( 1, sizeof( uint32_t ) ); — once,
            // inside the task, after the scheduler has started.
            0 => {
                match k.queue_create(QUEUE_LENGTH) {
                    Ok(queue) => self.queue = queue,
                    Err(_) => self.status = false,
                }
                self.pc = 1;
            }
            // ulValue = 10; xQueueOverwrite( xTaskQueue, &ulValue );
            1 => {
                self.value = 10;
                let _ = k.queue_overwrite(self.queue, self.value);
                self.pc = 2;
            }
            // ulValue = 0; xQueueReceive( xTaskQueue, &ulValue, qoDONT_BLOCK );
            2 => {
                self.value = 0;
                if let Ok(Wait::Ready(value)) = k.queue_receive(self.queue, DONT_BLOCK) {
                    self.value = value;
                }
                self.pc = 3;
            }
            // if( ulValue != 10 ) { ulStatus = pdFAIL; }
            3 => {
                if self.value != 10 {
                    self.status = false;
                }
                self.x = 0;
                self.pc = 4;
            }
            // for( x = 0; x < qoLOOPS; x++ ) — xQueueOverwrite( ..., &x );
            4 => {
                if self.x < LOOPS {
                    let _ = k.queue_overwrite(self.queue, u64::from(self.x));
                    self.pc = 5;
                } else {
                    self.pc = 8;
                }
            }
            // xQueuePeek( xTaskQueue, &ulValue, qoDONT_BLOCK );
            // if( ulValue != x ) { ulStatus = pdFAIL; }
            5 => {
                if let Ok(Wait::Ready(value)) = k.queue_peek(self.queue, DONT_BLOCK) {
                    self.value = value;
                }
                self.pc = 6;
            }
            6 => {
                if self.value != u64::from(self.x) {
                    self.status = false;
                }
                self.pc = 7;
            }
            // if( uxQueueMessagesWaiting( xTaskQueue ) != uxQueueLength ) { ulStatus = pdFAIL; }
            7 => {
                if k.queue_messages_waiting(self.queue).unwrap_or(usize::MAX) != QUEUE_LENGTH {
                    self.status = false;
                }
                self.x = self.x.wrapping_add(1);
                self.pc = 4;
            }
            // xQueueReceive( xTaskQueue, &ulValue, qoDONT_BLOCK );
            8 => {
                if let Ok(Wait::Ready(value)) = k.queue_receive(self.queue, DONT_BLOCK) {
                    self.value = value;
                }
                self.pc = 9;
            }
            // if( uxQueueMessagesWaiting( xTaskQueue ) != 0 ) { ulStatus = pdFAIL; }
            9 => {
                if k.queue_messages_waiting(self.queue).unwrap_or(usize::MAX) != 0 {
                    self.status = false;
                }
                self.pc = 10;
            }
            // if( ulStatus != pdFAIL ) { ulLoopCounter++; }
            _ => {
                if self.status {
                    s.loops = s.loops.wrapping_add(1);
                }
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vStartQueueOverwriteTask`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (isr_queue, task) = {
        let k = runner.kernel_mut();
        // The queue the interrupt uses is created first, before the
        // scheduler; the task's own is created by the task.
        let isr_queue = k.queue_create(QUEUE_LENGTH)?;
        let task = k.create_task("QOver", PRIORITY)?;
        (isr_queue, task)
    };
    runner.shared_mut().state = runner::State::QOverwrite(State::default());
    *runner.kernel_mut().tick_hook_mut() = TickIsr::QueueOverwrite(Isr {
        queue: isr_queue,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        task,
        runner::Body::QOverwrite(Body {
            // `ulStatus = pdPASS` before the loop.
            status: true,
            ..Body::default()
        }),
    );
    Ok(())
}
