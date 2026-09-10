//! `PollQ`, written against the Rust face instead of the C-shaped one.
//!
//! This is the same scenario as [`crate::pollq`] — the same C original, the
//! same two tasks, the same order — with one difference: the queue is a
//! [`Queue<u16, N>`] that *moves* `u16`s rather than a `QueueHandle` that
//! copies `u64`s. It exists to answer one question with a number instead of
//! an argument: **what does the Rust face cost?**
//!
//! It is diffed against the *same C oracle trace* as `PollQ`, exits
//! included. If the two agree, the face costs nothing and every application
//! can have it. If they differ by a single critical-section exit, the face
//! costs sim time and the ledger has to say so.
//!
//! # What is different at the call site, and why it matters
//!
//! ```ignore
//! // The C-shaped face: a handle, a u64, and a promise you got it right.
//! k.queue_send(s.queue, u64::from(self.value), NO_DELAY)
//!
//! // The Rust face: the value moves, and a send that did not happen
//! // hands it back rather than leaving you to wonder.
//! s.queue.send(k, self.value, NO_DELAY)
//! ```
//!
//! The consumer is the sharper case. In the C-shaped arm a receive answers
//! a `u64` and the task narrows it with `u16::try_from(...).unwrap_or(...)`
//! — a conversion that cannot fail here but that the API forces the caller
//! to write, and that would silently clamp if the queue ever carried
//! something bigger. In the typed arm a receive answers a `u16` because
//! nothing else could have been sent.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_kernel::queue::Wait;
use rusty_rtos_kernel::typed::{Queue, Sent};

use crate::pollq::{CONSUMER_DELAY, NO_DELAY, PRIORITY, PRODUCER_DELAY, VALUES_TO_PRODUCE};
use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `pollqQUEUE_SIZE`, as a const parameter — the queue cannot be created
/// with one length and sent to with another.
pub const QUEUE_SIZE: usize = 10;

/// `PollQ.c`'s file-scope variables, with the queue owning its items.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `xPolledQueue`, typed.
    pub queue: Queue<u16, QUEUE_SIZE>,
    /// `xPollingProducerCount`.
    pub producer_count: i32,
    /// `xPollingConsumerCount`.
    pub consumer_count: i32,
    /// `xLastProducerCount`, the static in the check.
    last_producer: i32,
    /// `xLastConsumerCount`.
    last_consumer: i32,
    /// Whether either task has latched an error.
    pub status_ok: bool,
}

impl State {
    /// `xArePollingQueuesStillRunning`.
    pub fn still_running(&mut self) -> bool {
        let mut status = self.status_ok;
        if self.last_producer == self.producer_count || self.last_consumer == self.consumer_count {
            status = false;
        }
        self.last_producer = self.producer_count;
        self.last_consumer = self.consumer_count;
        status
    }
}

/// The scenario's two tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vPolledQueueProducer`.
    Producer(Producer),
    /// `vPolledQueueConsumer`.
    Consumer(Consumer),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::PollQTyped(state) = &mut s.state else {
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
            1 => {
                // The value moves in. `Sent` is `#[must_use]` and has no
                // variant that drops it, so the failure branch cannot be
                // written without acknowledging what happened to it.
                match s.queue.send(k, self.value, NO_DELAY) {
                    Sent::Ok => self.pc = 2,
                    Sent::Full(_) | Sent::Blocked(_) => {
                        self.error = true;
                        self.pc = 4;
                    }
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
                let waiting = s.queue.len(k).unwrap_or(0);
                self.pc = if waiting > 0 { 1 } else { 5 };
            }
            // if( xQueueReceive( ..., pollqNO_DELAY ) == pdPASS )
            1 => {
                // A `u16` comes out because nothing else could have gone
                // in: no `try_from`, no clamp, no widening to `u64` and
                // back. That is the whole difference at this line.
                match s.queue.receive(k, NO_DELAY) {
                    Ok(Wait::Ready(Some(value))) => {
                        self.data = value;
                        self.pc = 2;
                    }
                    // A zero block time cannot park the task, so anything
                    // else means the queue emptied under us; the C `if`
                    // simply falls through to the `while` test.
                    _ => self.pc = 0,
                }
            }
            // if( usData != usExpectedValue ) { ... } else if( !xError ) { ... }
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

/// `vStartPolledQueueTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (queue, consumer, producer) = {
        let k = runner.kernel_mut();
        let queue = Queue::<u16, QUEUE_SIZE>::create(k)?;
        // The C creates the consumer first, and the order is in the trace.
        let consumer = k.create_task("QConsNB", PRIORITY)?;
        let producer = k.create_task("QProdNB", PRIORITY)?;
        (queue, consumer, producer)
    };
    runner.shared_mut().state = runner::State::PollQTyped(State {
        queue,
        producer_count: 0,
        consumer_count: 0,
        last_producer: 0,
        last_consumer: 0,
        status_ok: true,
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        consumer,
        runner::Body::PollQTyped(Body::Consumer(Consumer::default())),
    );
    runner.attach(
        producer,
        runner::Body::PollQTyped(Body::Producer(Producer::default())),
    );
    Ok(())
}
