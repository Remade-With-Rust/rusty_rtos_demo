//! `QueueSet` — three queues in one set, written by a task and an interrupt.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/QueueSet.c`. `QueueSetPolling`
//! already covers the queue-set API; what this adds is **contention** between
//! three queues in one set, and the overwrite-into-a-set corner that no other
//! scenario reaches.
//!
//! # Why this was the last one blocked, and what unblocked it
//!
//! Upstream seeds its pseudo-random generator from the ADDRESS of one of the
//! sending task's own stack locals:
//!
//! ```c
//! prvSRand( ( size_t ) &ulTaskTxValue );
//! ```
//!
//! That is reproducible on the C side — two runs of one build are
//! byte-identical — and **unknowable to any second implementation**, because
//! the seed decides which of the three queues every single write goes to for
//! the rest of the run. It is also not stable across builds or machines, so a
//! checked-in trace would rot.
//!
//! `kairos oracle patch` now pins it to a constant, which is the edit
//! `TaskNotify.c` already carried for exactly the same defect. A constant
//! changes nothing the demo tests: which queue a given write picks is
//! arbitrary by design, and the test is that ALL THREE get used, which
//! `xAreQueueSetTasksStillRunning` checks directly.
//!
//! # Where the file-scope statics live
//!
//! As `intqueue.rs`: the interrupt half runs from the tick hook, which is
//! handed the kernel and nothing else, so everything both halves touch lives
//! in [`Isr`] and the tasks reach it through a short `tick_hook_mut()`
//! borrow. `prvCheckReceivedValue` is called from BOTH sides — the C puts a
//! critical section around the task's call for exactly that reason — so its
//! two expected-value statics are in there too.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `queuesetNUM_QUEUES_IN_SET`.
pub const NUM_QUEUES_IN_SET: usize = 3;
/// `queuesetQUEUE_LENGTH`.
const QUEUE_LENGTH: usize = 3;
/// `queuesetSHORT_DELAY`.
const SHORT_DELAY: u64 = 200;
/// `queuesetDONT_BLOCK`.
const DONT_BLOCK: u64 = 0;
/// `queuesetINITIAL_ISR_TX_VALUE`: the task sends below this, the interrupt
/// at or above it, which is how a received value says who sent it.
const INITIAL_ISR_TX_VALUE: u64 = 0xffff;
/// `ULONG_MAX`, which is 32-bit here because the values are `uint32_t`.
const ULONG_MAX: u64 = 0xffff_ffff;
/// `queuesetLOW_PRIORITY` (`tskIDLE_PRIORITY`).
pub const LOW_PRIORITY: u8 = 0;
/// `queuesetMEDIUM_PRIORITY`.
pub const MEDIUM_PRIORITY: u8 = 1;
/// `queuesetPRIORITY_CHANGE_LOOPS`.
const PRIORITY_CHANGE_LOOPS: u32 = (NUM_QUEUES_IN_SET * QUEUE_LENGTH) as u32 * 2;
/// `queuesetISR_TX_PERIOD`.
const ISR_TX_PERIOD: u32 = 100;
/// `queuesetTX_LOOP_DELAY`, `pdMS_TO_TICKS( 200 )` at 1 kHz.
const TX_LOOP_DELAY: u64 = 200;
/// `queuesetALLOWABLE_RX_DEVIATION`.
const ALLOWABLE_RX_DEVIATION: u64 = 3;
/// `queuesetIGNORED_BOUNDARY`.
const IGNORED_BOUNDARY: u64 = ALLOWABLE_RX_DEVIATION * 2;
/// The seed `kairos oracle patch` pins into `QueueSet.c`.
const RAND_SEED: u64 = 0x0dc0_ffee;

/// `eRelativePriorities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Relative {
    /// `eEqualPriority`.
    #[default]
    Equal,
    /// `eTxHigherPriority`.
    TxHigher,
    /// `eTxLowerPriority`.
    TxLower,
}

/// The interrupt half, and every static both halves touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `xQueues`.
    pub queues: [QueueHandle; NUM_QUEUES_IN_SET],
    /// `xQueueSet`.
    pub set: QueueHandle,
    /// `xQueueSetTasksStatus`.
    pub status: bool,
    /// `xSetupComplete`.
    pub setup_complete: bool,
    /// `ulISRTxValue`.
    pub isr_tx_value: u64,
    /// `ulExpectedReceivedFromTask`, a static inside `prvCheckReceivedValue`.
    expected_from_task: u64,
    /// `ulExpectedReceivedFromISR`, likewise.
    expected_from_isr: u64,
    /// `xQueueToWriteTo`, a static inside `prvSendToQueueInSetFromISR`.
    queue_to_write_to: usize,
    /// `ulCallCount`, a static inside `vQueueSetAccessQueueSetFromISR`.
    call_count: u32,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            queues: [QueueHandle::NULL; NUM_QUEUES_IN_SET],
            set: QueueHandle::NULL,
            status: true,
            setup_complete: false,
            isr_tx_value: INITIAL_ISR_TX_VALUE,
            expected_from_task: 0,
            expected_from_isr: INITIAL_ISR_TX_VALUE,
            queue_to_write_to: 0,
            call_count: 0,
        }
    }
}

impl Isr {
    /// `prvCheckReceivedValueWithinExpectedRange`.
    fn within_range(received: u64, expected: u64) -> bool {
        let gap = if received > expected {
            received.saturating_sub(expected)
        } else {
            expected.saturating_sub(received)
        };
        gap <= ALLOWABLE_RX_DEVIATION
    }

    /// `prvCheckReceivedValue`.
    ///
    /// Called from the task AND from the interrupt, which is why the C
    /// wraps the task's call in a critical section and why this lives here.
    /// The value is tested against a small RANGE rather than one number:
    /// the receiving interrupt can preempt the receiving task between its
    /// read and this check, so values arrive slightly out of order.
    fn check_received(&mut self, received: u64) {
        if received >= INITIAL_ISR_TX_VALUE {
            // Sent by the interrupt.
            let low = received.saturating_sub(INITIAL_ISR_TX_VALUE) < IGNORED_BOUNDARY;
            let high = ULONG_MAX.saturating_sub(received) <= IGNORED_BOUNDARY;
            if !low && !high && !Self::within_range(received, self.expected_from_isr) {
                self.status = false;
            }
            self.expected_from_isr = self.expected_from_isr.wrapping_add(1) & ULONG_MAX;
            if self.expected_from_isr == 0 {
                self.expected_from_isr = INITIAL_ISR_TX_VALUE;
            }
        } else {
            // Sent by the Tx task.
            let low = received < IGNORED_BOUNDARY;
            let high = INITIAL_ISR_TX_VALUE
                .saturating_sub(1)
                .saturating_sub(received)
                <= IGNORED_BOUNDARY;
            if !low && !high && !Self::within_range(received, self.expected_from_task) {
                self.status = false;
            }
            self.expected_from_task = self.expected_from_task.saturating_add(1);
            if self.expected_from_task >= INITIAL_ISR_TX_VALUE {
                self.expected_from_task = 0;
            }
        }
    }

    /// `prvReceiveFromQueueInSetFromISR`.
    fn receive_from_set<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some(set) = with_isr(k, |i| i.set) else {
            return;
        };
        let activated = match k.queue_select_from_set_from_isr(set) {
            Ok(Some(queue)) => queue,
            Ok(None) | Err(_) => return,
        };
        match k.queue_receive_from_isr(activated) {
            Ok((value, _woken)) => {
                with_isr(k, |i| i.check_received(value));
            }
            Err(_) => {
                // "Data should have been available as the handle was
                // returned from xQueueSelectFromSetFromISR()."
                with_isr(k, |i| i.status = false);
            }
        }
    }

    /// `prvSendToQueueInSetFromISR`.
    fn send_to_set<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some((queue, value)) = with_isr(k, |i| {
            (
                i.queues
                    .get(i.queue_to_write_to)
                    .copied()
                    .unwrap_or_default(),
                i.isr_tx_value,
            )
        }) else {
            return;
        };
        if k.queue_send_from_isr(queue, value).is_err() {
            return;
        }
        with_isr(k, |i| {
            i.isr_tx_value = i.isr_tx_value.wrapping_add(1) & ULONG_MAX;
            if i.isr_tx_value == 0 {
                i.isr_tx_value = INITIAL_ISR_TX_VALUE;
            }
            // "Use a different queue next time."
            i.queue_to_write_to = i.queue_to_write_to.saturating_add(1);
            if i.queue_to_write_to >= NUM_QUEUES_IN_SET {
                i.queue_to_write_to = 0;
            }
        });
    }

    /// `vQueueSetAccessQueueSetFromISR`, one call per tick.
    pub(crate) fn tick<W: fmt::Write>(self, k: &mut SimKernel<W>) -> Self {
        let ready = with_isr(k, |i| {
            if !i.setup_complete {
                return false;
            }
            i.call_count = i.call_count.wrapping_add(1);
            if i.call_count > ISR_TX_PERIOD {
                i.call_count = 0;
                true
            } else {
                false
            }
        })
        .unwrap_or(false);

        if ready {
            // "First attempt to read from the queue set. Then write to it."
            Self::receive_from_set(k);
            Self::send_to_set(k);
        }
        with_isr(k, |i| *i).unwrap_or(self)
    }
}

/// A short mutable borrow of the interrupt half's state. The closure must
/// not call the kernel; the borrow enforces it.
fn with_isr<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> Option<R> {
    match k.tick_hook_mut() {
        TickIsr::QueueSet(isr) => Some(f(isr)),
        _ => None,
    }
}

/// `QueueSet.c`'s task-side statics.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `ulQueueUsedCounter`.
    pub queue_used_counter: [u32; NUM_QUEUES_IN_SET],
    /// `ulCycleCounter`.
    pub cycle_counter: u32,
    /// `xQueueSetSendingTask`.
    pub sending: TaskHandle,
    /// `xQueueSetReceivingTask`.
    pub receiving: TaskHandle,
    /// `ulLastCycleCounter`, a static inside the check function.
    last_cycle_counter: u32,
    /// `ulLastQueueUsedCounter`, likewise.
    last_queue_used_counter: [u32; NUM_QUEUES_IN_SET],
    /// `ulLastISRTxValue`, likewise.
    last_isr_tx_value: u64,
}

impl State {
    /// `xAreQueueSetTasksStillRunning`, all four clauses.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        let mut pass = true;

        if self.last_cycle_counter == self.cycle_counter {
            // "Either one of the tasks is stalled or an error has been
            // detected."
            pass = false;
        }
        self.last_cycle_counter = self.cycle_counter;

        // "Ensure that all the queues in the set have been used." This is
        // the clause that makes the pinned seed safe: whichever queue the
        // generator picks, the test is that every one of the three is used.
        for index in 0..NUM_QUEUES_IN_SET {
            let now = self.queue_used_counter.get(index).copied().unwrap_or(0);
            let last = self
                .last_queue_used_counter
                .get(index)
                .copied()
                .unwrap_or(0);
            if last == now {
                pass = false;
            }
            if let Some(slot) = self.last_queue_used_counter.get_mut(index) {
                *slot = now;
            }
        }

        if !isr.status {
            pass = false;
        }

        // "Check that the ISR is still sending values to the queues too."
        if isr.isr_tx_value == self.last_isr_tx_value {
            pass = false;
        } else {
            self.last_isr_tx_value = isr.isr_tx_value;
        }

        pass
    }
}

/// The scenario's two tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvQueueSetSendingTask`.
    Sending(Sending),
    /// `prvQueueSetReceivingTask`.
    Receiving(Receiving),
}

impl Body {
    /// `#[inline(never)]`, as every body in this corpus is.
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::QueueSet(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Sending(b) => b.step(k, state),
            Self::Receiving(b) => b.step(k, state),
        }
    }
}

// -------------------------------------------------------- the Tx task --

/// `prvQueueSetSendingTask`.
#[derive(Debug, Clone, Copy)]
pub struct Sending {
    pc: u16,
    /// `ulTaskTxValue`.
    tx_value: u64,
    /// `uxNextRand`, the generator's state.
    next_rand: u64,
    /// The queue this iteration writes to.
    queue_in_use: QueueHandle,
    /// `ulLoops`, a static inside `prvChangeRelativePriorities`.
    loops: u32,
    /// `ePriorities`, likewise.
    priorities: Relative,
}

impl Default for Sending {
    fn default() -> Self {
        Self {
            pc: 0,
            tx_value: 0,
            // `prvSRand` runs before the loop, and the patched seed is a
            // constant rather than the address of a stack local.
            next_rand: RAND_SEED,
            queue_in_use: QueueHandle::NULL,
            loops: 0,
            priorities: Relative::Equal,
        }
    }
}

impl Sending {
    /// `prvRand`, on `size_t` — eight bytes on the machine the oracle runs
    /// on, so the multiply wraps at 64 bits and not 32.
    fn rand(&mut self) -> u64 {
        self.next_rand = self
            .next_rand
            .wrapping_mul(1_103_515_245)
            .wrapping_add(12345);
        (self.next_rand / 65536) % 32768
    }

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // uxQueueToWriteTo = prvRand() % queuesetNUM_QUEUES_IN_SET;
            // ( ulQueueUsedCounter[ uxQueueToWriteTo ] )++;
            //
            // No kernel call, so it shares an arm with the send below it.
            0 => {
                let queues = u64::try_from(NUM_QUEUES_IN_SET).unwrap_or(1).max(1);
                let index =
                    usize::try_from(self.rand().checked_rem(queues).unwrap_or(0)).unwrap_or(0);
                self.queue_in_use = with_isr(k, |i| i.queues.get(index).copied())
                    .flatten()
                    .unwrap_or_default();
                if let Some(slot) = s.queue_used_counter.get_mut(index) {
                    *slot = slot.saturating_add(1);
                }
                self.pc = 1;
            }
            // xQueueSendToBack( xQueueInUse, &ulTaskTxValue, portMAX_DELAY )
            1 => match k.queue_send(self.queue_in_use, self.tx_value, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(())) => {
                    self.tx_value = self.tx_value.saturating_add(1);
                    if self.tx_value == INITIAL_ISR_TX_VALUE {
                        self.tx_value = 0;
                    }
                    self.pc = 2;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    // "The send should always pass as an infinite block
                    // time was used."
                    with_isr(k, |i| i.status = false);
                    self.pc = 2;
                }
            },
            // prvChangeRelativePriorities(): the counter, and the branch.
            2 => {
                self.loops = self.loops.saturating_add(1);
                if self.loops < PRIORITY_CHANGE_LOOPS {
                    self.pc = 0;
                    return Step::Continue;
                }
                self.loops = 0;
                self.pc = match self.priorities {
                    Relative::Equal => 3,
                    Relative::TxHigher => 4,
                    Relative::TxLower => 6,
                };
            }
            // eEqualPriority: lower the Rx task so Tx is relatively higher.
            3 => {
                let _ = k.set_priority(Some(s.receiving), LOW_PRIORITY);
                self.priorities = Relative::TxHigher;
                self.pc = 0;
            }
            // eTxHigherPriority: swap them around. Two calls, two arms.
            4 => {
                let _ = k.set_priority(Some(s.receiving), MEDIUM_PRIORITY);
                self.pc = 5;
            }
            5 => {
                let _ = k.set_priority(Some(s.sending), LOW_PRIORITY);
                self.priorities = Relative::TxLower;
                self.pc = 0;
            }
            // eTxLowerPriority: equal again, then stand back so the idle
            // priority tasks get some time.
            6 => {
                let _ = k.set_priority(Some(s.sending), MEDIUM_PRIORITY);
                self.priorities = Relative::Equal;
                self.pc = 7;
            }
            _ => {
                let _ = k.delay(TX_LOOP_DELAY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

// -------------------------------------------------------- the Rx task --

/// Where each phase of the receiving task's state machine begins. The
/// numbers are spaced so each phase keeps the C's own order within itself.
const SETUP: u16 = 0;
const OVERWRITE_ONE: u16 = 100;
const OVERWRITE_TWO: u16 = 200;
const OVERWRITE_TWO_ISR: u16 = 300;
const RESUME_TX: u16 = 400;
const LOOP: u16 = 500;

/// `prvQueueSetReceivingTask`, including `prvSetupTest` and its three
/// overwrite sub-tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct Receiving {
    pc: u16,
    /// The index of whichever setup loop is running.
    index: usize,
    /// The queue the set just handed back.
    activated: QueueHandle,
    /// The value just read.
    received: u64,
    /// The two scratch queues the overwrite tests create.
    scratch: [QueueHandle; 2],
    /// `ulValueToSend` in the overwrite tests.
    value_to_send: u64,
    /// Whether this task is polling rather than blocking, which the C
    /// derives from its own priority each time round the loop.
    block_time: u64,
}

impl Receiving {
    /// `uxQueueMessagesWaiting( xQueueSet ) != expected` -> fail.
    fn expect_waiting<W: fmt::Write>(k: &mut SimKernel<W>, expected: usize) {
        let Some(set) = with_isr(k, |i| i.set) else {
            return;
        };
        let waiting = k.queue_messages_waiting(set).unwrap_or(usize::MAX);
        if waiting != expected {
            with_isr(k, |i| i.status = false);
        }
    }

    /// `xQueuePeek( xQueueSet, &xReceivedHandle, queuesetDONT_BLOCK )` and
    /// the handle comparison that always follows it.
    fn expect_peek<W: fmt::Write>(k: &mut SimKernel<W>, expected: QueueHandle) {
        let Some(set) = with_isr(k, |i| i.set) else {
            return;
        };
        let got = match k.queue_peek(set, DONT_BLOCK) {
            Ok(Wait::Ready(raw)) => QueueHandle::from_raw(u32::try_from(raw).unwrap_or(0)),
            Ok(Wait::Blocked) | Err(_) => QueueHandle::NULL,
        };
        if got != expected {
            with_isr(k, |i| i.status = false);
        }
    }

    /// `xQueueSelectFromSet( xQueueSet, queuesetDONT_BLOCK )` and its
    /// comparison.
    fn expect_select<W: fmt::Write>(k: &mut SimKernel<W>, expected: QueueHandle) -> QueueHandle {
        let Some(set) = with_isr(k, |i| i.set) else {
            return QueueHandle::NULL;
        };
        let got = match k.queue_select_from_set(set, DONT_BLOCK) {
            Ok(Wait::Ready(Some(queue))) => queue,
            Ok(Wait::Ready(None) | Wait::Blocked) | Err(_) => QueueHandle::NULL,
        };
        if got != expected {
            with_isr(k, |i| i.status = false);
        }
        got
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kernel call, in the C's order"
    )]
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // xQueueSet = xQueueCreateSet( NUM_QUEUES_IN_SET * QUEUE_LENGTH );
            SETUP => {
                if let Ok(set) = k.queue_create_set(NUM_QUEUES_IN_SET * QUEUE_LENGTH) {
                    with_isr(k, |i| i.set = set);
                }
                self.index = 0;
                self.pc = SETUP + 1;
            }
            // for( x = 0; x < NUM_QUEUES_IN_SET; x++ ): create the queue.
            1 => {
                if self.index >= NUM_QUEUES_IN_SET {
                    self.pc = SETUP + 4;
                    return Step::Continue;
                }
                if let Ok(queue) = k.queue_create(QUEUE_LENGTH) {
                    let index = self.index;
                    with_isr(k, |i| {
                        if let Some(slot) = i.queues.get_mut(index) {
                            *slot = queue;
                        }
                    });
                }
                self.pc = SETUP + 2;
            }
            // xQueueAddToSet( xQueues[ x ], xQueueSet ) != pdPASS -> fail
            2 => {
                let (queue, set) = with_isr(k, |i| {
                    (i.queues.get(self.index).copied().unwrap_or_default(), i.set)
                })
                .unwrap_or_default();
                if k.queue_add_to_set(queue, set) != Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = SETUP + 3;
            }
            // "The queue has now been added to the queue set and cannot be
            // added to another" -- adding it twice must FAIL.
            3 => {
                let (queue, set) = with_isr(k, |i| {
                    (i.queues.get(self.index).copied().unwrap_or_default(), i.set)
                })
                .unwrap_or_default();
                if k.queue_add_to_set(queue, set) == Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.index = self.index.saturating_add(1);
                self.pc = SETUP + 1;
            }
            // Removing a queue from a set it does not belong to must fail.
            // The C passes NULL as the set.
            4 => {
                let queue = with_isr(k, |i| i.queues.first().copied().unwrap_or_default())
                    .unwrap_or_default();
                if k.queue_remove_from_set(queue, QueueHandle::NULL) == Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = SETUP + 5;
            }
            // Removing it from the set it DOES belong to must pass.
            5 => {
                let (queue, set) = with_isr(k, |i| {
                    (i.queues.first().copied().unwrap_or_default(), i.set)
                })
                .unwrap_or_default();
                if k.queue_remove_from_set(queue, set) != Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = SETUP + 6;
            }
            // Put an item in the queue before trying to add it back.
            6 => {
                let queue = with_isr(k, |i| i.queues.first().copied().unwrap_or_default())
                    .unwrap_or_default();
                let _ = k.queue_send(queue, 0, DONT_BLOCK);
                self.pc = SETUP + 7;
            }
            // "Should not be able to add a non-empty queue to a set."
            7 => {
                let (queue, set) = with_isr(k, |i| {
                    (i.queues.first().copied().unwrap_or_default(), i.set)
                })
                .unwrap_or_default();
                if k.queue_add_to_set(queue, set) == Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = SETUP + 8;
            }
            // Empty it again, then add it back.
            8 => {
                let queue = with_isr(k, |i| i.queues.first().copied().unwrap_or_default())
                    .unwrap_or_default();
                let _ = k.queue_receive(queue, DONT_BLOCK);
                self.pc = SETUP + 9;
            }
            9 => {
                let (queue, set) = with_isr(k, |i| {
                    (i.queues.first().copied().unwrap_or_default(), i.set)
                })
                .unwrap_or_default();
                if k.queue_add_to_set(queue, set) != Ok(true) {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = SETUP + 10;
            }
            // "The task that sends to the queues is not running yet, so
            // attempting to read from the queue set should fail." 200 ticks
            // of nothing, deliberately.
            10 => {
                let Some(set) = with_isr(k, |i| i.set) else {
                    self.pc = OVERWRITE_ONE;
                    return Step::Continue;
                };
                match k.queue_select_from_set(set, SHORT_DELAY) {
                    Ok(Wait::Ready(got)) => {
                        if got.is_some() {
                            with_isr(k, |i| i.status = false);
                        }
                        self.value_to_send = 0;
                        self.pc = OVERWRITE_ONE;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.value_to_send = 0;
                        self.pc = OVERWRITE_ONE;
                    }
                }
            }

            // ---- prvTestQueueOverwriteWithQueueSet ----
            //
            // "a requirement in order to call xQueueOverwrite": a queue of
            // length one.
            OVERWRITE_ONE => {
                self.scratch[0] = k.queue_create(1).unwrap_or_default();
                self.pc = OVERWRITE_ONE + 1;
            }
            101 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_add_to_set(self.scratch[0], set);
                self.pc = OVERWRITE_ONE + 2;
            }
            102 => {
                let _ = k.queue_overwrite(self.scratch[0], self.value_to_send);
                self.pc = OVERWRITE_ONE + 3;
            }
            103 => {
                Self::expect_waiting(k, 1);
                self.pc = OVERWRITE_ONE + 4;
            }
            104 => {
                Self::expect_peek(k, self.scratch[0]);
                self.pc = OVERWRITE_ONE + 5;
            }
            // "Now overwrite the value and ensure the queue set state
            // doesn't change."
            105 => {
                self.value_to_send = self.value_to_send.saturating_add(1);
                let _ = k.queue_overwrite(self.scratch[0], self.value_to_send);
                self.pc = OVERWRITE_ONE + 6;
            }
            106 => {
                Self::expect_waiting(k, 1);
                self.pc = OVERWRITE_ONE + 7;
            }
            107 => {
                Self::expect_select(k, self.scratch[0]);
                self.pc = OVERWRITE_ONE + 8;
            }
            // "Also ensure the value received is the OVERWRITTEN value."
            108 => {
                let got = match k.queue_receive(self.scratch[0], DONT_BLOCK) {
                    Ok(Wait::Ready(value)) => value,
                    Ok(Wait::Blocked) | Err(_) => u64::MAX,
                };
                if got != self.value_to_send {
                    with_isr(k, |i| i.status = false);
                }
                self.pc = OVERWRITE_ONE + 9;
            }
            109 => {
                Self::expect_waiting(k, 0);
                self.pc = OVERWRITE_ONE + 10;
            }
            110 => {
                Self::expect_select(k, QueueHandle::NULL);
                self.pc = OVERWRITE_ONE + 11;
            }
            111 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_remove_from_set(self.scratch[0], set);
                self.pc = OVERWRITE_ONE + 12;
            }
            112 => {
                let _ = k.queue_delete(self.scratch[0]);
                self.index = 0;
                self.pc = OVERWRITE_TWO;
            }

            // ---- the two-queue overwrite tests ----
            //
            // `prvTestQueueOverwriteOnTwoQueuesInQueueSet` and its FromISR
            // twin are the same sequence with a different overwrite call,
            // so they share these arms and branch on the phase.
            OVERWRITE_TWO | OVERWRITE_TWO_ISR => {
                self.scratch[0] = k.queue_create(1).unwrap_or_default();
                self.pc = self.pc.saturating_add(1);
            }
            201 | 301 => {
                self.scratch[1] = k.queue_create(1).unwrap_or_default();
                self.pc = self.pc.saturating_add(1);
            }
            202 | 302 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_add_to_set(self.scratch[0], set);
                self.pc = self.pc.saturating_add(1);
            }
            203 | 303 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_add_to_set(self.scratch[1], set);
                self.pc = self.pc.saturating_add(1);
            }
            // The six overwrites, in the C's order: (h1,1) (h2,2) (h1,2)
            // (h2,1) (h1,2) (h2,1), each followed by a waiting check.
            204 | 304 => {
                self.overwrite(k, 0, 1);
                self.pc = self.pc.saturating_add(1);
            }
            205 | 305 => {
                Self::expect_waiting(k, 1);
                self.pc = self.pc.saturating_add(1);
            }
            206 | 306 => {
                Self::expect_peek(k, self.scratch[0]);
                self.pc = self.pc.saturating_add(1);
            }
            207 | 307 => {
                self.overwrite(k, 1, 2);
                self.pc = self.pc.saturating_add(1);
            }
            208 | 308 => {
                Self::expect_waiting(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            // "The head of the queue set should not have changed though."
            209 | 309 => {
                Self::expect_peek(k, self.scratch[0]);
                self.pc = self.pc.saturating_add(1);
            }
            210 | 310 => {
                self.overwrite(k, 0, 2);
                self.pc = self.pc.saturating_add(1);
            }
            211 | 311 => {
                Self::expect_waiting(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            212 | 312 => {
                self.overwrite(k, 1, 1);
                self.pc = self.pc.saturating_add(1);
            }
            213 | 313 => {
                Self::expect_waiting(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            214 | 314 => {
                self.overwrite(k, 0, 2);
                self.pc = self.pc.saturating_add(1);
            }
            215 | 315 => {
                Self::expect_waiting(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            216 | 316 => {
                self.overwrite(k, 1, 1);
                self.pc = self.pc.saturating_add(1);
            }
            217 | 317 => {
                Self::expect_waiting(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            // The first handle out of the set is the first one written.
            218 | 318 => {
                self.activated = Self::expect_select(k, self.scratch[0]);
                self.pc = self.pc.saturating_add(1);
            }
            219 | 319 => {
                Self::expect_waiting(k, 1);
                self.pc = self.pc.saturating_add(1);
            }
            // ...and it holds the OVERWRITTEN value, 2.
            220 | 320 => {
                self.expect_receive(k, 2);
                self.pc = self.pc.saturating_add(1);
            }
            221 | 321 => {
                self.activated = Self::expect_select(k, self.scratch[1]);
                self.pc = self.pc.saturating_add(1);
            }
            222 | 322 => {
                Self::expect_waiting(k, 0);
                self.pc = self.pc.saturating_add(1);
            }
            223 | 323 => {
                self.expect_receive(k, 1);
                self.pc = self.pc.saturating_add(1);
            }
            224 | 324 => {
                Self::expect_select(k, QueueHandle::NULL);
                self.pc = self.pc.saturating_add(1);
            }
            225 | 325 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_remove_from_set(self.scratch[0], set);
                self.pc = self.pc.saturating_add(1);
            }
            226 | 326 => {
                let set = with_isr(k, |i| i.set).unwrap_or_default();
                let _ = k.queue_remove_from_set(self.scratch[1], set);
                self.pc = self.pc.saturating_add(1);
            }
            227 | 327 => {
                let _ = k.queue_delete(self.scratch[0]);
                self.pc = self.pc.saturating_add(1);
            }
            228 => {
                let _ = k.queue_delete(self.scratch[1]);
                self.pc = OVERWRITE_TWO_ISR;
            }
            328 => {
                let _ = k.queue_delete(self.scratch[1]);
                self.pc = RESUME_TX;
            }

            // vTaskResume( xQueueSetSendingTask );
            RESUME_TX => {
                let _ = k.resume(s.sending);
                self.pc = RESUME_TX + 1;
            }
            // "Let the ISR access the queues also." No kernel call, so it
            // shares the arm that starts the loop.
            401 => {
                with_isr(k, |i| i.setup_complete = true);
                self.pc = LOOP;
            }

            // ---- the forever loop ----
            //
            // "For test coverage reasons, the block time is dependent on
            // the priority of this task - which changes during the test."
            LOOP => {
                let priority = k.task_priority_get(None).unwrap_or(MEDIUM_PRIORITY);
                self.block_time = if priority == LOW_PRIORITY {
                    0
                } else {
                    SimKernel::<W>::MAX_DELAY
                };
                self.pc = LOOP + 1;
            }
            // xActivatedQueue = xQueueSelectFromSet( xQueueSet, portMAX_DELAY );
            //
            // Note the C passes portMAX_DELAY here regardless of the block
            // time it just computed -- xBlockTime is only used to decide
            // whether a NULL return is an error.
            501 => {
                let Some(set) = with_isr(k, |i| i.set) else {
                    return Step::Continue;
                };
                match k.queue_select_from_set(set, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(Some(queue))) => {
                        self.activated = queue;
                        self.pc = LOOP + 2;
                    }
                    Ok(Wait::Ready(None)) => {
                        if self.block_time != 0 {
                            // "This should not happen as an infinite delay
                            // was used."
                            with_isr(k, |i| i.status = false);
                        }
                        self.pc = LOOP;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = LOOP,
                }
            }
            // "Reading from the queue should pass with a zero block time as
            // this task will only run when something has been posted."
            502 => {
                match k.queue_receive(self.activated, DONT_BLOCK) {
                    Ok(Wait::Ready(value)) => self.received = value,
                    Ok(Wait::Blocked) | Err(_) => {
                        with_isr(k, |i| i.status = false);
                    }
                }
                self.pc = LOOP + 3;
            }
            // taskENTER_CRITICAL(); prvCheckReceivedValue(); taskEXIT_CRITICAL();
            //
            // The section is the C's, and it is there because this function
            // is also called from the interrupt.
            _ => {
                k.enter_critical();
                let received = self.received;
                with_isr(k, |i| i.check_received(received));
                k.exit_critical();
                if with_isr(k, |i| i.status).unwrap_or(false) {
                    s.cycle_counter = s.cycle_counter.saturating_add(1);
                }
                self.pc = LOOP;
            }
        }
        Step::Continue
    }

    /// One of the six overwrites, taking the ISR variant in the third test.
    fn overwrite<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, which: usize, value: u64) {
        let queue = self.scratch.get(which).copied().unwrap_or_default();
        if self.pc >= OVERWRITE_TWO_ISR {
            let _ = k.queue_overwrite_from_isr(queue, value);
        } else {
            let _ = k.queue_overwrite(queue, value);
        }
    }

    /// `xQueueReceive( xReceivedHandle, &ulValueReceived, DONT_BLOCK )` and
    /// the comparison that follows it.
    fn expect_receive<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, expected: u64) {
        let got = match k.queue_receive(self.activated, DONT_BLOCK) {
            Ok(Wait::Ready(value)) => value,
            Ok(Wait::Blocked) | Err(_) => u64::MAX,
        };
        if got != expected {
            with_isr(k, |i| i.status = false);
        }
    }
}

/// `vStartQueueSetTasks`.
///
/// The sending task is created FIRST and then suspended before the scheduler
/// starts: it must not write to a queue before the receiving task has
/// created them, and the receiving task resumes it once it has.
///
/// # Errors
///
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (sending, receiving) = {
        let mut k = runner.kernel_mut();
        let sending = k.create_task("SetTx", MEDIUM_PRIORITY)?;
        let receiving = k.create_task("SetRx", MEDIUM_PRIORITY)?;
        k.suspend(Some(sending))?;
        (sending, receiving)
    };

    runner.shared_mut().state = runner::State::QueueSet(State {
        sending,
        receiving,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::QueueSet(Isr::default());
    runner.start_common(max_ticks)?;

    runner.attach(
        sending,
        runner::Body::QueueSet(Body::Sending(Sending::default())),
    );
    runner.attach(
        receiving,
        runner::Body::QueueSet(Body::Receiving(Receiving::default())),
    );
    Ok(())
}
