//! `death` — the scenario that deletes tasks.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/death.c`: one persistent
//! CREATOR task that every second spawns two suicidal tasks, one of which
//! kills the other and then itself. It is the first scenario in the corpus
//! that calls `vTaskDelete` at all, and the first that calls `xTaskCreate`
//! with the scheduler already **running**.
//!
//! # Why this scenario earns its place
//!
//! `vTaskDelete` has two paths and they are not variations on each other:
//!
//! * deleting **another** task frees its TCB inside `vTaskDelete` itself,
//!   after the critical section;
//! * deleting **yourself** cannot, because the caller is still executing out
//!   of that TCB. The C parks it on `xTasksWaitingTermination` and the idle
//!   task frees it later, in `prvCheckTasksWaitingTermination`.
//!
//! `vSuicidalTask` takes both paths back to back — `vTaskDelete( xTaskToKill )`
//! then `vTaskDelete( NULL )` — which is why 200 lines of C are worth the
//! remake. A kernel that got the deferred path wrong would still pass the
//! other eighteen scenarios, because none of them deletes anything.
//!
//! # The thing this scenario measures that no other one can
//!
//! The sim port counts outermost exits **only once the scheduler is
//! running** (`exits_are_not_counted_before_the_scheduler_runs`), and every
//! other scenario creates all of its tasks before `vTaskStartScheduler`.
//! So the two `pvPortMalloc` calls inside `prvCreateTask` — and the two
//! `vPortFree` calls inside `prvDeleteTCB` — were invisible to the entire
//! corpus. Here they are not: two creates and two deletes per cycle, all
//! with the scheduler up, each one worth an exit on heap_3. If that
//! accounting is wrong, sim time drifts and the diff says so.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::TaskHandle;

use crate::runner::{self, Runner, Shared, SimKernel, Spawn, Step};

/// `harnessDEATH_PRIORITY`: the upstream Posix demo's
/// `mainCREATOR_TASK_PRIORITY`, `tskIDLE_PRIORITY + 3`.
pub const PRIORITY: u8 = 3;
/// `xDelay` in `vCreateTasks`: `pdMS_TO_TICKS( 1000 )` at 1000 Hz.
pub const CREATE_DELAY: u64 = 1000;
/// `xDelay` in `vSuicidalTask`: `pdMS_TO_TICKS( 200 )`.
pub const SUICIDE_DELAY: u64 = 200;
/// `uxMaxNumberOfExtraTasksRunning`.
pub const MAX_EXTRA_TASKS: usize = 3;

/// `death.c`'s file-scope variables.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `usCreationCount`.
    pub creation_count: u16,
    /// `uxTasksRunningAtStart`.
    pub tasks_running_at_start: usize,
    /// `xCreatedTask`, the handle the second suicidal task is handed so it
    /// can kill the first.
    pub created_task: TaskHandle,
    /// `usLastCreationCount`, the `static` inside the check function. The C
    /// initialises it to `0xfff` precisely so the first check cannot fail
    /// for want of a previous value.
    pub last_creation_count: u16,
}

impl Default for State {
    fn default() -> Self {
        Self {
            creation_count: 0,
            tasks_running_at_start: 0,
            created_task: TaskHandle::NULL,
            last_creation_count: 0xfff,
        }
    }
}

impl State {
    /// `xIsCreateTaskStillRunning`, statics and all.
    ///
    /// The count must have MOVED since the last check — a creator that has
    /// stopped creating is a failure — and the task count must have come
    /// back down, which is the assertion that the deletions really happened.
    /// A kernel that deleted nothing would keep passing the first test for
    /// ever and fail this one.
    pub fn still_running(&mut self, tasks_running_now: usize) -> bool {
        let mut running = true;
        if self.last_creation_count == self.creation_count {
            running = false;
        } else {
            self.last_creation_count = self.creation_count;
        }
        // The C asks these as two branches, and not for style: its
        // `uxTasksRunningNow - uxTasksRunningAtStart` is UNSIGNED, so it
        // has to rule out the smaller-than case before it dares subtract.
        // `saturating_sub` makes the subtraction safe but NOT the check —
        // it would quietly return 0 for "fewer tasks than we started with",
        // which is a failure reported as a pass. Both questions still get
        // asked; only the underflow is gone.
        let fewer_than_at_start = tasks_running_now < self.tasks_running_at_start;
        let too_many_left_alive =
            tasks_running_now.saturating_sub(self.tasks_running_at_start) > MAX_EXTRA_TASKS;
        if fewer_than_at_start || too_many_left_alive {
            running = false;
        }
        running
    }
}

/// One of the two task functions.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vCreateTasks`.
    Creator(Creator),
    /// `vSuicidalTask`.
    Suicidal(Suicidal),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self {
            Self::Creator(b) => b.step(k, s),
            Self::Suicidal(b) => {
                let runner::State::Death(state) = &mut s.state else {
                    return Step::Finish(false);
                };
                b.step(k, state)
            }
        }
    }
}

/// `vCreateTasks`: the CREATOR task, which never dies.
#[derive(Debug, Clone, Copy, Default)]
pub struct Creator {
    pc: u8,
    /// `uxPriority`, read once before the loop.
    priority: u8,
}

impl Creator {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        match self.pc {
            // vTaskDelay( xDelay ); -- before the count is taken, so the
            // tasks of other demos exist by the time it is.
            0 => {
                let _ = k.delay(CREATE_DELAY);
                self.pc = 1;
            }
            // uxTasksRunningAtStart = uxTaskGetNumberOfTasks();
            //
            // `uxTaskGetNumberOfTasks` takes NO critical section in the C —
            // the comment in `tasks.c` says so outright — so this must not
            // cost an exit, and `Kernel::task_count` is a plain getter for
            // exactly that reason.
            1 => {
                let count = k.task_count();
                if let runner::State::Death(state) = &mut s.state {
                    state.tasks_running_at_start = count;
                }
                self.pc = 2;
            }
            // uxPriority = uxTaskPriorityGet( NULL );
            2 => {
                self.priority = k.task_priority_get(None).unwrap_or(0);
                self.pc = 3;
            }
            // vTaskDelay( xDelay ); -- the top of the for(;;).
            3 => {
                let _ = k.delay(CREATE_DELAY);
                self.pc = 4;
            }
            // xCreatedTask = NULL;
            4 => {
                if let runner::State::Death(state) = &mut s.state {
                    state.created_task = TaskHandle::NULL;
                }
                self.pc = 5;
            }
            // xTaskCreate( vSuicidalTask, "SUICID1", ..., NULL, uxPriority,
            //              &xCreatedTask );
            //
            // Passed NULL as its parameter, so it kills nothing; and its
            // handle is stored, which is what the SECOND one is handed.
            5 => {
                match k.create_task("SUICID1", self.priority) {
                    Ok(task) => {
                        if let runner::State::Death(state) = &mut s.state {
                            state.created_task = task;
                        }
                        s.spawn = Some((task, Spawn::Death(Body::Suicidal(Suicidal::victim()))));
                    }
                    Err(_) => return Step::Finish(false),
                }
                self.pc = 6;
            }
            // xTaskCreate( vSuicidalTask, "SUICID2", ..., &xCreatedTask,
            //              uxPriority, NULL );
            6 => {
                match k.create_task("SUICID2", self.priority) {
                    Ok(task) => s.spawn = Some((task, Spawn::Death(Body::Suicidal(Suicidal::killer())))),
                    Err(_) => return Step::Finish(false),
                }
                self.pc = 7;
            }
            // ++usCreationCount;
            _ => {
                if let runner::State::Death(state) = &mut s.state {
                    state.creation_count = state.creation_count.wrapping_add(1);
                }
                self.pc = 3;
            }
        }
        Step::Continue
    }
}

/// `vSuicidalTask`: delay, and if it was given a victim, kill it and then
/// itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct Suicidal {
    pc: u8,
    /// Whether `pvParameters` was non-NULL. The C reads the handle THROUGH
    /// that pointer when the task first runs, not when it is created — and
    /// the distinction is real, because `xCreatedTask` is a file-scope
    /// variable the creator has written by then.
    takes_param: bool,
    /// `xTaskToKill`.
    kill: TaskHandle,
}

impl Suicidal {
    /// `SUICID1`: `pvParameters == NULL`, so it kills nothing and waits to
    /// be killed.
    const fn victim() -> Self {
        Self {
            pc: 0,
            takes_param: false,
            kill: TaskHandle::NULL,
        }
    }

    /// `SUICID2`: handed `&xCreatedTask`, so it kills `SUICID1` and itself.
    const fn killer() -> Self {
        Self {
            pc: 0,
            takes_param: true,
            kill: TaskHandle::NULL,
        }
    }

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // if( pvParameters != NULL ) { xTaskToKill = *( TaskHandle_t * )
            // pvParameters; } else { xTaskToKill = NULL; }
            0 => {
                self.kill = if self.takes_param {
                    s.created_task
                } else {
                    TaskHandle::NULL
                };
                self.pc = 1;
            }
            // l1 = 2; l2 = 89; l2 *= l1; -- "do something random just to use
            // some stack and registers". No kernel call, so no trace line
            // and no exit; it is kept because the C loop has a body.
            1 => {
                self.pc = 2;
            }
            // vTaskDelay( xDelay );
            2 => {
                let _ = k.delay(SUICIDE_DELAY);
                self.pc = 3;
            }
            // if( xTaskToKill != NULL ) { vTaskDelay( ( TickType_t ) 0 ); }
            //
            // A zero delay is NOT a block: the C's `traceTASK_DELAY` sits
            // inside `if( xTicksToDelay > 0U )`, so this emits no event and
            // only yields — "make sure the other task has a go before we
            // delete it".
            3 => {
                if self.kill.is_null() {
                    self.pc = 1;
                } else {
                    let _ = k.delay(0);
                    self.pc = 4;
                }
            }
            // vTaskDelete( xTaskToKill ); -- the other task, which is
            // sitting on the delayed list. Not deferred: it is not running,
            // so its TCB is freed here and `prvResetNextTaskUnblockTime`
            // runs because the list it was on may now be empty.
            4 => {
                let _ = k.task_delete(Some(self.kill));
                self.pc = 5;
            }
            // vTaskDelete( NULL ); -- itself. Deferred to the idle task, and
            // it yields inside the call and never comes back.
            _ => {
                let _ = k.task_delete(None);
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vCreateSuicidalTasks`, at `harnessDEATH_PRIORITY`.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let creator = {
        let mut k = runner.kernel_mut();
        k.create_task("CREATOR", PRIORITY)?
    };
    runner.shared_mut().state = runner::State::Death(State::default());
    runner.start_common(max_ticks)?;
    runner.attach(
        creator,
        runner::Body::Death(Body::Creator(Creator::default())),
    );
    Ok(())
}
