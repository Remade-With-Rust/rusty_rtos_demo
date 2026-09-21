//! `IntQueue` — two queues, six tasks and two interrupt handlers.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/IntQueue.c`, and until now
//! it was the one demo file in the corpus that **did not compile**: it
//! includes `IntQueueTimer.h`, which no demo directory supplies because every
//! BOARD project writes its own. `oracle/harness/IntQueueTimer.h` is this
//! harness's.
//!
//! # What is proven here, and what is not
//!
//! **Proven:** every queue access the demo makes — from two interrupt
//! handlers and from six tasks at three priorities, with suspend/resume
//! sequencing between them and a duplicate-and-missing-value audit over a
//! 200-entry log — trace-identical to the C kernel running the same way.
//!
//! **Not proven:** the property the demo was written for. Its own comment
//! says "the interrupts are prioritised such to ensure that nesting occurs",
//! and this port has one interrupt source and no nesting. The harness calls
//! `xFirstTimerHandler` then `xSecondTimerHandler` from the tick, in that
//! order, once per tick.
//!
//! Nothing in the C detects the difference — `xAreIntQueueTasksStillRunning`
//! checks only that all four counted tasks are cycling and that no access
//! logged an error — which is exactly why it has to be written down rather
//! than left for a reader to infer from a passing trace.
//!
//! # Where the file-scope statics live here
//!
//! The C keeps the queues, the two running values and the two 200-entry logs
//! as file-scope statics that tasks AND interrupts write. In this port the
//! interrupt half runs from the tick hook, which is handed the kernel and
//! nothing else — so everything both halves touch lives in [`Isr`], and the
//! tasks reach it through a short `tick_hook_mut()` borrow with no kernel
//! call inside it. That is the shape `intsem.rs` already uses.
//!
//! Copying the state out, calling the kernel and writing it back would be
//! the obvious alternative and it is **wrong**: a task that blocks inside
//! that window lets the tick run, and the write-back would then discard
//! whatever the interrupt did. [`State`] therefore holds only what the tasks
//! alone touch.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `intqHIGHER_PRIORITY`, `configMAX_PRIORITIES - 2`.
pub const HIGHER_PRIORITY: u8 = 5;
/// `intqLOWER_PRIORITY` (`tskIDLE_PRIORITY`).
pub const LOWER_PRIORITY: u8 = 0;
/// `intqHIGHER_PRIORITY + 1`: what the low-priority tasks raise themselves
/// to so they can preempt the high-priority ones.
const PREEMPTING_PRIORITY: u8 = HIGHER_PRIORITY.saturating_add(1);

/// `intqNUM_VALUES_TO_LOG`.
pub const NUM_VALUES_TO_LOG: usize = 200;
/// `intqSHORT_DELAY`.
const SHORT_DELAY: u64 = 140;
/// `intqVALUE_OVERRUN`.
const VALUE_OVERRUN: u64 = 50;
/// `intqONE_TICK_DELAY`.
const ONE_TICK_DELAY: u64 = 1;
/// `intqQUEUE_LENGTH`.
const QUEUE_LENGTH: usize = 10;
/// `intqMIN_ACCEPTABLE_TASK_COUNT`.
const MIN_ACCEPTABLE_TASK_COUNT: usize = 5;
/// The value the log audit compares against, as a `u64`.
const LOG_LIMIT: u64 = NUM_VALUES_TO_LOG as u64;

/// `intqHIGH_PRIORITY_TASK1`.
const HIGH_PRIORITY_TASK1: u8 = 1;
/// `intqHIGH_PRIORITY_TASK2`.
const HIGH_PRIORITY_TASK2: u8 = 2;
/// `intqLOW_PRIORITY_TASK`.
const LOW_PRIORITY_TASK: u8 = 3;
/// `intqSECOND_INTERRUPT`. `intqFIRST_INTERRUPT` is defined in the C and
/// never used as a source, so it is not modelled.
const SECOND_INTERRUPT: u8 = 5;

/// `uxTxed = 9999`, deliberately outside the logged range so it is never
/// recorded.
const LOW_PRIORITY_TX_VALUE: u64 = 9999;

/// The interrupt half, and every static both halves touch.
///
/// See the module docs for why the shared state lives here rather than in
/// [`State`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `uxNextOperation`, the static inside `xFirstTimerHandler`.
    first_operation: u32,
    /// `uxNextOperation`, the static inside `xSecondTimerHandler`.
    second_operation: u32,
    /// `xNormallyEmptyQueue`.
    pub normally_empty: QueueHandle,
    /// `xNormallyFullQueue`.
    pub normally_full: QueueHandle,
    /// `uxValueForNormallyEmptyQueue`.
    value_for_empty: u64,
    /// `uxValueForNormallyFullQueue`.
    value_for_full: u64,
    /// `ucNormallyEmptyReceivedValues`.
    empty_received: [u8; NUM_VALUES_TO_LOG],
    /// `ucNormallyFullReceivedValues`.
    full_received: [u8; NUM_VALUES_TO_LOG],
    /// `xErrorStatus`, which either half can fail.
    pub error_status: bool,
    /// `xErrorLine`. Kept because the C keeps it: a failing run that names
    /// a line is worth more than one that names none.
    pub error_line: u32,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            first_operation: 0,
            second_operation: 0,
            normally_empty: QueueHandle::NULL,
            normally_full: QueueHandle::NULL,
            value_for_empty: 0,
            value_for_full: 0,
            empty_received: [0; NUM_VALUES_TO_LOG],
            full_received: [0; NUM_VALUES_TO_LOG],
            error_status: true,
            error_line: 0,
        }
    }
}

impl Isr {
    /// `prvQueueAccessLogError`: latch the line, fail the run.
    fn log_error(&mut self, line: u32) {
        self.error_line = line;
        self.error_status = false;
    }

    /// `prvRecordValue_NormallyEmpty`.
    fn record_empty(&mut self, value: u64, source: u8) {
        if value >= LOG_LIMIT {
            return;
        }
        let Ok(index) = usize::try_from(value) else {
            return;
        };
        // "We don't expect to receive the same value twice."
        if self.empty_received.get(index).copied().unwrap_or(0) != 0 {
            self.log_error(line!());
        }
        if let Some(slot) = self.empty_received.get_mut(index) {
            *slot = source;
        }
    }

    /// `prvRecordValue_NormallyFull`.
    fn record_full(&mut self, value: u64, source: u8) {
        if value >= LOG_LIMIT {
            return;
        }
        let Ok(index) = usize::try_from(value) else {
            return;
        };
        if self.full_received.get(index).copied().unwrap_or(0) != 0 {
            self.log_error(line!());
        }
        if let Some(slot) = self.full_received.get_mut(index) {
            *slot = source;
        }
    }

    /// `timerNORMALLY_EMPTY_TX`.
    fn empty_tx<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some(queue) = with_isr(k, |i| i.normally_empty) else {
            return;
        };
        // The fullness test comes FIRST, and the increment is inside the
        // section the C opens only once it has passed.
        if k.queue_is_full_from_isr(queue).unwrap_or(true) {
            return;
        }
        let next = with_isr(k, |i| {
            i.value_for_empty = i.value_for_empty.saturating_add(1);
            i.value_for_empty
        })
        .unwrap_or(0);
        if k.queue_send_from_isr(queue, next).is_err() {
            // "if( xQueueSendFromISR(...) != pdPASS ) { uxValue--; }"
            with_isr(k, |i| {
                i.value_for_empty = i.value_for_empty.saturating_sub(1)
            });
        }
    }

    /// `timerNORMALLY_FULL_TX`.
    fn full_tx<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some(queue) = with_isr(k, |i| i.normally_full) else {
            return;
        };
        // The fullness test comes FIRST, and the increment is inside the
        // section the C opens only once it has passed.
        if k.queue_is_full_from_isr(queue).unwrap_or(true) {
            return;
        }
        let next = with_isr(k, |i| {
            i.value_for_full = i.value_for_full.saturating_add(1);
            i.value_for_full
        })
        .unwrap_or(0);
        if k.queue_send_from_isr(queue, next).is_err() {
            // "if( xQueueSendFromISR(...) != pdPASS ) { uxValue--; }"
            with_isr(k, |i| i.value_for_full = i.value_for_full.saturating_sub(1));
        }
    }

    /// `timerNORMALLY_EMPTY_RX`, which LOGS AN ERROR when nothing is there.
    fn empty_rx<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some(queue) = with_isr(k, |i| i.normally_empty) else {
            return;
        };
        match k.queue_receive_from_isr(queue) {
            Ok((value, _woken)) => {
                with_isr(k, |i| i.record_empty(value, SECOND_INTERRUPT));
            }
            Err(_) => {
                with_isr(k, |i| i.log_error(line!()));
            }
        }
    }

    /// `timerNORMALLY_FULL_RX`, which does NOT log an error when empty. The
    /// asymmetry with [`Isr::empty_rx`] is the C's, not a transcription slip.
    fn full_rx<W: fmt::Write>(k: &mut SimKernel<W>) {
        let Some(queue) = with_isr(k, |i| i.normally_full) else {
            return;
        };
        if let Ok((value, _woken)) = k.queue_receive_from_isr(queue) {
            with_isr(k, |i| i.record_full(value, SECOND_INTERRUPT));
        }
    }

    /// Both handlers, first then second — the order the C harness is wired
    /// to match.
    ///
    /// `taskENTER_CRITICAL_FROM_ISR` is deliberately NOT modelled as a
    /// charged section: on the Posix port `xPortSetInterruptMask` and
    /// `vPortClearInterruptMask` are both no-ops and neither touches the
    /// exit counter, so charging here would put a tick where the C has none.
    pub(crate) fn tick<W: fmt::Write>(self, k: &mut SimKernel<W>) -> Self {
        // xFirstTimerHandler
        let first = with_isr(k, |i| {
            i.first_operation = i.first_operation.wrapping_add(1);
            i.first_operation
        })
        .unwrap_or(0);
        if first & 1 == 1 {
            for _ in 0..3 {
                Self::empty_tx(k);
            }
        } else {
            for _ in 0..3 {
                Self::full_rx(k);
            }
        }

        // xSecondTimerHandler
        let second = with_isr(k, |i| {
            i.second_operation = i.second_operation.wrapping_add(1);
            i.second_operation
        })
        .unwrap_or(0);
        if second & 1 == 1 {
            Self::empty_tx(k);
            Self::empty_tx(k);
            Self::empty_rx(k);
            Self::empty_rx(k);
        } else {
            Self::full_rx(k);
            Self::full_tx(k);
            Self::full_tx(k);
            Self::full_tx(k);
        }

        // Every mutation went through the hook IN PLACE, so the copy this
        // was handed is stale by now; hand back what the hook holds.
        with_isr(k, |i| *i).unwrap_or(self)
    }
}

/// A short mutable borrow of the interrupt half's state.
///
/// The closure must not call the kernel — that is the whole point, and it is
/// enforced by the borrow: `k` is mutably borrowed for the duration.
fn with_isr<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> Option<R> {
    match k.tick_hook_mut() {
        TickIsr::IntQueue(isr) => Some(f(isr)),
        _ => None,
    }
}

/// `IntQueue.c`'s task-side state.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `xHighPriorityNormallyEmptyTask1` and `...2`.
    pub high_empty: [TaskHandle; 2],
    /// `xHighPriorityNormallyFullTask1` and `...2`.
    pub high_full: [TaskHandle; 2],
    /// `xWasSuspended`.
    was_suspended: bool,
    /// `uxHighPriorityLoops1`, `uxHighPriorityLoops2`,
    /// `uxLowPriorityLoops1`, `uxLowPriorityLoops2`.
    pub loops: [u32; 4],
    /// The four remembered counts inside `xAreIntQueueTasksStillRunning`.
    last_loops: [u32; 4],
    /// The task half's share of `xErrorStatus`.
    pub error_status: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            high_empty: [TaskHandle::NULL; 2],
            high_full: [TaskHandle::NULL; 2],
            was_suspended: false,
            loops: [0; 4],
            last_loops: [0; 4],
            error_status: true,
        }
    }
}

impl State {
    /// `xAreIntQueueTasksStillRunning`: all four counted tasks must be
    /// cycling, and neither half may have logged an error.
    ///
    /// As everywhere in this corpus the remembered count only advances when
    /// the demo IS moving, so a stall latches rather than clears.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        for index in 0..self.loops.len() {
            let now = self.loops.get(index).copied().unwrap_or(0);
            let last = self.last_loops.get(index).copied().unwrap_or(0);
            if now == last {
                self.error_status = false;
            }
            if let Some(slot) = self.last_loops.get_mut(index) {
                *slot = now;
            }
        }
        self.error_status && isr.error_status
    }
}

/// The scenario's six tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvHigherPriorityNormallyEmptyTask`, run by two tasks.
    HigherEmpty(HigherEmpty),
    /// `prvLowerPriorityNormallyEmptyTask`.
    LowerEmpty(LowerEmpty),
    /// `prv1stHigherPriorityNormallyFullTask`.
    FirstHigherFull(FirstHigherFull),
    /// `prv2ndHigherPriorityNormallyFullTask`.
    SecondHigherFull(SecondHigherFull),
    /// `prvLowerPriorityNormallyFullTask`.
    LowerFull(LowerFull),
}

impl Body {
    /// `#[inline(never)]`, as every body in this corpus is.
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::IntQueue(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::HigherEmpty(b) => b.step(k, state),
            Self::LowerEmpty(b) => b.step(k, state),
            Self::FirstHigherFull(b) => b.step(k, state),
            Self::SecondHigherFull(b) => b.step(k, state),
            Self::LowerFull(b) => b.step(k, state),
        }
    }
}

/// The two queue handles, read once per arm that needs them.
fn queues<W: fmt::Write>(k: &mut SimKernel<W>) -> (QueueHandle, QueueHandle) {
    with_isr(k, |i| (i.normally_empty, i.normally_full))
        .unwrap_or((QueueHandle::NULL, QueueHandle::NULL))
}

// ------------------------------------------ the normally-empty queue side --

/// `prvHigherPriorityNormallyEmptyTask`, both instances.
#[derive(Debug, Clone, Copy)]
pub struct HigherEmpty {
    pc: u16,
    /// The task's parameter: `intqHIGH_PRIORITY_TASK1` or `...TASK2`.
    which: u8,
    /// `uxErrorCount1`, `uxErrorCount2`.
    error_count: [u32; 2],
}

impl HigherEmpty {
    /// One of the two; `which` is the C's task parameter.
    #[must_use]
    pub fn new(which: u8) -> Self {
        Self {
            pc: 0,
            which,
            error_count: [0; 2],
        }
    }

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let (empty, _full) = queues(k);
        match self.pc {
            // if( xQueueReceive( xNormallyEmptyQueue, &uxRxed,
            //                    intqSHORT_DELAY ) != pdPASS )
            //
            // `vInitialiseTimerForIntQueueTest()` would be called once here
            // by task 1. It starts nothing on this port -- the tick hook
            // drives both handlers -- and takes no critical section on
            // either side, so it costs the clock nothing and is not an arm.
            0 => match k.queue_receive(empty, SHORT_DELAY) {
                Ok(Wait::Ready(value)) => {
                    with_isr(k, |i| i.record_empty(value, self.which));
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    with_isr(k, |i| i.log_error(line!()));
                    self.pc = 1;
                }
            },
            // taskYIELD();
            1 => {
                k.task_yield();
                self.pc = 2;
            }
            // if( pvParameters == intqHIGH_PRIORITY_TASK1 ) and
            // if( uxValueForNormallyEmptyQueue > ( NUM + OVERRUN ) )
            2 => {
                let value = with_isr(k, |i| i.value_for_empty).unwrap_or(0);
                let ready = self.which == HIGH_PRIORITY_TASK1
                    && value > LOG_LIMIT.saturating_add(VALUE_OVERRUN);
                self.pc = if ready { 3 } else { 0 };
            }
            // vTaskSuspend( xHighPriorityNormallyEmptyTask2 );
            3 => {
                if let Some(other) = s.high_empty.get(1).copied() {
                    let _ = k.suspend(Some(other));
                }
                self.pc = 4;
            }
            // The audit over the log, then the counters. No kernel call in
            // any of it, so it is one arm rather than two hundred.
            4 => {
                let counted = with_isr(k, |i| {
                    let (mut task1, mut task2, mut interrupts) = (0usize, 0usize, 0usize);
                    let mut missing = false;
                    // "Start at 1 as we expect position 0 to be unused."
                    for index in 1..NUM_VALUES_TO_LOG {
                        match i.empty_received.get(index).copied().unwrap_or(0) {
                            0 => missing = true,
                            v if v == HIGH_PRIORITY_TASK1 => task1 = task1.saturating_add(1),
                            v if v == HIGH_PRIORITY_TASK2 => task2 = task2.saturating_add(1),
                            v if v == SECOND_INTERRUPT => interrupts = interrupts.saturating_add(1),
                            _ => {}
                        }
                    }
                    if missing {
                        i.log_error(line!());
                    }
                    if interrupts == 0 {
                        i.log_error(line!());
                    }
                    // "Clear the array again, ready to start a new cycle."
                    i.empty_received = [0; NUM_VALUES_TO_LOG];
                    (task1, task2)
                });
                let (task1, task2) = counted.unwrap_or((0, 0));

                // Each task must have logged a few, but one lean cycle is
                // tolerated: the C only fails on the THIRD in a row.
                for (slot, count) in [(0usize, task1), (1usize, task2)] {
                    let seen = self.error_count.get(slot).copied().unwrap_or(0);
                    if count < MIN_ACCEPTABLE_TASK_COUNT {
                        let now = seen.saturating_add(1);
                        if let Some(cell) = self.error_count.get_mut(slot) {
                            *cell = now;
                        }
                        if now > 2 {
                            with_isr(k, |i| i.log_error(line!()));
                        }
                    } else if let Some(cell) = self.error_count.get_mut(slot) {
                        *cell = 0;
                    }
                }

                if let Some(cell) = s.loops.get_mut(0) {
                    *cell = cell.saturating_add(1);
                }
                self.pc = 5;
            }
            // portENTER_CRITICAL(); uxValueForNormallyEmptyQueue = 0;
            // portEXIT_CRITICAL();
            5 => {
                k.enter_critical();
                with_isr(k, |i| i.value_for_empty = 0);
                k.exit_critical();
                self.pc = 6;
            }
            // vTaskSuspend( NULL );
            //
            // "Suspend ourselves, allowing the lower priority task to
            // actually receive something from the queue."
            6 => {
                let _ = k.suspend(None);
                self.pc = 7;
            }
            // vTaskResume( xHighPriorityNormallyEmptyTask2 );
            _ => {
                if let Some(other) = s.high_empty.get(1).copied() {
                    let _ = k.resume(other);
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvLowerPriorityNormallyEmptyTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LowerEmpty {
    pc: u16,
    /// `uxValue`, carried between the critical section and the send.
    value: u64,
}

impl LowerEmpty {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let (empty, _full) = queues(k);
        match self.pc {
            // if( xQueueReceive( ..., intqONE_TICK_DELAY ) != errQUEUE_EMPTY )
            //
            // The C tests against errQUEUE_EMPTY, so the TAKEN branch is
            // "something arrived" and the else is "it timed out".
            0 => match k.queue_receive(empty, ONE_TICK_DELAY) {
                Ok(Wait::Ready(value)) => {
                    // "A value should only be obtained when the high
                    // priority task is suspended."
                    let first = s.high_empty.first().copied().unwrap_or(TaskHandle::NULL);
                    if k.task_state_get(first).unwrap_or(TaskState::Deleted) != TaskState::Suspended
                    {
                        with_isr(k, |i| i.log_error(line!()));
                    }
                    with_isr(k, |i| i.record_empty(value, LOW_PRIORITY_TASK));
                    self.value = value;
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                // Timed out: raise our priority and send instead.
                Err(_) => self.pc = 2,
            },
            // vTaskResume( xHighPriorityNormallyEmptyTask1 );
            1 => {
                if let Some(first) = s.high_empty.first().copied() {
                    let _ = k.resume(first);
                }
                if let Some(cell) = s.loops.get_mut(2) {
                    *cell = cell.saturating_add(1);
                }
                self.pc = 0;
            }
            // vTaskPrioritySet( NULL, intqHIGHER_PRIORITY + 1 );
            2 => {
                let _ = k.set_priority(None, PREEMPTING_PRIORITY);
                self.pc = 3;
            }
            // portENTER_CRITICAL(); uxValue = ++uxValueForNormallyEmptyQueue;
            // portEXIT_CRITICAL();
            3 => {
                k.enter_critical();
                self.value = with_isr(k, |i| {
                    i.value_for_empty = i.value_for_empty.saturating_add(1);
                    i.value_for_empty
                })
                .unwrap_or(0);
                k.exit_critical();
                self.pc = 4;
            }
            // xQueueSend( xNormallyEmptyQueue, &uxValue, portMAX_DELAY )
            4 => match k.queue_send(empty, self.value, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(())) => self.pc = 5,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    with_isr(k, |i| i.log_error(line!()));
                    self.pc = 5;
                }
            },
            // vTaskPrioritySet( NULL, intqLOWER_PRIORITY );
            _ => {
                let _ = k.set_priority(None, LOWER_PRIORITY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

// ------------------------------------------- the normally-full queue side --

/// `prv1stHigherPriorityNormallyFullTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FirstHigherFull {
    pc: u16,
    /// The priming loop's index, `ux < ( intqQUEUE_LENGTH >> 1 )`.
    primed: usize,
    /// `uxValueToTx`.
    value_to_tx: u64,
}

impl FirstHigherFull {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let (_empty, full) = queues(k);
        match self.pc {
            // The priming loop: "Make sure the queue starts full or near
            // full. >> 1 as there are two high priority tasks."
            0 => {
                self.pc = if self.primed < QUEUE_LENGTH >> 1 {
                    1
                } else {
                    3
                };
            }
            1 => {
                k.enter_critical();
                self.value_to_tx = with_isr(k, |i| {
                    i.value_for_full = i.value_for_full.saturating_add(1);
                    i.value_for_full
                })
                .unwrap_or(0);
                k.exit_critical();
                self.pc = 2;
            }
            // The priming send's result is discarded by the C.
            2 => match k.queue_send(full, self.value_to_tx, SHORT_DELAY) {
                Ok(Wait::Blocked) => {}
                Ok(Wait::Ready(())) | Err(_) => {
                    self.primed = self.primed.saturating_add(1);
                    self.pc = 0;
                }
            },
            // The forever loop's head.
            3 => {
                k.enter_critical();
                self.value_to_tx = with_isr(k, |i| {
                    i.value_for_full = i.value_for_full.saturating_add(1);
                    i.value_for_full
                })
                .unwrap_or(0);
                k.exit_critical();
                self.pc = 4;
            }
            4 => match k.queue_send(full, self.value_to_tx, SHORT_DELAY) {
                Ok(Wait::Ready(())) => self.pc = 5,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    // "TASK2 is never suspended so we would not expect it to
                    // ever time out."
                    with_isr(k, |i| i.log_error(line!()));
                    self.pc = 5;
                }
            },
            // taskYIELD();
            5 => {
                k.task_yield();
                self.pc = 6;
            }
            // if( uxValueToTx > ( NUM + OVERRUN ) )
            6 => {
                let ready = self.value_to_tx > LOG_LIMIT.saturating_add(VALUE_OVERRUN);
                self.pc = if ready { 7 } else { 3 };
            }
            // vTaskDelay( intqSHORT_DELAY );
            7 => {
                let _ = k.delay(SHORT_DELAY);
                self.pc = 8;
            }
            // vTaskSuspend( xHighPriorityNormallyFullTask2 );
            8 => {
                if let Some(other) = s.high_full.get(1).copied() {
                    let _ = k.suspend(Some(other));
                }
                self.pc = 9;
            }
            // The flag check and the log audit. No kernel call in any of it.
            9 => {
                if s.was_suspended {
                    // "We would have expected the other high priority task
                    // to have set this back to false by now."
                    with_isr(k, |i| i.log_error(line!()));
                }
                s.was_suspended = true;

                with_isr(k, |i| {
                    let mut interrupts = 0usize;
                    let mut missing = false;
                    for index in 1..NUM_VALUES_TO_LOG {
                        match i.full_received.get(index).copied().unwrap_or(0) {
                            0 => missing = true,
                            v if v == SECOND_INTERRUPT => interrupts = interrupts.saturating_add(1),
                            _ => {}
                        }
                    }
                    if missing {
                        i.log_error(line!());
                    }
                    if interrupts == 0 {
                        // "No writes from interrupts were found. Are
                        // interrupts actually running?"
                        i.log_error(line!());
                    }
                    i.full_received = [0; NUM_VALUES_TO_LOG];
                });

                if let Some(cell) = s.loops.get_mut(1) {
                    *cell = cell.saturating_add(1);
                }
                self.pc = 10;
            }
            10 => {
                k.enter_critical();
                with_isr(k, |i| i.value_for_full = 0);
                k.exit_critical();
                self.pc = 11;
            }
            // vTaskSuspend( NULL );
            11 => {
                let _ = k.suspend(None);
                self.pc = 12;
            }
            // vTaskResume( xHighPriorityNormallyFullTask2 );
            _ => {
                if let Some(other) = s.high_full.get(1).copied() {
                    let _ = k.resume(other);
                }
                self.pc = 3;
            }
        }
        Step::Continue
    }
}

/// `prv2ndHigherPriorityNormallyFullTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SecondHigherFull {
    pc: u16,
    primed: usize,
    value_to_tx: u64,
}

impl SecondHigherFull {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let (_empty, full) = queues(k);
        match self.pc {
            0 => {
                self.pc = if self.primed < QUEUE_LENGTH >> 1 {
                    1
                } else {
                    3
                };
            }
            1 => {
                k.enter_critical();
                self.value_to_tx = with_isr(k, |i| {
                    i.value_for_full = i.value_for_full.saturating_add(1);
                    i.value_for_full
                })
                .unwrap_or(0);
                k.exit_critical();
                self.pc = 2;
            }
            2 => match k.queue_send(full, self.value_to_tx, SHORT_DELAY) {
                Ok(Wait::Blocked) => {}
                Ok(Wait::Ready(())) | Err(_) => {
                    self.primed = self.primed.saturating_add(1);
                    self.pc = 0;
                }
            },
            3 => {
                k.enter_critical();
                self.value_to_tx = with_isr(k, |i| {
                    i.value_for_full = i.value_for_full.saturating_add(1);
                    i.value_for_full
                })
                .unwrap_or(0);
                k.exit_critical();
                self.pc = 4;
            }
            4 => match k.queue_send(full, self.value_to_tx, SHORT_DELAY) {
                Ok(Wait::Ready(())) => self.pc = 5,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    // "It is ok to time out if the task has been suspended."
                    if !s.was_suspended {
                        with_isr(k, |i| i.log_error(line!()));
                    }
                    self.pc = 5;
                }
            },
            // xWasSuspended = pdFALSE; taskYIELD();
            _ => {
                s.was_suspended = false;
                k.task_yield();
                self.pc = 3;
            }
        }
        Step::Continue
    }
}

/// `prvLowerPriorityNormallyFullTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LowerFull {
    pc: u16,
}

impl LowerFull {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let (_empty, full) = queues(k);
        match self.pc {
            // if( xQueueSend( ..., intqONE_TICK_DELAY ) != errQUEUE_FULL )
            //
            // As the empty side's receive: the TAKEN branch is "it went in".
            0 => match k.queue_send(full, LOW_PRIORITY_TX_VALUE, ONE_TICK_DELAY) {
                Ok(Wait::Ready(())) => {
                    // "Should only succeed when the higher priority task is
                    // suspended."
                    let first = s.high_full.first().copied().unwrap_or(TaskHandle::NULL);
                    if k.task_state_get(first).unwrap_or(TaskState::Deleted) != TaskState::Suspended
                    {
                        with_isr(k, |i| i.log_error(line!()));
                    }
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 2,
            },
            // vTaskResume( xHighPriorityNormallyFullTask1 );
            1 => {
                if let Some(first) = s.high_full.first().copied() {
                    let _ = k.resume(first);
                }
                if let Some(cell) = s.loops.get_mut(3) {
                    *cell = cell.saturating_add(1);
                }
                self.pc = 0;
            }
            // vTaskPrioritySet( NULL, intqHIGHER_PRIORITY + 1 );
            2 => {
                let _ = k.set_priority(None, PREEMPTING_PRIORITY);
                self.pc = 3;
            }
            // xQueueReceive( xNormallyFullQueue, &uxValue, portMAX_DELAY )
            3 => match k.queue_receive(full, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(value)) => {
                    with_isr(k, |i| i.record_full(value, LOW_PRIORITY_TASK));
                    self.pc = 4;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    with_isr(k, |i| i.log_error(line!()));
                    self.pc = 4;
                }
            },
            // vTaskPrioritySet( NULL, intqLOWER_PRIORITY );
            _ => {
                let _ = k.set_priority(None, LOWER_PRIORITY);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartInterruptQueueTasks`, in the C's order.
///
/// The order matters twice: it is the order the tasks appear in the trace,
/// and the two queues are created AFTER all six tasks — with the FULL queue
/// first — which is what decides the handles' identities.
///
/// # Errors
///
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (handles, isr) = {
        let mut k = runner.kernel_mut();
        let h1_rx = k.create_task("H1QRx", HIGHER_PRIORITY)?;
        let h2_rx = k.create_task("H2QRx", HIGHER_PRIORITY)?;
        let l1_rx = k.create_task("L1QRx", LOWER_PRIORITY)?;
        let h1_tx = k.create_task("H1QTx", HIGHER_PRIORITY)?;
        let h2_tx = k.create_task("H2QTx", HIGHER_PRIORITY)?;
        let l2_rx = k.create_task("L2QRx", LOWER_PRIORITY)?;

        // The C creates the FULL queue first.
        let normally_full = k.queue_create(QUEUE_LENGTH)?;
        let normally_empty = k.queue_create(QUEUE_LENGTH)?;

        (
            (h1_rx, h2_rx, l1_rx, h1_tx, h2_tx, l2_rx),
            Isr {
                normally_empty,
                normally_full,
                ..Isr::default()
            },
        )
    };
    let (h1_rx, h2_rx, l1_rx, h1_tx, h2_tx, l2_rx) = handles;

    runner.shared_mut().state = runner::State::IntQueue(State {
        high_empty: [h1_rx, h2_rx],
        high_full: [h1_tx, h2_tx],
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::IntQueue(isr);
    runner.start_common(max_ticks)?;

    runner.attach(
        h1_rx,
        runner::Body::IntQueue(Body::HigherEmpty(HigherEmpty::new(HIGH_PRIORITY_TASK1))),
    );
    runner.attach(
        h2_rx,
        runner::Body::IntQueue(Body::HigherEmpty(HigherEmpty::new(HIGH_PRIORITY_TASK2))),
    );
    runner.attach(
        l1_rx,
        runner::Body::IntQueue(Body::LowerEmpty(LowerEmpty::default())),
    );
    runner.attach(
        h1_tx,
        runner::Body::IntQueue(Body::FirstHigherFull(FirstHigherFull::default())),
    );
    runner.attach(
        h2_tx,
        runner::Body::IntQueue(Body::SecondHigherFull(SecondHigherFull::default())),
    );
    runner.attach(
        l2_rx,
        runner::Body::IntQueue(Body::LowerFull(LowerFull::default())),
    );
    Ok(())
}
