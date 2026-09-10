//! `QueueSetPolling` — a queue set polled by a task, written by an interrupt.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/QueueSetPolling.c`. One
//! queue is in a set of one; the lowest priority task in the system spins
//! on `xQueueSelectFromSet` with no block time, and every fifty-first tick
//! the interrupt puts one ascending value on the queue. The task must see
//! the values in order and never miss one.
//!
//! The point is the no-block poll. A set that only ever wakes a blocked
//! task would never exercise the path where an item arrives *between* two
//! polls, which is exactly what an interrupt writing to a member queue
//! does.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `setpollQUEUE_LENGTH`, the length of both the queue and the set.
pub const QUEUE_LENGTH: usize = 10;
/// `setpollDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;
/// `queuesetISR_TX_PERIOD`: the interrupt writes on the tick after this many.
pub const ISR_TX_PERIOD: u32 = 50;
/// The priority the C creates the task at: `tskIDLE_PRIORITY`.
pub const PRIORITY: u8 = 0;

/// `QueueSetPolling.c`'s task-half file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xQueue`, the one member of the set.
    pub queue: QueueHandle,
    /// `xQueueSet`.
    pub set: QueueHandle,
    /// `xQueueSetPollStatus`.
    pub status_ok: bool,
    /// `ulCycleCounter`.
    pub cycles: u32,
    /// `ulLastCycleCounter`, a static inside the check function.
    pub last_cycles: u32,
}

impl State {
    /// `xAreQueueSetPollTasksStillRunning`.
    pub fn still_running(&mut self) -> bool {
        if self.last_cycles == self.cycles {
            self.status_ok = false;
        }
        self.last_cycles = self.cycles;
        self.status_ok
    }
}

/// `vQueueSetPollingInterruptAccess`: the half that runs in the tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `ulCallCount`.
    call_count: u32,
    /// `ulValueToSend`, only advanced when the send succeeded.
    value_to_send: u64,
    /// `xQueue`.
    queue: QueueHandle,
}

impl Isr {
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        // "It is intended that this function is called from the tick hook
        // function, so each call is one tick period apart."
        self.call_count = self.call_count.wrapping_add(1);
        if self.call_count > ISR_TX_PERIOD {
            self.call_count = 0;
            if k.queue_send_from_isr(self.queue, self.value_to_send).is_ok() {
                self.value_to_send = self.value_to_send.wrapping_add(1);
            }
        }
        self
    }
}

/// `prvQueueSetReceivingTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// `xActivatedQueue`.
    activated: Option<QueueHandle>,
    /// `ulReceived`.
    received: u64,
    /// `ulExpected`.
    expected: u64,
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::QSetPoll(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self.pc {
            // xActivatedQueue = xQueueSelectFromSet( xQueueSet, setpollDONT_BLOCK );
            0 => {
                self.activated = match k.queue_select_from_set(s.set, DONT_BLOCK) {
                    Ok(Wait::Ready(queue)) => queue,
                    Ok(Wait::Blocked) | Err(_) => None,
                };
                // if( xActivatedQueue != NULL )
                self.pc = if self.activated.is_some() { 1 } else { 0 };
            }
            // if( xQueueReceive( xActivatedQueue, &ulReceived, setpollDONT_BLOCK ) != pdPASS )
            //     { xQueueSetPollStatus = pdFAIL; }
            1 => {
                let queue = self.activated.unwrap_or(QueueHandle::NULL);
                match k.queue_receive(queue, DONT_BLOCK) {
                    Ok(Wait::Ready(value)) => self.received = value,
                    Ok(Wait::Blocked) | Err(_) => s.status_ok = false,
                }
                self.pc = 2;
            }
            // if( ulReceived == ulExpected ) { ulExpected++; } else { fail }
            2 => {
                if self.received == self.expected {
                    self.expected = self.expected.wrapping_add(1);
                } else {
                    s.status_ok = false;
                }
                self.pc = 3;
            }
            // if( xQueueSetPollStatus == pdPASS ) { ulCycleCounter++; }
            _ => {
                if s.status_ok {
                    s.cycles = s.cycles.wrapping_add(1);
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartQueueSetPollingTask`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (queue, set, task) = {
        let k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let set = k.queue_create_set(QUEUE_LENGTH)?;
        let _ = k.queue_add_to_set(queue, set)?;
        let task = k.create_task("SetPoll", PRIORITY)?;
        (queue, set, task)
    };
    runner.shared_mut().state = runner::State::QSetPoll(State {
        queue,
        set,
        status_ok: true,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::QueueSetPolling(Isr {
        queue,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(task, runner::Body::QSetPoll(Body::default()));
    Ok(())
}
