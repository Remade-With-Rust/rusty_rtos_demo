//! `dynamic` — the first scenario of the conformance corpus.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/dynamic.c`: five tasks
//! that exercise `vTaskSuspend`, `vTaskResume`, `vTaskPrioritySet`,
//! `vTaskSuspendAll` / `xTaskResumeAll` and a queue used with a zero block
//! time from inside a suspended scheduler. It is the scenario that proves a
//! scheduler can be interrupted at every awkward moment and still agree
//! with itself.
//!
//! Each task here is the C function turned inside out: one `step` per C
//! statement, with `pc` naming the statement it will run next. The comments
//! give the C line each arm stands for, because the diff against the oracle
//! is read that way — a divergence names a trace line, and the trace line
//! names the statement.
//!
//! The `configASSERT` calls in the C are **not** decoration: `configASSERT`
//! is defined in the harness, so `uxTaskPriorityGet` and `eTaskGetState`
//! really are called, and each takes a critical section — which on the sim
//! is where time passes. Dropping them would change the tick count, so they
//! are kept, with their results checked the way the C checks them.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, TaskHandle};
use rusty_rtos_kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `priSLEEP_TIME`: `pdMS_TO_TICKS( 128 )` at 1000 Hz.
pub const SLEEP_TIME: u64 = 128;
/// `priLOOPS`.
pub const LOOPS: u16 = 5;
/// `priMAX_COUNT`.
pub const MAX_COUNT: u32 = 0xff;
/// `priNO_BLOCK`.
pub const NO_BLOCK: u64 = 0;
/// `priSUSPENDED_QUEUE_LENGTH`.
pub const QUEUE_LENGTH: usize = 1;

/// `dynamic.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `ulCounter`.
    pub counter: u32,
    /// `usCheckVariable`.
    pub check_variable: u16,
    /// `ulExpectedValue`.
    pub expected_value: u32,
    /// `xSuspendedQueueSendError`.
    pub send_error: bool,
    /// `xSuspendedQueueReceiveError`.
    pub receive_error: bool,
    /// `xContinuousIncrementHandle`.
    pub cnt_inc: TaskHandle,
    /// `xLimitedIncrementHandle`.
    pub lim_inc: TaskHandle,
    /// `xSuspendedTestQueue`.
    pub queue: QueueHandle,
    /// `usLastTaskCheck`, a static inside the check function.
    pub last_task_check: u16,
    /// `ulLastExpectedValue`, likewise.
    pub last_expected_value: u32,
}

impl State {
    /// `xAreDynamicPriorityTasksStillRunning`, statics and all.
    pub fn still_running(&mut self) -> bool {
        let mut running = true;
        if self.check_variable == self.last_task_check {
            running = false;
        }
        if self.expected_value == self.last_expected_value {
            running = false;
        }
        if self.send_error || self.receive_error {
            running = false;
        }
        self.last_task_check = self.check_variable;
        self.last_expected_value = self.expected_value;
        running
    }
}

/// One of the scenario's five tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `vContinuousIncrementTask`.
    CntInc(CntInc),
    /// `vLimitedIncrementTask`.
    LimInc(LimInc),
    /// `vCounterControlTask`.
    CCtrl(CCtrl),
    /// `vQueueSendWhenSuspendedTask`.
    SuspTx(SuspTx),
    /// `vQueueReceiveWhenSuspendedTask`.
    SuspRx(SuspRx),
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::Dynamic(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::CntInc(b) => b.step(k, s),
            Self::LimInc(b) => b.step(k, s),
            Self::CCtrl(b) => b.step(k, s),
            Self::SuspTx(b) => b.step(k, s),
            Self::SuspRx(b) => b.step(k, s),
        }
    }
}

/// `vContinuousIncrementTask`: raise own priority, count, lower it again,
/// for ever. The task the control task suspends and resumes underneath.
#[derive(Debug, Clone, Copy, Default)]
pub struct CntInc {
    pc: u8,
    our_priority: u8,
}

impl CntInc {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // uxOurPriority = uxTaskPriorityGet( NULL );
            0 => {
                self.our_priority = k.task_priority_get(None).unwrap_or(0);
                self.pc = 1;
            }
            // vTaskPrioritySet( NULL, uxOurPriority + 1 );
            1 => {
                let _ = k.set_priority(None, self.our_priority.saturating_add(1));
                self.pc = 2;
            }
            // configASSERT( uxTaskPriorityGet( NULL ) == uxOurPriority + 1 );
            2 => {
                let _ = k.task_priority_get(None);
                self.pc = 3;
            }
            // ( *pulCounter )++;
            3 => {
                s.counter = s.counter.wrapping_add(1);
                self.pc = 4;
            }
            // vTaskPrioritySet( NULL, uxOurPriority );
            4 => {
                let _ = k.set_priority(None, self.our_priority);
                self.pc = 5;
            }
            // configASSERT( uxTaskPriorityGet( NULL ) == uxOurPriority );
            _ => {
                let _ = k.task_priority_get(None);
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vLimitedIncrementTask`: suspend itself, and once resumed count up to
/// `priMAX_COUNT` and suspend itself again.
#[derive(Debug, Clone, Copy, Default)]
pub struct LimInc {
    pc: u8,
}

impl LimInc {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspend( NULL ); — before the loop starts.
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // ( *pulCounter )++;
            1 => {
                s.counter = s.counter.wrapping_add(1);
                self.pc = 2;
            }
            // if( *pulCounter >= priMAX_COUNT ) { vTaskSuspend( NULL ); }
            _ => {
                if s.counter >= MAX_COUNT {
                    let _ = k.suspend(None);
                }
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vCounterControlTask`: the scenario's referee. Suspends and resumes the
/// continuous task, checks the counter moved, then hands the limited task
/// its turn and checks it stopped at exactly `priMAX_COUNT`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CCtrl {
    pc: u8,
    loops: u16,
    last_counter: u32,
    /// `sError`, which in C is declared outside the outer loop and so
    /// latches for the life of the task.
    error: bool,
}

impl CCtrl {
    #[allow(clippy::too_many_lines)]
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // ulCounter = 0; sLoops = 0;
            0 => {
                s.counter = 0;
                self.loops = 0;
                self.pc = 1;
            }
            // vTaskSuspend( xContinuousIncrementHandle );
            1 => {
                let _ = k.suspend(Some(s.cnt_inc));
                self.pc = 2;
            }
            // configASSERT( eTaskGetState( ... ) == eSuspended );
            2 => {
                if k.task_state_get(s.cnt_inc) != Ok(TaskState::Suspended) {
                    self.error = true;
                }
                self.pc = 3;
            }
            // ulLastCounter = ulCounter;
            3 => {
                self.last_counter = s.counter;
                self.pc = 4;
            }
            // vTaskResume( xContinuousIncrementHandle );
            4 => {
                let _ = k.resume(s.cnt_inc);
                self.pc = 5;
            }
            // configASSERT( eTaskGetState( ... ) == eReady );
            5 => {
                if k.task_state_get(s.cnt_inc) != Ok(TaskState::Ready) {
                    self.error = true;
                }
                self.pc = 6;
            }
            // vTaskDelay( priSLEEP_TIME );
            6 => {
                let _ = k.delay(SLEEP_TIME);
                self.pc = 7;
            }
            // vTaskSuspendAll();
            7 => {
                k.suspend_all();
                self.pc = 8;
            }
            // if( ulLastCounter == ulCounter ) { sError = pdTRUE; }
            8 => {
                if self.last_counter == s.counter {
                    self.error = true;
                }
                self.pc = 9;
            }
            // xTaskResumeAll(); and the loop test.
            9 => {
                let _ = k.resume_all();
                self.loops = self.loops.saturating_add(1);
                self.pc = if self.loops < LOOPS { 1 } else { 10 };
            }
            // vTaskSuspend( xContinuousIncrementHandle );
            10 => {
                let _ = k.suspend(Some(s.cnt_inc));
                self.pc = 11;
            }
            // ulCounter = 0;
            11 => {
                s.counter = 0;
                self.pc = 12;
            }
            // configASSERT( eTaskGetState( xLimitedIncrementHandle ) == eSuspended );
            12 => {
                if k.task_state_get(s.lim_inc) != Ok(TaskState::Suspended) {
                    self.error = true;
                }
                self.pc = 13;
            }
            // vTaskResume( xLimitedIncrementHandle );
            13 => {
                let _ = k.resume(s.lim_inc);
                self.pc = 14;
            }
            // configASSERT( eTaskGetState( xLimitedIncrementHandle ) == eSuspended );
            //
            // Yes, suspended again: the limited task runs at a higher
            // priority, counts to priMAX_COUNT and suspends itself before
            // this task ever sees the CPU again. That is the assertion the
            // whole scenario exists to make.
            14 => {
                if k.task_state_get(s.lim_inc) != Ok(TaskState::Suspended) {
                    self.error = true;
                }
                self.pc = 15;
            }
            // if( ulCounter != priMAX_COUNT ) { sError = pdTRUE; }
            15 => {
                if s.counter != MAX_COUNT {
                    self.error = true;
                }
                self.pc = 16;
            }
            // if( sError == pdFALSE ) { portENTER_CRITICAL(); usCheckVariable++; portEXIT_CRITICAL(); }
            16 => {
                if !self.error {
                    k.enter_critical();
                    s.check_variable = s.check_variable.wrapping_add(1);
                    k.exit_critical();
                }
                self.pc = 17;
            }
            // vTaskResume( xContinuousIncrementHandle );
            _ => {
                let _ = k.resume(s.cnt_inc);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vQueueSendWhenSuspendedTask`: send to a one-deep queue with the
/// scheduler suspended and a zero block time, then sleep.
#[derive(Debug, Clone, Copy, Default)]
pub struct SuspTx {
    pc: u8,
    /// `ulValueToSend`, a function static in C.
    value: u32,
}

impl SuspTx {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspendAll();
            0 => {
                k.suspend_all();
                self.pc = 1;
            }
            // if( xQueueSend( ..., priNO_BLOCK ) != pdTRUE ) { xSuspendedQueueSendError = pdTRUE; }
            1 => {
                // A zero block time cannot park the task, so `Ready` or
                // an error is the whole answer.
                if !matches!(
                    k.queue_send(s.queue, u64::from(self.value), NO_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.send_error = true;
                }
                self.pc = 2;
            }
            // xTaskResumeAll();
            2 => {
                let _ = k.resume_all();
                self.pc = 3;
            }
            // vTaskDelay( priSLEEP_TIME );
            3 => {
                let _ = k.delay(SLEEP_TIME);
                self.pc = 4;
            }
            // ++ulValueToSend;
            _ => {
                self.value = self.value.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vQueueReceiveWhenSuspendedTask`: poll the queue from inside a *doubly*
/// suspended scheduler until something arrives, checking that the inner
/// `xTaskResumeAll` never claims to have yielded.
#[derive(Debug, Clone, Copy, Default)]
pub struct SuspRx {
    pc: u8,
    got_value: bool,
    received: u32,
}

impl SuspRx {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // vTaskSuspendAll();  (outer)
            0 => {
                k.suspend_all();
                self.pc = 1;
            }
            // vTaskSuspendAll();  (inner)
            1 => {
                k.suspend_all();
                self.pc = 2;
            }
            // xGotValue = xQueueReceive( ..., priNO_BLOCK );
            2 => {
                match k.queue_receive(s.queue, NO_BLOCK) {
                    Ok(Wait::Ready(value)) => {
                        self.received = u32::try_from(value).unwrap_or(u32::MAX);
                        self.got_value = true;
                    }
                    Ok(Wait::Blocked) | Err(_) => self.got_value = false,
                }
                self.pc = 3;
            }
            // if( xTaskResumeAll() != pdFALSE ) { xSuspendedQueueReceiveError = pdTRUE; }
            //
            // The inner resume must not yield: the scheduler is still
            // suspended by the outer one.
            3 => {
                if k.resume_all() {
                    s.receive_error = true;
                }
                self.pc = 4;
            }
            // xTaskResumeAll();  (outer), then `while( xGotValue == pdFALSE )`.
            4 => {
                let _ = k.resume_all();
                self.pc = if self.got_value { 5 } else { 0 };
            }
            // if( ulReceivedValue != ulExpectedValue ) { xSuspendedQueueReceiveError = pdTRUE; }
            5 => {
                if self.received != s.expected_value {
                    s.receive_error = true;
                }
                self.pc = 6;
            }
            // if( xSuspendedQueueReceiveError != pdTRUE ) { ++ulExpectedValue; }
            _ => {
                if !s.receive_error {
                    s.expected_value = s.expected_value.wrapping_add(1);
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartDynamicPriorityTasks`, in the C's order — which is what puts the
/// queue at ordinal `q1` and the five tasks where they are in the trace.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (queue, cnt_inc, lim_inc, c_ctrl, susp_tx, susp_rx) = {
        let mut k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let cnt_inc = k.create_task("CNT_INC", 0)?;
        let lim_inc = k.create_task("LIM_INC", 1)?;
        let c_ctrl = k.create_task("C_CTRL", 0)?;
        let susp_tx = k.create_task("SUSP_TX", 0)?;
        let susp_rx = k.create_task("SUSP_RX", 0)?;
        (queue, cnt_inc, lim_inc, c_ctrl, susp_tx, susp_rx)
    };
    runner.shared_mut().state = runner::State::Dynamic(State {
        queue,
        cnt_inc,
        lim_inc,
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(
        cnt_inc,
        runner::Body::Dynamic(Body::CntInc(CntInc::default())),
    );
    runner.attach(
        lim_inc,
        runner::Body::Dynamic(Body::LimInc(LimInc::default())),
    );
    runner.attach(c_ctrl, runner::Body::Dynamic(Body::CCtrl(CCtrl::default())));
    runner.attach(
        susp_tx,
        runner::Body::Dynamic(Body::SuspTx(SuspTx::default())),
    );
    runner.attach(
        susp_rx,
        runner::Body::Dynamic(Body::SuspRx(SuspRx::default())),
    );
    Ok(())
}
