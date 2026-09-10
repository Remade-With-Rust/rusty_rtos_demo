//! `PollQ` — a queue polled from both ends, never blocking.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/PollQ.c`: a producer
//! posts three values with a zero block time and sleeps; a consumer drains
//! whatever is there, checking the values arrive in order, and sleeps for
//! slightly less. It is the scenario that proves `uxQueueMessagesWaiting`
//! and a zero-timeout send and receive agree about what is in the queue.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `pollqQUEUE_SIZE`.
pub const QUEUE_SIZE: usize = 10;
/// `pollqPRODUCER_DELAY`: `pdMS_TO_TICKS( 200 )` at 1000 Hz.
pub const PRODUCER_DELAY: u64 = 200;
/// `pollqCONSUMER_DELAY`: the producer's delay less 20 ms.
pub const CONSUMER_DELAY: u64 = PRODUCER_DELAY - 20;
/// `pollqNO_DELAY`.
pub const NO_DELAY: u64 = 0;
/// `pollqVALUES_TO_PRODUCE`.
pub const VALUES_TO_PRODUCE: u16 = 3;
/// The priority `oracle/harness/main.c` starts this scenario at.
pub const PRIORITY: u8 = 2;

/// `PollQ.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xPolledQueue`.
    pub queue: QueueHandle,
    /// `xPollingProducerCount`.
    pub producer_count: i32,
    /// `xPollingConsumerCount`.
    pub consumer_count: i32,
}

impl State {
    /// `xArePollingQueuesStillRunning`.
    pub fn still_running(&mut self) -> bool {
        let running = self.consumer_count != 0 && self.producer_count != 0;
        self.consumer_count = 0;
        self.producer_count = 0;
        running
    }
}

/// One of the scenario's two tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vPolledQueueProducer`.
    Producer(Producer),
    /// `vPolledQueueConsumer`.
    Consumer(Consumer),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::PollQ(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Producer(b) => b.step(k, state),
            Self::Consumer(b) => b.step(k, state),
        }
    }
}

/// `vPolledQueueProducer`: post three values, then sleep.
#[derive(Debug, Clone, Copy, Default)]
pub struct Producer {
    pc: u8,
    /// `usValue`.
    value: u16,
    /// `xError`.
    error: bool,
    /// `xLoop`.
    loops: u16,
}

impl Producer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // for( xLoop = 0; xLoop < pollqVALUES_TO_PRODUCE; xLoop++ )
            0 => {
                self.loops = 0;
                self.pc = 1;
            }
            // if( xQueueSend( ..., pollqNO_DELAY ) != pdPASS ) { xError = pdTRUE; }
            // else { if( !xError ) { critical: xPollingProducerCount++ } usValue++; }
            1 => {
                if matches!(
                    k.queue_send(s.queue, u64::from(self.value), NO_DELAY),
                    Ok(Wait::Ready(()))
                ) {
                    self.pc = 2;
                } else {
                    self.error = true;
                    self.pc = 4;
                }
            }
            2 => {
                if !self.error {
                    k.enter_critical();
                    s.producer_count = s.producer_count.wrapping_add(1);
                    k.exit_critical();
                }
                self.pc = 3;
            }
            3 => {
                self.value = self.value.wrapping_add(1);
                self.pc = 4;
            }
            // The loop test.
            4 => {
                self.loops = self.loops.saturating_add(1);
                self.pc = if self.loops < VALUES_TO_PRODUCE { 1 } else { 5 };
            }
            // vTaskDelay( pollqPRODUCER_DELAY );
            _ => {
                let _ = k.delay(PRODUCER_DELAY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vPolledQueueConsumer`: drain the queue, then sleep.
#[derive(Debug, Clone, Copy, Default)]
pub struct Consumer {
    pc: u8,
    /// `usExpectedValue`.
    expected: u16,
    /// `usData`.
    data: u16,
    /// `xError`.
    error: bool,
}

impl Consumer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // while( uxQueueMessagesWaiting( ... ) )
            0 => {
                let waiting = k.queue_messages_waiting(s.queue).unwrap_or(0);
                self.pc = if waiting > 0 { 1 } else { 5 };
            }
            // if( xQueueReceive( ..., pollqNO_DELAY ) == pdPASS )
            1 => {
                match k.queue_receive(s.queue, NO_DELAY) {
                    Ok(Wait::Ready(value)) => {
                        self.data = u16::try_from(value).unwrap_or(u16::MAX);
                        self.pc = 2;
                    }
                    // A zero block time cannot park the task, so anything
                    // else means the queue emptied under us; the C `if`
                    // simply falls through to the `while` test.
                    Ok(Wait::Blocked) | Err(_) => self.pc = 0,
                }
            }
            // if( usData != usExpectedValue ) { xError = pdTRUE; usExpectedValue = usData; }
            // else if( !xError ) { critical: xPollingConsumerCount++ }
            2 => {
                if self.data == self.expected {
                    if !self.error {
                        k.enter_critical();
                        s.consumer_count = s.consumer_count.wrapping_add(1);
                        k.exit_critical();
                    }
                } else {
                    self.error = true;
                    self.expected = self.data;
                }
                self.pc = 3;
            }
            // usExpectedValue++;
            3 | 4 => {
                self.expected = self.expected.wrapping_add(1);
                self.pc = 0;
            }
            // vTaskDelay( pollqCONSUMER_DELAY );
            _ => {
                let _ = k.delay(CONSUMER_DELAY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartPolledQueueTasks`, then the harness's own tasks.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (queue, consumer, producer) = {
        let mut k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_SIZE)?;
        // The C creates the consumer first, and the order is in the trace.
        let consumer = k.create_task("QConsNB", PRIORITY)?;
        let producer = k.create_task("QProdNB", PRIORITY)?;
        (queue, consumer, producer)
    };
    runner.shared_mut().state = runner::State::PollQ(State {
        queue,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        consumer,
        runner::Body::PollQ(Body::Consumer(Consumer::default())),
    );
    runner.attach(
        producer,
        runner::Body::PollQ(Body::Producer(Producer::default())),
    );
    Ok(())
}
