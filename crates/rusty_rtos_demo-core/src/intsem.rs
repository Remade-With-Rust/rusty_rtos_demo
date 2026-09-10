//! `IntSemTest` — semaphores given from an interrupt, mutexes shared with one.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/IntSemTest.c`. A master
//! task takes a mutex it shares with a higher priority slave, inherits the
//! slave's priority when the slave blocks on it, and then takes a *second*
//! mutex that only the tick interrupt ever gives. Holding two, it gives them
//! back in each order in turn, and checks that it keeps the inherited
//! priority until the last one goes.
//!
//! A third task proves a counting semaphore can be filled from an interrupt
//! to exactly its maximum and then drained.
//!
//! Two things here are unusual enough to be the point of the scenario: a
//! *mutex* given from an interrupt (which the C allows and this does too,
//! disinheriting its holder from interrupt context), and a second give of
//! the same mutex that must fail because it is already available.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `intsemMASTER_PRIORITY`.
pub const MASTER_PRIORITY: u8 = 0;
/// `intsemSLAVE_PRIORITY`.
pub const SLAVE_PRIORITY: u8 = 1;
/// `intsemINTERRUPT_MUTEX_GIVE_PERIOD_MS`, which is also the tick count at
/// 1000 Hz.
pub const GIVE_PERIOD: u64 = 100;
/// `intsemNO_BLOCK`.
pub const NO_BLOCK: u64 = 0;
/// `intsemMAX_COUNT`.
pub const MAX_COUNT: usize = 3;
/// `configMAX_PRIORITIES - 1`, which the counting task raises itself to.
pub const TOP_PRIORITY: u8 = 6;

/// `IntSemTest.c`'s task-half file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xMasterSlaveMutex`.
    pub master_slave_mutex: QueueHandle,
    /// `xISRMutex`, given only by the interrupt.
    pub isr_mutex: QueueHandle,
    /// `xISRCountingSemaphore`.
    pub isr_counting: QueueHandle,
    /// `xSlaveHandle`.
    pub slave: TaskHandle,
    /// `xErrorDetected`. The C stores `__LINE__`; only `!= pdFALSE` is ever
    /// tested, so a flag says the same thing.
    pub error: bool,
    /// `ulMasterLoops`.
    pub master_loops: u32,
    /// `ulCountingSemaphoreLoops`.
    pub counting_loops: u32,
    /// `ulLastMasterLoopCounter`, a static inside the check function.
    pub last_master_loops: u32,
    /// `ulLastCountingSemaphoreLoops`, likewise.
    pub last_counting_loops: u32,
}

impl State {
    /// `xAreInterruptSemaphoreTasksStillRunning`: both counters must move.
    pub fn still_running(&mut self) -> bool {
        if self.last_master_loops == self.master_loops
            || self.last_counting_loops == self.counting_loops
        {
            self.error = true;
        }
        self.last_master_loops = self.master_loops;
        self.last_counting_loops = self.counting_loops;
        !self.error
    }
}

/// `vInterruptSemaphorePeriodicTest`: the half that runs in the tick.
///
/// The two `xOkToGive...` flags live here rather than in the scenario's
/// state because the interrupt is the only reader and the tasks are the
/// only writers; [`Isr::permit_mutex`] and [`Isr::permit_counting`] are how
/// a task sets them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `xLastGiveTime`.
    last_give: u64,
    /// `xOkToGiveMutex`.
    ok_to_give_mutex: bool,
    /// `xOkToGiveCountingSemaphore`.
    ok_to_give_counting: bool,
    /// `xISRMutex`.
    mutex: QueueHandle,
    /// `xISRCountingSemaphore`.
    counting: QueueHandle,
    /// Set when the second give of the mutex unexpectedly succeeded — the
    /// C spells this `configASSERT( ... == pdFAIL )`.
    pub second_give_succeeded: bool,
}

impl Isr {
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        let now = k.tick_count_from_isr();
        if now.wrapping_sub(self.last_give) < GIVE_PERIOD {
            return self;
        }
        if self.ok_to_give_mutex {
            let _ = k.semaphore_give_from_isr(self.mutex);
            // The C asserts the second give fails: the mutex is available
            // again, so there is nothing to give back.
            if k.semaphore_give_from_isr(self.mutex).is_ok() {
                self.second_give_succeeded = true;
            }
        }
        if self.ok_to_give_counting {
            let _ = k.semaphore_give_from_isr(self.counting);
        }
        self.last_give = now;
        self
    }
}

/// One of the scenario's three tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vInterruptMutexMasterTask`.
    Master(Master),
    /// `vInterruptMutexSlaveTask`.
    Slave(Slave),
    /// `vInterruptCountingSemaphoreTask`.
    Counting(Counting),
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::IntSem(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::Master(b) => b.step(k, state),
            Self::Slave(b) => b.step(k, state),
            Self::Counting(b) => b.step(k, state),
        }
    }
}

/// A task's way of writing one of the interrupt's two permission flags.
fn permit_mutex<W: fmt::Write>(k: &mut SimKernel<W>, allow: bool) {
    if let TickIsr::IntSem(isr) = k.tick_hook_mut() {
        isr.ok_to_give_mutex = allow;
    }
}

fn permit_counting<W: fmt::Write>(k: &mut SimKernel<W>, allow: bool) {
    if let TickIsr::IntSem(isr) = k.tick_hook_mut() {
        isr.ok_to_give_counting = allow;
    }
}

/// `vInterruptMutexMasterTask` and the two helpers it calls.
///
/// The two helpers differ only in the order the mutexes go back, so they
/// share the `pc` space: 0..=15 is the body common to both, and `opposite`
/// says which order arm 11 and 13 take.
#[derive(Debug, Clone, Copy, Default)]
pub struct Master {
    pc: u8,
    /// Which of the two helpers is running.
    opposite: bool,
}

impl Master {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // configASSERT( eTaskGetState( xSlaveHandle ) == eSuspended );
            0 => {
                if k.task_state_get(s.slave) != Ok(TaskState::Suspended) {
                    s.error = true;
                }
                self.pc = 1;
            }
            // if( uxTaskPriorityGet( NULL ) != intsemMASTER_PRIORITY )
            1 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MASTER_PRIORITY {
                    s.error = true;
                }
                self.pc = 2;
            }
            // if( xSemaphoreTake( xMasterSlaveMutex, intsemNO_BLOCK ) != pdPASS )
            2 => {
                if !matches!(
                    k.semaphore_take(s.master_slave_mutex, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 3;
            }
            // vTaskResume( xSlaveHandle );
            3 => {
                let _ = k.resume(s.slave);
                self.pc = 4;
            }
            // configASSERT( eTaskGetState( xSlaveHandle ) == eBlocked );
            4 => {
                if k.task_state_get(s.slave) != Ok(TaskState::Blocked) {
                    s.error = true;
                }
                self.pc = 5;
            }
            // if( uxTaskPriorityGet( NULL ) != intsemSLAVE_PRIORITY )
            5 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != SLAVE_PRIORITY {
                    s.error = true;
                }
                self.pc = 6;
            }
            // xOkToGiveMutex = pdTRUE;
            6 => {
                permit_mutex(k, true);
                self.pc = 7;
            }
            // if( xSemaphoreTake( xISRMutex, xInterruptGivePeriod * 2 ) != pdPASS )
            7 => match k.semaphore_take(s.isr_mutex, GIVE_PERIOD.saturating_mul(2)) {
                Ok(Wait::Ready(())) => self.pc = 8,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 8;
                }
            },
            // xOkToGiveMutex = pdFALSE;
            8 => {
                permit_mutex(k, false);
                self.pc = 9;
            }
            // if( xSemaphoreTake( xISRMutex, intsemNO_BLOCK ) != pdFAIL )
            9 => {
                if matches!(
                    k.semaphore_take(s.isr_mutex, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 10;
            }
            // if( uxTaskPriorityGet( NULL ) != intsemSLAVE_PRIORITY )
            10 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != SLAVE_PRIORITY {
                    s.error = true;
                }
                self.pc = 11;
            }
            // The first give: the ISR mutex in the same-order helper, the
            // shared one in the opposite-order helper.
            11 => {
                let first = if self.opposite {
                    s.master_slave_mutex
                } else {
                    s.isr_mutex
                };
                if !matches!(k.semaphore_give(first), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 12;
            }
            // Either way the priority is still the slave's: one mutex is
            // still held, and FreeRTOS disinherits only on the last one.
            12 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != SLAVE_PRIORITY {
                    s.error = true;
                }
                self.pc = 13;
            }
            // The second give, which is where the priority finally drops.
            13 => {
                let second = if self.opposite {
                    s.isr_mutex
                } else {
                    s.master_slave_mutex
                };
                if !matches!(k.semaphore_give(second), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 14;
            }
            // if( uxTaskPriorityGet( NULL ) != intsemMASTER_PRIORITY )
            14 => {
                if k.task_priority_get(None).unwrap_or(u8::MAX) != MASTER_PRIORITY {
                    s.error = true;
                }
                self.pc = 15;
            }
            // configASSERT( eTaskGetState( xSlaveHandle ) == eSuspended );
            //
            // Only the same-order helper checks this, and the check is a
            // real kernel call with a critical section of its own — so
            // running it on both passes would cost an exit the C never
            // spends, and every trace line after it would move.
            15 => {
                if !self.opposite && k.task_state_get(s.slave) != Ok(TaskState::Suspended) {
                    s.error = true;
                }
                self.pc = 16;
            }
            // xQueueReset( xISRMutex ); — both helpers end with it.
            //
            // This is what makes the *next* take of the ISR mutex block:
            // a reset empties a mutex, so it is no longer available, and
            // only the interrupt can give it back.
            16 => {
                let _ = k.queue_reset(s.isr_mutex);
                self.pc = 17;
            }
            // ulMasterLoops++;
            17 => {
                s.master_loops = s.master_loops.wrapping_add(1);
                self.pc = 18;
            }
            // vTaskDelay( intsemINTERRUPT_MUTEX_GIVE_PERIOD_MS );
            _ => {
                let _ = k.delay(GIVE_PERIOD);
                // The next pass runs the other helper.
                self.opposite = !self.opposite;
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vInterruptMutexSlaveTask`: suspend, block on the shared mutex, give it
/// straight back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Slave {
    pc: u8,
}

impl Slave {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspend( NULL );
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // if( xSemaphoreTake( xMasterSlaveMutex, portMAX_DELAY ) != pdPASS )
            1 => match k.semaphore_take(s.master_slave_mutex, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Ready(())) => self.pc = 2,
                Ok(Wait::Blocked) => {}
                Err(_) => {
                    s.error = true;
                    self.pc = 2;
                }
            },
            // if( xSemaphoreGive( xMasterSlaveMutex ) != pdPASS )
            _ => {
                if !matches!(k.semaphore_give(s.master_slave_mutex), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vInterruptCountingSemaphoreTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counting {
    pc: u8,
    /// `xCount`.
    count: usize,
}

impl Counting {
    /// `xDelay`: long enough for the interrupt to fill the semaphore.
    const DELAY: u64 = GIVE_PERIOD * (MAX_COUNT as u64 + 1);

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( uxQueueMessagesWaiting( xISRCountingSemaphore ) != 0 )
            0 => {
                if k.queue_messages_waiting(s.isr_counting).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 1;
            }
            // xOkToGiveCountingSemaphore = pdTRUE;
            1 => {
                permit_counting(k, true);
                self.pc = 2;
            }
            // vTaskDelay( xDelay );
            2 => {
                let _ = k.delay(Self::DELAY);
                self.pc = 3;
            }
            // xOkToGiveCountingSemaphore = pdFALSE;
            3 => {
                permit_counting(k, false);
                self.pc = 4;
            }
            // if( uxQueueMessagesWaiting( xISRCountingSemaphore ) != intsemMAX_COUNT )
            4 => {
                if k.queue_messages_waiting(s.isr_counting).unwrap_or(usize::MAX) != MAX_COUNT {
                    s.error = true;
                }
                self.pc = 5;
            }
            // if( uxQueueSpacesAvailable( xISRCountingSemaphore ) != 0 )
            5 => {
                if k.queue_spaces_available(s.isr_counting).unwrap_or(usize::MAX) != 0 {
                    s.error = true;
                }
                self.pc = 6;
            }
            // ulCountingSemaphoreLoops++;
            6 => {
                s.counting_loops = s.counting_loops.wrapping_add(1);
                self.count = 0;
                self.pc = 7;
            }
            // while( xSemaphoreTake( xISRCountingSemaphore, 0 ) == pdPASS ) { xCount++; }
            7 => {
                if matches!(
                    k.semaphore_take(s.isr_counting, NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    self.count = self.count.saturating_add(1);
                } else {
                    self.pc = 8;
                }
            }
            // if( xCount != intsemMAX_COUNT )
            8 => {
                if self.count != MAX_COUNT {
                    s.error = true;
                }
                self.pc = 9;
            }
            // vTaskPrioritySet( NULL, configMAX_PRIORITIES - 1 );
            9 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 10;
            }
            // xOkToGiveCountingSemaphore = pdTRUE;
            10 => {
                permit_counting(k, true);
                self.pc = 11;
            }
            // xSemaphoreTake( xISRCountingSemaphore, portMAX_DELAY );
            11 => match k.semaphore_take(s.isr_counting, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Blocked) => {}
                Ok(Wait::Ready(())) | Err(_) => self.pc = 12,
            },
            // xSemaphoreTake( xISRCountingSemaphore, portMAX_DELAY );
            12 => match k.semaphore_take(s.isr_counting, SimKernel::<W>::MAX_DELAY) {
                Ok(Wait::Blocked) => {}
                Ok(Wait::Ready(())) | Err(_) => self.pc = 13,
            },
            // xOkToGiveCountingSemaphore = pdFALSE;
            13 => {
                permit_counting(k, false);
                self.pc = 14;
            }
            // vTaskPrioritySet( NULL, tskIDLE_PRIORITY );
            14 => {
                let _ = k.set_priority(None, 0);
                self.pc = 15;
            }
            // ulCountingSemaphoreLoops++;
            _ => {
                s.counting_loops = s.counting_loops.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartInterruptSemaphoreTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (isr_mutex, isr_counting, master_slave_mutex, slave, master, counting) = {
        let k = runner.kernel_mut();
        let isr_mutex = k.mutex_create()?;
        let isr_counting = k.semaphore_create_counting(MAX_COUNT, 0)?;
        let master_slave_mutex = k.mutex_create()?;
        let slave = k.create_task("IntMuS", SLAVE_PRIORITY)?;
        let master = k.create_task("IntMuM", MASTER_PRIORITY)?;
        let counting = k.create_task("IntCnt", 0)?;
        (
            isr_mutex,
            isr_counting,
            master_slave_mutex,
            slave,
            master,
            counting,
        )
    };
    runner.shared_mut().state = runner::State::IntSem(State {
        master_slave_mutex,
        isr_mutex,
        isr_counting,
        slave,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::IntSem(Isr {
        mutex: isr_mutex,
        counting: isr_counting,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(slave, runner::Body::IntSem(Body::Slave(Slave::default())));
    runner.attach(master, runner::Body::IntSem(Body::Master(Master::default())));
    runner.attach(
        counting,
        runner::Body::IntSem(Body::Counting(Counting::default())),
    );
    Ok(())
}
