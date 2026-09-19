//! `recmutex` — a recursive mutex, and the priority inheritance around it.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/recmutex.c`. Three tasks
//! at three priorities share one recursive mutex: the controlling task
//! takes it ten times over and gives it back ten times over, then suspends
//! itself holding nothing; the blocking task waits for it almost for ever;
//! the polling task, the *lowest* priority of the three, grabs it with no
//! block time and then wakes the other two — at which point it must find
//! itself running at the controlling task's priority, because it now holds
//! what they are waiting for.
//!
//! That last assertion is the scenario: `uxTaskPriorityGet( NULL ) ==
//! recmuCONTROLLING_TASK_PRIORITY` while the polling task holds the mutex,
//! and back to its own the instant it gives it up. It is the sharpest test
//! of priority inheritance in the corpus, and the reason `recmutex` is in
//! the K1 set rather than deferred with the rest of the mutex work.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `recmuMAX_COUNT`: the recursive call depth.
pub const MAX_COUNT: usize = 10;
/// `recmuSHORT_DELAY`: `pdMS_TO_TICKS( 20 )` at 1000 Hz.
pub const SHORT_DELAY: u64 = 20;
/// `recmu15ms_DELAY`.
pub const DELAY_15MS: u64 = 15;
/// `recmuNO_DELAY`.
pub const NO_DELAY: u64 = 0;
/// `recmuCONTROLLING_TASK_PRIORITY`.
pub const CONTROLLING_PRIORITY: u8 = 2;
/// `recmuBLOCKING_TASK_PRIORITY`.
pub const BLOCKING_PRIORITY: u8 = 1;
/// `recmuPOLLING_TASK_PRIORITY`.
pub const POLLING_PRIORITY: u8 = 0;

/// `recmutex.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xMutex`.
    pub mutex: QueueHandle,
    /// `xControllingTaskHandle`.
    pub controlling: TaskHandle,
    /// `xBlockingTaskHandle`.
    pub blocking: TaskHandle,
    /// `xErrorOccurred`.
    pub error: bool,
    /// `xControllingIsSuspended`.
    pub controlling_suspended: bool,
    /// `xBlockingIsSuspended`.
    pub blocking_suspended: bool,
    /// `uxControllingCycles`.
    pub controlling_cycles: u32,
    /// `uxBlockingCycles`.
    pub blocking_cycles: u32,
    /// `uxPollingCycles`.
    pub polling_cycles: u32,
    /// The three `uxLast*Cycles` statics inside the check function.
    pub last: [u32; 3],
}

impl State {
    /// `xAreRecursiveMutexTasksStillRunning`. Note the C sets the error
    /// flag rather than only returning `pdFAIL`, so a stalled cycle
    /// poisons every later check too — kept, because that is the C.
    pub fn still_running(&mut self) -> bool {
        for (i, now) in [
            self.controlling_cycles,
            self.blocking_cycles,
            self.polling_cycles,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(last) = self.last.get_mut(i) {
                if *last == now {
                    self.error = true;
                } else {
                    *last = now;
                }
            }
        }
        !self.error
    }
}

/// One of the scenario's three tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvRecursiveMutexControllingTask`.
    Controlling(Controlling),
    /// `prvRecursiveMutexBlockingTask`.
    Blocking(Blocking),
    /// `prvRecursiveMutexPollingTask`.
    Polling(Polling),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::RecMutex(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Controlling(b) => b.step(k, s),
            Self::Blocking(b) => b.step(k, s),
            Self::Polling(b) => b.step(k, s),
        }
    }
}

/// `prvRecursiveMutexControllingTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Controlling {
    pc: u8,
    /// `ux`.
    ux: usize,
}

impl Controlling {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xSemaphoreGiveRecursive( xMutex ) == pdPASS ) { error }
            // — it does not hold the mutex yet.
            0 => {
                if k.mutex_give_recursive(s.mutex).is_ok() {
                    s.error = true;
                }
                self.ux = 0;
                self.pc = 1;
            }
            // if( xSemaphoreTakeRecursive( xMutex, recmu15ms_DELAY ) != pdPASS ) { error }
            1 => match k.mutex_take_recursive(s.mutex, DELAY_15MS) {
                Ok(Wait::Ready(())) => self.pc = 2,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 2;
                }
            },
            // vTaskDelay( recmuSHORT_DELAY );
            2 => {
                let _ = k.delay(SHORT_DELAY);
                self.ux = self.ux.saturating_add(1);
                self.pc = if self.ux < MAX_COUNT { 1 } else { 3 };
            }
            // The giving loop opens with the delay.
            3 => {
                self.ux = 0;
                self.pc = 4;
            }
            4 => {
                let _ = k.delay(SHORT_DELAY);
                self.pc = 5;
            }
            // if( xSemaphoreGiveRecursive( xMutex ) != pdPASS ) { error }
            5 => {
                if k.mutex_give_recursive(s.mutex).is_err() {
                    s.error = true;
                }
                self.ux = self.ux.saturating_add(1);
                self.pc = if self.ux < MAX_COUNT { 4 } else { 6 };
            }
            // if( xSemaphoreGiveRecursive( xMutex ) == pdPASS ) { error }
            // — the depth is back to zero, so this one must fail.
            6 => {
                if k.mutex_give_recursive(s.mutex).is_ok() {
                    s.error = true;
                }
                self.pc = 7;
            }
            // uxControllingCycles++; xControllingIsSuspended = pdTRUE;
            7 => {
                s.controlling_cycles = s.controlling_cycles.wrapping_add(1);
                s.controlling_suspended = true;
                self.pc = 8;
            }
            // vTaskSuspend( NULL );
            8 => {
                let _ = k.suspend(None);
                self.pc = 9;
            }
            // xControllingIsSuspended = pdFALSE;
            _ => {
                s.controlling_suspended = false;
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvRecursiveMutexBlockingTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Blocking {
    pc: u8,
    /// Whether the take succeeded, so the C's `if`/`else` can be split.
    took: bool,
}

impl Blocking {
    /// The C block time: `portMAX_DELAY - 1`, which is very long but
    /// finite, so the task lands on a delayed list rather than the
    /// suspended one.
    const BLOCK_TIME: u64 = u64::MAX - 1;

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xSemaphoreTakeRecursive( xMutex, portMAX_DELAY - 1 ) == pdPASS )
            0 => match k.mutex_take_recursive(s.mutex, Self::BLOCK_TIME) {
                Ok(Wait::Ready(())) => {
                    self.took = true;
                    self.pc = 1;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    self.took = false;
                    s.error = true;
                    self.pc = 5;
                }
            },
            // if( xControllingIsSuspended != pdTRUE ) { error }
            1 => {
                if s.controlling_suspended {
                    self.pc = 2;
                } else {
                    s.error = true;
                    self.pc = 5;
                }
            }
            // if( xSemaphoreGiveRecursive( xMutex ) != pdPASS ) { error }
            2 => {
                if k.mutex_give_recursive(s.mutex).is_err() {
                    s.error = true;
                }
                s.blocking_suspended = true;
                self.pc = 3;
            }
            // vTaskSuspend( NULL );
            3 => {
                let _ = k.suspend(None);
                self.pc = 4;
            }
            // xBlockingIsSuspended = pdFALSE;
            4 => {
                s.blocking_suspended = false;
                self.pc = 5;
            }
            // if( uxControllingCycles != ( uxBlockingCycles + 1 ) ) { error }
            // uxBlockingCycles++;
            _ => {
                if s.controlling_cycles != s.blocking_cycles.wrapping_add(1) {
                    s.error = true;
                }
                s.blocking_cycles = s.blocking_cycles.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvRecursiveMutexPollingTask` — the lowest priority of the three, and
/// the one whose priority the mutex lifts.
#[derive(Debug, Clone, Copy, Default)]
pub struct Polling {
    pc: u8,
}

impl Polling {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( xSemaphoreTakeRecursive( xMutex, recmuNO_DELAY ) == pdPASS )
            0 => match k.mutex_take_recursive(s.mutex, NO_DELAY) {
                Ok(Wait::Ready(())) => self.pc = 1,
                // A zero block time never parks; anything else means the
                // mutex was held, and the C simply loops.
                Ok(Wait::Blocked) | Err(_) => {}
            },
            // configASSERT( eTaskGetState( xControllingTaskHandle ) == eSuspended );
            1 => {
                if k.task_state_get(s.controlling) != Ok(TaskState::Suspended) {
                    s.error = true;
                }
                self.pc = 2;
            }
            // configASSERT( eTaskGetState( xBlockingTaskHandle ) == eSuspended );
            2 => {
                if k.task_state_get(s.blocking) != Ok(TaskState::Suspended) {
                    s.error = true;
                }
                self.pc = 3;
            }
            // if( ( !xBlockingIsSuspended ) || ( !xControllingIsSuspended ) ) { error }
            3 => {
                if s.blocking_suspended && s.controlling_suspended {
                    self.pc = 4;
                } else {
                    s.error = true;
                    self.pc = 0;
                }
            }
            // uxPollingCycles++;
            4 => {
                s.polling_cycles = s.polling_cycles.wrapping_add(1);
                self.pc = 5;
            }
            // vTaskResume( xBlockingTaskHandle );
            5 => {
                let _ = k.resume(s.blocking);
                self.pc = 6;
            }
            // vTaskResume( xControllingTaskHandle );
            6 => {
                let _ = k.resume(s.controlling);
                self.pc = 7;
            }
            // if( xBlockingIsSuspended || xControllingIsSuspended ) { error }
            7 => {
                if s.blocking_suspended || s.controlling_suspended {
                    s.error = true;
                }
                self.pc = 8;
            }
            // configASSERT( uxTaskPriorityGet( NULL ) == recmuCONTROLLING_TASK_PRIORITY );
            // — this task now holds what a priority-2 task wants.
            8 => {
                if k.task_priority_get(None) != Ok(CONTROLLING_PRIORITY) {
                    s.error = true;
                }
                self.pc = 9;
            }
            // configASSERT( eTaskGetState( xControllingTaskHandle ) == eBlocked );
            9 => {
                if k.task_state_get(s.controlling) != Ok(TaskState::Blocked) {
                    s.error = true;
                }
                self.pc = 10;
            }
            // configASSERT( eTaskGetState( xBlockingTaskHandle ) == eBlocked );
            10 => {
                if k.task_state_get(s.blocking) != Ok(TaskState::Blocked) {
                    s.error = true;
                }
                self.pc = 11;
            }
            // if( xSemaphoreGiveRecursive( xMutex ) != pdPASS ) { error }
            11 => {
                if k.mutex_give_recursive(s.mutex).is_err() {
                    s.error = true;
                }
                self.pc = 12;
            }
            // configASSERT( uxTaskPriorityGet( NULL ) == recmuPOLLING_TASK_PRIORITY );
            // — and back down again, the instant it lets go.
            _ => {
                if k.task_priority_get(None) != Ok(POLLING_PRIORITY) {
                    s.error = true;
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartRecursiveMutexTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (mutex, controlling, blocking, polling) = {
        let mut k = runner.kernel_mut();
        let mutex = k.mutex_create_recursive()?;
        let controlling = k.create_task("Rec1", CONTROLLING_PRIORITY)?;
        let blocking = k.create_task("Rec2", BLOCKING_PRIORITY)?;
        let polling = k.create_task("Rec3", POLLING_PRIORITY)?;
        (mutex, controlling, blocking, polling)
    };
    runner.shared_mut().state = runner::State::RecMutex(State {
        mutex,
        controlling,
        blocking,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        controlling,
        runner::Body::RecMutex(Body::Controlling(Controlling::default())),
    );
    runner.attach(
        blocking,
        runner::Body::RecMutex(Body::Blocking(Blocking::default())),
    );
    runner.attach(
        polling,
        runner::Body::RecMutex(Body::Polling(Polling::default())),
    );
    Ok(())
}
