//! `BlockQ` — blocking sends and receives, three task pairs.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/BlockQ.c`: three queues,
//! six tasks, and every combination of "one end blocks, the other does
//! not". It is the scenario that proves a task really is parked on a
//! queue's event list and really is woken by the other end, at the right
//! priority and in the right order — the first thing in the corpus that
//! exercises a block time at all.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.
//! A blocking call that returns `Blocked` leaves `pc` where it is, and the
//! body makes the same call again when the kernel next runs it.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `blckqNUM_TASK_SETS`.
pub const TASK_SETS: usize = 3;
/// The block time the blocking half of each pair uses: `pdMS_TO_TICKS( 1000 )`.
pub const BLOCK_TIME: u64 = 1000;
/// The non-blocking half.
pub const DONT_BLOCK: u64 = 0;
/// The priority `oracle/harness/main.c` starts this scenario at.
pub const PRIORITY: u8 = 2;

/// Which check variable a task bumps. The C hands each task a pointer;
/// here it is an array and an index, and the pairing is deliberately the
/// C's own — `QConsB3` bumps a *producer* counter, because that is what
/// `pxQueueParameters3` points at.
#[derive(Debug, Clone, Copy, Default)]
pub struct Slot {
    /// `true` for `sBlockingConsumerCount`, `false` for the producer array.
    consumer: bool,
    index: usize,
}

/// `BlockQ.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `sBlockingConsumerCount`.
    pub consumer_count: [i16; TASK_SETS],
    /// `sBlockingProducerCount`.
    pub producer_count: [i16; TASK_SETS],
    /// `sLastBlockingConsumerCount`, a static inside the check function.
    pub last_consumer: [i16; TASK_SETS],
    /// `sLastBlockingProducerCount`, likewise.
    pub last_producer: [i16; TASK_SETS],
}

impl State {
    fn bump(&mut self, slot: Slot) {
        let array = if slot.consumer {
            &mut self.consumer_count
        } else {
            &mut self.producer_count
        };
        if let Some(cell) = array.get_mut(slot.index) {
            *cell = cell.wrapping_add(1);
        }
    }

    /// `xAreBlockingQueuesStillRunning`: every one of the six counters must
    /// have moved since the last check.
    pub fn still_running(&mut self) -> bool {
        let mut running = true;
        for i in 0..TASK_SETS {
            let (now, last) = (self.consumer_count.get(i), self.last_consumer.get_mut(i));
            if let (Some(now), Some(last)) = (now.copied(), last) {
                if now == *last {
                    running = false;
                }
                *last = now;
            }
            let (now, last) = (self.producer_count.get(i), self.last_producer.get_mut(i));
            if let (Some(now), Some(last)) = (now.copied(), last) {
                if now == *last {
                    running = false;
                }
                *last = now;
            }
        }
        running
    }
}

/// One of the scenario's six tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vBlockingQueueProducer`.
    Producer(Producer),
    /// `vBlockingQueueConsumer`.
    Consumer(Consumer),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::BlockQ(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Producer(b) => b.step(k, state),
            Self::Consumer(b) => b.step(k, state),
        }
    }
}

/// `vBlockingQueueProducer`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Producer {
    pc: u8,
    /// `usValue`.
    value: u16,
    /// `sErrorEverOccurred`.
    error: bool,
    queue: QueueHandle,
    block_time: u64,
    slot: Slot,
}

impl Producer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xQueueSend( ..., xBlockTime ) != pdPASS ) { sErrorEverOccurred = pdTRUE; }
            0 => match k.queue_send(self.queue, u64::from(self.value), self.block_time) {
                Ok(Wait::Ready(())) => self.pc = 1,
                // Parked on the queue: the same call runs again when the
                // kernel next schedules this task.
                Ok(Wait::Blocked) => {}
                Err(_) => self.error = true,
            },
            // if( sErrorEverOccurred == pdFALSE ) { ( *psCheckVariable )++; }
            1 => {
                if !self.error {
                    s.bump(self.slot);
                }
                self.pc = 2;
            }
            // ++usValue;
            _ => {
                self.value = self.value.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vBlockingQueueConsumer`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Consumer {
    pc: u8,
    /// `usExpectedValue`.
    expected: u16,
    /// `usData`.
    data: u16,
    /// `sErrorEverOccurred`.
    error: bool,
    queue: QueueHandle,
    block_time: u64,
    slot: Slot,
}

impl Consumer {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xQueueReceive( ..., xBlockTime ) == pdPASS )
            0 => match k.queue_receive(self.queue, self.block_time) {
                Ok(Wait::Ready(value)) => {
                    self.data = u16::try_from(value).unwrap_or(u16::MAX);
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {}
            },
            // if( usData != usExpectedValue ) { usExpectedValue = usData; sError = pdTRUE; }
            // else { if( !sError ) { ( *psCheckVariable )++; } ++usExpectedValue; }
            _ => {
                if self.data == self.expected {
                    if !self.error {
                        s.bump(self.slot);
                    }
                    self.expected = self.expected.wrapping_add(1);
                } else {
                    self.expected = self.data;
                    self.error = true;
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartBlockingQueueTasks`, in the C's order — which is what puts the
/// three queues at ordinals `q1`, `q2`, `q3` in the trace.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    struct Made {
        task: rusty_rtos_core::handle::TaskHandle,
        body: Body,
    }
    let mut made: [Option<Made>; 6] = [None, None, None, None, None, None];
    {
        let k = runner.kernel_mut();

        // Set 1: a one-deep queue, blocking consumer at PRIORITY, polling
        // producer at idle priority.
        let q1 = k.queue_create(1)?;
        let cons_b1 = k.create_task("QConsB1", PRIORITY)?;
        let prod_b2 = k.create_task("QProdB2", 0)?;
        made[0] = Some(Made {
            task: cons_b1,
            body: Body::Consumer(Consumer {
                queue: q1,
                block_time: BLOCK_TIME,
                slot: Slot {
                    consumer: true,
                    index: 0,
                },
                ..Consumer::default()
            }),
        });
        made[1] = Some(Made {
            task: prod_b2,
            body: Body::Producer(Producer {
                queue: q1,
                block_time: DONT_BLOCK,
                slot: Slot {
                    consumer: false,
                    index: 0,
                },
                ..Producer::default()
            }),
        });

        // Set 2: the same, the other way round. Note the check variables
        // are crossed in the C, and are crossed here.
        let q2 = k.queue_create(1)?;
        let cons_b3 = k.create_task("QConsB3", 0)?;
        let prod_b4 = k.create_task("QProdB4", PRIORITY)?;
        made[2] = Some(Made {
            task: cons_b3,
            body: Body::Consumer(Consumer {
                queue: q2,
                block_time: DONT_BLOCK,
                slot: Slot {
                    consumer: false,
                    index: 1,
                },
                ..Consumer::default()
            }),
        });
        made[3] = Some(Made {
            task: prod_b4,
            body: Body::Producer(Producer {
                queue: q2,
                block_time: BLOCK_TIME,
                slot: Slot {
                    consumer: true,
                    index: 1,
                },
                ..Producer::default()
            }),
        });

        // Set 3: a five-deep queue, both ends blocking, both at idle
        // priority.
        let q3 = k.queue_create(5)?;
        let prod_b5 = k.create_task("QProdB5", 0)?;
        let cons_b6 = k.create_task("QConsB6", 0)?;
        made[4] = Some(Made {
            task: prod_b5,
            body: Body::Producer(Producer {
                queue: q3,
                block_time: BLOCK_TIME,
                slot: Slot {
                    consumer: false,
                    index: 2,
                },
                ..Producer::default()
            }),
        });
        made[5] = Some(Made {
            task: cons_b6,
            body: Body::Consumer(Consumer {
                queue: q3,
                block_time: BLOCK_TIME,
                slot: Slot {
                    consumer: true,
                    index: 2,
                },
                ..Consumer::default()
            }),
        });
    }
    runner.shared_mut().state = runner::State::BlockQ(State::default());
    runner.start_common(max_ticks)?;
    for slot in made.into_iter().flatten() {
        runner.attach(slot.task, runner::Body::BlockQ(slot.body));
    }
    Ok(())
}
