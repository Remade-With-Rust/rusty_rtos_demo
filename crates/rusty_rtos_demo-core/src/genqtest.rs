//! `GenQTest` — send-to-front, send-to-back, peek, and priority inheritance.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/GenQTest.c`, and it is
//! two tests in one file. One task fills a queue from both ends and checks
//! the order it reads back. Four more play out priority inheritance: a low
//! priority task takes a mutex, a high priority task blocks on it and lends
//! its priority, a medium priority task is released to prove it *cannot*
//! preempt the holder, and the holder disinherits only when the last mutex
//! goes back.
//!
//! The third part is the sharp one. `xTaskAbortDelay` drags the blocked
//! high priority task out of the Blocked state without the mutex, so the
//! holder has to disinherit down to whoever is still waiting rather than
//! all the way to its base priority. It is the only scenario in the corpus
//! that reaches that code at all.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `genqQUEUE_LENGTH`.
pub const QUEUE_LENGTH: usize = 5;
/// `genqQUEUE_LENGTH` where the C counts items rather than slots.
const QUEUE_ITEMS: u32 = 5;
/// `7 + genqQUEUE_LENGTH`, the end of the second read-back loop.
const SECOND_READ_END: u32 = 12;
/// `intsemNO_BLOCK`.
pub const NO_BLOCK: u64 = 0;
/// `genqSHORT_BLOCK`: `pdMS_TO_TICKS( 2 )` at 1000 Hz.
pub const SHORT_BLOCK: u64 = 2;

/// `genqMUTEX_LOW_PRIORITY`.
pub const MUTEX_LOW_PRIORITY: u8 = 0;
/// `genqMUTEX_TEST_PRIORITY`.
pub const MUTEX_TEST_PRIORITY: u8 = 1;
/// `genqMUTEX_MEDIUM_PRIORITY`.
pub const MUTEX_MEDIUM_PRIORITY: u8 = 2;
/// `genqMUTEX_HIGH_PRIORITY`.
pub const MUTEX_HIGH_PRIORITY: u8 = 3;
/// The priority `oracle/harness/main.c` starts this scenario at.
pub const PRIORITY: u8 = 0;

/// `GenQTest.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// The queue `prvSendFrontAndBackTest` fills from both ends.
    pub queue: QueueHandle,
    /// `xMutex`, shared by the three mutex tasks.
    pub mutex: QueueHandle,
    /// `xLocalMutex`, created by the low priority task once it is running.
    pub local_mutex: QueueHandle,
    /// `xMediumPriorityMutexTask`.
    pub medium: TaskHandle,
    /// `xHighPriorityMutexTask`.
    pub high: TaskHandle,
    /// `xSecondMediumPriorityMutexTask` — a second copy of the high
    /// priority task's body, created at the *medium* priority.
    pub second_medium: TaskHandle,
    /// `xErrorDetected`.
    pub error: bool,
    /// `ulLoopCounter`, the queue test's.
    pub loops: u32,
    /// `ulLoopCounter2`, the mutex test's.
    pub loops2: u32,
    /// `ulLastLoopCounter`, a static inside the check function.
    pub last_loops: u32,
    /// `ulLastLoopCounter2`, likewise.
    pub last_loops2: u32,
    /// `ulGuardedVariable`.
    pub guarded: u32,
    /// `xBlockWasAborted`.
    pub block_was_aborted: bool,
    /// `uxLoopCount`, a static inside `prvHighPriorityTimeout`.
    pub timeout_loops: u32,
}

impl State {
    /// `xAreGenericQueueTasksStillRunning`: both counters must have moved.
    pub fn still_running(&mut self) -> bool {
        if self.last_loops == self.loops {
            self.error = true;
        }
        if self.last_loops2 == self.loops2 {
            self.error = true;
        }
        self.last_loops = self.loops;
        self.last_loops2 = self.loops2;
        !self.error
    }
}

/// One of the scenario's five tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvSendFrontAndBackTest`.
    GenQ(GenQ),
    /// `prvLowPriorityMutexTask`.
    MuLow(MuLow),
    /// `prvMediumPriorityMutexTask`.
    MuMed(MuMed),
    /// `prvHighPriorityMutexTask`, run by two tasks at two priorities.
    MuHigh(MuHigh),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::GenQTest(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::GenQ(b) => b.step(k, state),
            Self::MuLow(b) => b.step(k, state),
            Self::MuMed(b) => b.step(k, state),
            Self::MuHigh(b) => b.step(k, state),
        }
    }
}

/// `prvSendFrontAndBackTest`: fill a queue from both ends, read it back in
/// the order the two ends imply, and peek every item before taking it.
#[derive(Debug, Clone, Copy, Default)]
pub struct GenQ {
    pc: u8,
    /// `ulData`.
    data: u32,
    /// `ulData2`.
    data2: u32,
    /// `ulLoopCounterSnapshot`.
    snapshot: u32,
}

impl GenQ {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // ulLoopCounterSnapshot = ulLoopCounter;
            // xQueueSendToFront( xQueue, &ulLoopCounterSnapshot, intsemNO_BLOCK );
            0 => {
                self.snapshot = s.loops;
                let _ = k.queue_send_to_front(s.queue, u64::from(self.snapshot), NO_BLOCK);
                self.pc = 1;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            1 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 2;
            }
            // if( xQueueReceive( xQueue, &ulData, intsemNO_BLOCK ) != pdPASS ) { error }
            2 => {
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => self.data = value as u32,
                    Ok(Wait::Blocked) | Err(_) => s.error = true,
                }
                self.pc = 3;
            }
            // if( ulLoopCounter != ulData ) { error }
            3 => {
                if s.loops != self.data {
                    s.error = true;
                }
                self.pc = 4;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 0 ) { error }
            4 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 5;
            }
            // ulLoopCounterSnapshot = ulLoopCounter;
            // xQueueSendToBack( xQueue, &ulLoopCounterSnapshot, intsemNO_BLOCK );
            5 => {
                self.snapshot = s.loops;
                let _ = k.queue_send(s.queue, u64::from(self.snapshot), NO_BLOCK);
                self.pc = 6;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 1 ) { error }
            6 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 1 {
                    s.error = true;
                }
                self.pc = 7;
            }
            // if( xQueueReceive( xQueue, &ulData, intsemNO_BLOCK ) != pdPASS ) { error }
            7 => {
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => self.data = value as u32,
                    Ok(Wait::Blocked) | Err(_) => s.error = true,
                }
                self.pc = 8;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 0 ) { error }
            8 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 9;
            }
            // if( ulLoopCounter != ulData ) { error }
            9 => {
                if s.loops != self.data {
                    s.error = true;
                }
                self.data = 2;
                self.pc = 10;
            }
            // for( ulData = 2; ulData < 5; ulData++ )
            //     { xQueueSendToBack( xQueue, &ulData, intsemNO_BLOCK ); }
            10 => {
                if self.data < 5 {
                    let _ = k.queue_send(s.queue, u64::from(self.data), NO_BLOCK);
                    self.data = self.data.wrapping_add(1);
                } else {
                    self.pc = 11;
                }
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 3 ) { error }
            11 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 3 {
                    s.error = true;
                }
                self.pc = 12;
            }
            // ulData = 1; xQueueSendToFront( xQueue, &ulData, intsemNO_BLOCK );
            12 => {
                self.data = 1;
                let _ = k.queue_send_to_front(s.queue, u64::from(self.data), NO_BLOCK);
                self.pc = 13;
            }
            // ulData = 0; xQueueSendToFront( xQueue, &ulData, intsemNO_BLOCK );
            13 => {
                self.data = 0;
                let _ = k.queue_send_to_front(s.queue, u64::from(self.data), NO_BLOCK);
                self.pc = 14;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 5 ) { error }
            14 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 5 {
                    s.error = true;
                }
                self.pc = 15;
            }
            // if( xQueueSendToFront( ..., intsemNO_BLOCK ) != errQUEUE_FULL ) { error }
            15 => {
                if !matches!(
                    k.queue_send_to_front(s.queue, u64::from(self.data), NO_BLOCK),
                    Err(rusty_rtos_core::error::Error::Full)
                ) {
                    s.error = true;
                }
                self.pc = 16;
            }
            // if( xQueueSendToBack( ..., intsemNO_BLOCK ) != errQUEUE_FULL ) { error }
            16 => {
                if !matches!(
                    k.queue_send(s.queue, u64::from(self.data), NO_BLOCK),
                    Err(rusty_rtos_core::error::Error::Full)
                ) {
                    s.error = true;
                }
                self.data = 0;
                self.pc = 17;
            }
            // for( ulData = 0; ulData < genqQUEUE_LENGTH; ulData++ )
            // if( xQueuePeek( xQueue, &ulData2, intsemNO_BLOCK ) != pdPASS ) { error }
            17 => {
                if self.data < QUEUE_ITEMS {
                    match k.queue_peek(s.queue, NO_BLOCK) {
                        Ok(Wait::Ready(value)) => self.data2 = value as u32,
                        Ok(Wait::Blocked) | Err(_) => s.error = true,
                    }
                    self.pc = 18;
                } else {
                    self.pc = 21;
                }
            }
            // if( ulData != ulData2 ) { error }
            18 => {
                if self.data != self.data2 {
                    s.error = true;
                }
                self.pc = 19;
            }
            // ulData2 = ~ulData2;
            // if( xQueueReceive( xQueue, &ulData2, intsemNO_BLOCK ) != pdPASS ) { error }
            19 => {
                self.data2 = !self.data2;
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => self.data2 = value as u32,
                    Ok(Wait::Blocked) | Err(_) => s.error = true,
                }
                self.pc = 20;
            }
            // if( ulData != ulData2 ) { error }  — then the loop step.
            20 => {
                if self.data != self.data2 {
                    s.error = true;
                }
                self.data = self.data.wrapping_add(1);
                self.pc = 17;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 0 ) { error }
            21 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 22;
            }
            // ulData = 10; if( xQueueSend( ... ) != pdPASS ) { error }
            22 => {
                self.data = 10;
                if !matches!(
                    k.queue_send(s.queue, u64::from(self.data), NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 23;
            }
            // ulData = 11; if( xQueueSend( ... ) != pdPASS ) { error }
            23 => {
                self.data = 11;
                if !matches!(
                    k.queue_send(s.queue, u64::from(self.data), NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 24;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 2 ) { error }
            24 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 2 {
                    s.error = true;
                }
                self.data = 9;
                self.pc = 25;
            }
            // for( ulData = 9; ulData >= 7; ulData-- )
            //     { if( xQueueSendToFront( ... ) != pdPASS ) { error } }
            //
            // `ulData` is unsigned, so the loop stops at 6 and leaves 6
            // behind for the two full-queue checks below to use.
            25 => {
                if self.data >= 7 {
                    if !matches!(
                        k.queue_send_to_front(s.queue, u64::from(self.data), NO_BLOCK),
                        Ok(Wait::Ready(()))
                    ) {
                        s.error = true;
                    }
                    self.data = self.data.wrapping_sub(1);
                } else {
                    self.pc = 26;
                }
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 5 ) { error }
            26 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 5 {
                    s.error = true;
                }
                self.pc = 27;
            }
            // if( xQueueSendToFront( ..., intsemNO_BLOCK ) != errQUEUE_FULL ) { error }
            27 => {
                if !matches!(
                    k.queue_send_to_front(s.queue, u64::from(self.data), NO_BLOCK),
                    Err(rusty_rtos_core::error::Error::Full)
                ) {
                    s.error = true;
                }
                self.pc = 28;
            }
            // if( xQueueSendToBack( ..., intsemNO_BLOCK ) != errQUEUE_FULL ) { error }
            28 => {
                if !matches!(
                    k.queue_send(s.queue, u64::from(self.data), NO_BLOCK),
                    Err(rusty_rtos_core::error::Error::Full)
                ) {
                    s.error = true;
                }
                self.data = 7;
                self.pc = 29;
            }
            // for( ulData = 7; ulData < ( 7 + genqQUEUE_LENGTH ); ulData++ )
            //     { if( xQueueReceive( ... ) != pdPASS ) { error } }
            29 => {
                if self.data < SECOND_READ_END {
                    match k.queue_receive(s.queue, NO_BLOCK) {
                        Ok(Wait::Ready(value)) => self.data2 = value as u32,
                        Ok(Wait::Blocked) | Err(_) => s.error = true,
                    }
                    self.pc = 30;
                } else {
                    self.pc = 31;
                }
            }
            // if( ulData != ulData2 ) { error }  — then the loop step.
            30 => {
                if self.data != self.data2 {
                    s.error = true;
                }
                self.data = self.data.wrapping_add(1);
                self.pc = 29;
            }
            // if( uxQueueMessagesWaiting( xQueue ) != 0 ) { error }
            31 => {
                if k.queue_messages_waiting(s.queue).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 32;
            }
            // ulLoopCounter++;
            _ => {
                s.loops = s.loops.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvLowPriorityMutexTask`: the mutex holder, and the whole of the three
/// helper functions it calls, flattened into one program counter.
///
/// The `pc` ranges name the C function each arm belongs to: 0 is the local
/// mutex's one-off creation, 1..=18 is
/// `prvTakeTwoMutexesReturnInDifferentOrder`, 20..=35
/// `prvTakeTwoMutexesReturnInSameOrder`, and 40..=73
/// `prvHighPriorityTimeout`.
#[derive(Debug, Clone, Copy, Default)]
pub struct MuLow {
    pc: u8,
}

impl MuLow {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // xLocalMutex = xSemaphoreCreateMutex(); — once, before the loop.
            0 => {
                match k.mutex_create() {
                    Ok(mutex) => s.local_mutex = mutex,
                    Err(_) => s.error = true,
                }
                self.pc = 1;
            }

            // ---------------- prvTakeTwoMutexesReturnInDifferentOrder ----
            // if( xSemaphoreTake( xMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            1 => {
                if !matches!(k.semaphore_take(s.mutex, NO_BLOCK), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 2;
            }
            // ulGuardedVariable = 0;
            2 => {
                s.guarded = 0;
                self.pc = 3;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY ) { error }
            3 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_LOW_PRIORITY {
                    s.error = true;
                }
                self.pc = 4;
            }
            // vTaskResume( xHighPriorityMutexTask );
            4 => {
                let _ = k.resume(s.high);
                self.pc = 5;
            }
            // configASSERT( eTaskGetState( xHighPriorityMutexTask ) == eBlocked );
            //
            // The assert is compiled in, so the call really happens and its
            // critical section is part of sim time.
            5 => {
                if k.task_state_get(s.high) != Ok(TaskState::Blocked) {
                    s.error = true;
                }
                self.pc = 6;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            6 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 7;
            }
            // vTaskPrioritySet( NULL, genqMUTEX_TEST_PRIORITY );
            7 => {
                let _ = k.set_priority(None, MUTEX_TEST_PRIORITY);
                self.pc = 8;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            8 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 9;
            }
            // vTaskResume( xMediumPriorityMutexTask );
            9 => {
                let _ = k.resume(s.medium);
                self.pc = 10;
            }
            // if( ulGuardedVariable != 0 ) { error }
            10 => {
                if s.guarded != 0 {
                    s.error = true;
                }
                self.pc = 11;
            }
            // if( xSemaphoreTake( xLocalMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            11 => {
                if !matches!(
                    k.semaphore_take(s.local_mutex, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 12;
            }
            // if( xSemaphoreGive( xMutex ) != pdPASS ) { error }
            12 => {
                if !matches!(k.semaphore_give(s.mutex), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 13;
            }
            // if( ulGuardedVariable != 0 ) { error }
            13 => {
                if s.guarded != 0 {
                    s.error = true;
                }
                self.pc = 14;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            14 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 15;
            }
            // if( xSemaphoreGive( xLocalMutex ) != pdPASS ) { error }
            15 => {
                if !matches!(k.semaphore_give(s.local_mutex), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 16;
            }
            // if( ulGuardedVariable != 1 ) { error }
            16 => {
                if s.guarded != 1 {
                    s.error = true;
                }
                self.pc = 17;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_TEST_PRIORITY ) { error }
            17 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_TEST_PRIORITY {
                    s.error = true;
                }
                self.pc = 18;
            }
            // vTaskPrioritySet( NULL, genqMUTEX_LOW_PRIORITY );
            18 => {
                let _ = k.set_priority(None, MUTEX_LOW_PRIORITY);
                self.pc = 19;
            }
            // ulLoopCounter2++;
            19 => {
                s.loops2 = s.loops2.wrapping_add(1);
                self.pc = 20;
            }

            // -------------------- prvTakeTwoMutexesReturnInSameOrder -----
            // if( xSemaphoreTake( xMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            20 => {
                if !matches!(k.semaphore_take(s.mutex, NO_BLOCK), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 21;
            }
            // ulGuardedVariable = 0;
            21 => {
                s.guarded = 0;
                self.pc = 22;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY ) { error }
            22 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_LOW_PRIORITY {
                    s.error = true;
                }
                self.pc = 23;
            }
            // vTaskResume( xHighPriorityMutexTask );
            23 => {
                let _ = k.resume(s.high);
                self.pc = 24;
            }
            // configASSERT( eTaskGetState( xHighPriorityMutexTask ) == eBlocked );
            24 => {
                if k.task_state_get(s.high) != Ok(TaskState::Blocked) {
                    s.error = true;
                }
                self.pc = 25;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            25 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 26;
            }
            // vTaskResume( xMediumPriorityMutexTask );
            26 => {
                let _ = k.resume(s.medium);
                self.pc = 27;
            }
            // if( ulGuardedVariable != 0 ) { error }
            27 => {
                if s.guarded != 0 {
                    s.error = true;
                }
                self.pc = 28;
            }
            // if( xSemaphoreTake( xLocalMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            28 => {
                if !matches!(
                    k.semaphore_take(s.local_mutex, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 29;
            }
            // if( xSemaphoreGive( xLocalMutex ) != pdPASS ) { error }
            29 => {
                if !matches!(k.semaphore_give(s.local_mutex), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 30;
            }
            // if( ulGuardedVariable != 0 ) { error }
            30 => {
                if s.guarded != 0 {
                    s.error = true;
                }
                self.pc = 31;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            31 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 32;
            }
            // if( xSemaphoreGive( xMutex ) != pdPASS ) { error }
            32 => {
                if !matches!(k.semaphore_give(s.mutex), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 33;
            }
            // if( ulGuardedVariable != 1 ) { error }
            33 => {
                if s.guarded != 1 {
                    s.error = true;
                }
                self.pc = 34;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY ) { error }
            34 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_LOW_PRIORITY {
                    s.error = true;
                }
                self.pc = 35;
            }
            // ulLoopCounter2++;
            35 => {
                s.loops2 = s.loops2.wrapping_add(1);
                self.pc = 40;
            }

            // ----------------------------- prvHighPriorityTimeout --------
            // if( xSemaphoreGetMutexHolder( xMutex ) != NULL ) { error }
            40 => {
                if k.mutex_holder(s.mutex) != Ok(TaskHandle::NULL) {
                    s.error = true;
                }
                self.pc = 41;
            }
            // if( xSemaphoreTake( xMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            41 => {
                if !matches!(k.semaphore_take(s.mutex, NO_BLOCK), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 42;
            }
            // if( xSemaphoreGetMutexHolder( xMutex ) != xTaskGetCurrentTaskHandle() ) { error }
            42 => {
                let me = k.current();
                if k.mutex_holder(s.mutex) != Ok(me) {
                    s.error = true;
                }
                self.pc = 43;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY ) { error }
            43 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_LOW_PRIORITY {
                    s.error = true;
                }
                self.pc = 44;
            }
            // vTaskResume( xHighPriorityMutexTask );
            44 => {
                let _ = k.resume(s.high);
                self.pc = 45;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            45 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 46;
            }
            // vTaskResume( xSecondMediumPriorityMutexTask );
            46 => {
                let _ = k.resume(s.second_medium);
                self.pc = 47;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            47 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 48;
            }
            // vTaskDelay( uxLoopCount & 0x07 );
            48 => {
                let _ = k.delay(u64::from(s.timeout_loops & 0x07));
                self.pc = 49;
            }
            // xBlockWasAborted = pdTRUE;
            49 => {
                s.block_was_aborted = true;
                self.pc = 50;
            }
            // if( xTaskAbortDelay( xHighPriorityMutexTask ) != pdPASS ) { error }
            50 => {
                if k.abort_delay(s.high) != Ok(true) {
                    s.error = true;
                }
                self.pc = 51;
            }
            // while( uxTaskPriorityGet( NULL ) != genqMUTEX_MEDIUM_PRIORITY )
            //     { vTaskDelay( genqSHORT_BLOCK ); }
            51 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) == MUTEX_MEDIUM_PRIORITY {
                    self.pc = 52;
                } else {
                    self.pc = 100;
                }
            }
            // vTaskDelay( genqSHORT_BLOCK ), the loop body — its own arm
            // because a tick released by the priority read above can switch
            // this task out *between* the two statements, and on the C side
            // the delay then does not run until the task is back.
            100 => {
                let _ = k.delay(SHORT_BLOCK);
                self.pc = 51;
            }
            // xBlockWasAborted = pdTRUE;
            52 => {
                s.block_was_aborted = true;
                self.pc = 53;
            }
            // if( xTaskAbortDelay( xSecondMediumPriorityMutexTask ) != pdPASS ) { error }
            53 => {
                if k.abort_delay(s.second_medium) != Ok(true) {
                    s.error = true;
                }
                self.pc = 54;
            }
            // while( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY )
            //     { vTaskDelay( genqSHORT_BLOCK ); }
            54 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) == MUTEX_LOW_PRIORITY {
                    self.pc = 55;
                } else {
                    self.pc = 101;
                }
            }
            // vTaskDelay( genqSHORT_BLOCK ), the loop body — its own arm
            // because a tick released by the priority read above can switch
            // this task out *between* the two statements, and on the C side
            // the delay then does not run until the task is back.
            101 => {
                let _ = k.delay(SHORT_BLOCK);
                self.pc = 54;
            }
            // if( xSemaphoreGetMutexHolderFromISR( xMutex ) != xTaskGetCurrentTaskHandle() ) { error }
            55 => {
                let me = k.current();
                if k.mutex_holder_from_isr(s.mutex) != Ok(me) {
                    s.error = true;
                }
                self.pc = 56;
            }
            // xSemaphoreGive( xMutex );
            56 => {
                let _ = k.semaphore_give(s.mutex);
                self.pc = 57;
            }
            // if( xSemaphoreGetMutexHolderFromISR( xMutex ) != NULL ) { error }
            57 => {
                if k.mutex_holder_from_isr(s.mutex) != Ok(TaskHandle::NULL) {
                    s.error = true;
                }
                self.pc = 58;
            }
            // if( xSemaphoreTake( xMutex, intsemNO_BLOCK ) != pdPASS ) { error }
            58 => {
                if !matches!(k.semaphore_take(s.mutex, NO_BLOCK), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 59;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY ) { error }
            59 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_LOW_PRIORITY {
                    s.error = true;
                }
                self.pc = 60;
            }
            // vTaskResume( xSecondMediumPriorityMutexTask );
            60 => {
                let _ = k.resume(s.second_medium);
                self.pc = 61;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_MEDIUM_PRIORITY ) { error }
            61 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_MEDIUM_PRIORITY {
                    s.error = true;
                }
                self.pc = 62;
            }
            // vTaskResume( xHighPriorityMutexTask );
            62 => {
                let _ = k.resume(s.high);
                self.pc = 63;
            }
            // if( uxTaskPriorityGet( NULL ) != genqMUTEX_HIGH_PRIORITY ) { error }
            63 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MUTEX_HIGH_PRIORITY {
                    s.error = true;
                }
                self.pc = 64;
            }
            // xBlockWasAborted = pdTRUE;
            64 => {
                s.block_was_aborted = true;
                self.pc = 65;
            }
            // if( xTaskAbortDelay( xHighPriorityMutexTask ) != pdPASS ) { error }
            65 => {
                if k.abort_delay(s.high) != Ok(true) {
                    s.error = true;
                }
                self.pc = 66;
            }
            // while( uxTaskPriorityGet( NULL ) != genqMUTEX_MEDIUM_PRIORITY )
            //     { vTaskDelay( genqSHORT_BLOCK ); }
            66 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) == MUTEX_MEDIUM_PRIORITY {
                    self.pc = 67;
                } else {
                    self.pc = 102;
                }
            }
            // vTaskDelay( genqSHORT_BLOCK ), the loop body — its own arm
            // because a tick released by the priority read above can switch
            // this task out *between* the two statements, and on the C side
            // the delay then does not run until the task is back.
            102 => {
                let _ = k.delay(SHORT_BLOCK);
                self.pc = 66;
            }
            // xBlockWasAborted = pdTRUE;
            67 => {
                s.block_was_aborted = true;
                self.pc = 68;
            }
            // if( xTaskAbortDelay( xSecondMediumPriorityMutexTask ) != pdPASS ) { error }
            68 => {
                if k.abort_delay(s.second_medium) != Ok(true) {
                    s.error = true;
                }
                self.pc = 69;
            }
            // while( uxTaskPriorityGet( NULL ) != genqMUTEX_LOW_PRIORITY )
            //     { vTaskDelay( genqSHORT_BLOCK ); }
            69 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) == MUTEX_LOW_PRIORITY {
                    self.pc = 70;
                } else {
                    self.pc = 103;
                }
            }
            // vTaskDelay( genqSHORT_BLOCK ), the loop body — its own arm
            // because a tick released by the priority read above can switch
            // this task out *between* the two statements, and on the C side
            // the delay then does not run until the task is back.
            103 => {
                let _ = k.delay(SHORT_BLOCK);
                self.pc = 69;
            }
            // xSemaphoreGive( xMutex );
            70 => {
                let _ = k.semaphore_give(s.mutex);
                self.pc = 71;
            }
            // uxLoopCount++; — and back to the top of the task's loop.
            _ => {
                s.timeout_loops = s.timeout_loops.wrapping_add(1);
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `prvMediumPriorityMutexTask`: suspend, then prove it ran by touching the
/// guarded variable.
#[derive(Debug, Clone, Copy, Default)]
pub struct MuMed {
    pc: u8,
}

impl MuMed {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspend( NULL );
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // ulGuardedVariable++;
            _ => {
                s.guarded = s.guarded.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvHighPriorityMutexTask`: suspend, block on the mutex, give it back —
/// unless the block was aborted, in which case failing to get it is the
/// expected result rather than an error.
#[derive(Debug, Clone, Copy, Default)]
pub struct MuHigh {
    pc: u8,
    /// Whether the take succeeded, so the C's `if`/`else` can be split.
    took: bool,
}

impl MuHigh {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspend( NULL );
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // if( xSemaphoreTake( xMutex, portMAX_DELAY ) != pdPASS )
            1 => match k.semaphore_take(s.mutex, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(())) => {
                    self.took = true;
                    self.pc = 2;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    self.took = false;
                    self.pc = 2;
                }
            },
            // The failure arm: not getting the mutex is only an error when
            // the wait was not aborted. The success arm gives it back.
            _ => {
                if self.took {
                    if !matches!(k.semaphore_give(s.mutex), Ok(Wait::Ready(()))) {
                        s.error = true;
                    }
                } else if s.block_was_aborted {
                    s.block_was_aborted = false;
                } else {
                    s.error = true;
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartGenericQueueTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (queue, mutex, genq, low, medium, high, second_medium) = {
        let k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let genq = k.create_task("GenQ", PRIORITY)?;
        let mutex = k.mutex_create()?;
        let low = k.create_task("MuLow", MUTEX_LOW_PRIORITY)?;
        let medium = k.create_task("MuMed", MUTEX_MEDIUM_PRIORITY)?;
        let high = k.create_task("MuHigh", MUTEX_HIGH_PRIORITY)?;
        // The second copy of the high priority body runs at the *medium*
        // priority; that mismatch is the point of the abort-delay test.
        let second_medium = k.create_task("MuHigh2", MUTEX_MEDIUM_PRIORITY)?;
        (queue, mutex, genq, low, medium, high, second_medium)
    };
    runner.shared_mut().state = runner::State::GenQTest(State {
        queue,
        mutex,
        medium,
        high,
        second_medium,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(genq, runner::Body::GenQTest(Body::GenQ(GenQ::default())));
    runner.attach(low, runner::Body::GenQTest(Body::MuLow(MuLow::default())));
    runner.attach(
        medium,
        runner::Body::GenQTest(Body::MuMed(MuMed::default())),
    );
    runner.attach(
        high,
        runner::Body::GenQTest(Body::MuHigh(MuHigh::default())),
    );
    runner.attach(
        second_medium,
        runner::Body::GenQTest(Body::MuHigh(MuHigh::default())),
    );
    Ok(())
}
