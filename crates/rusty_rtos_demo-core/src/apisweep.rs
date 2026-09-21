//! `ApiSweep` — six public APIs that no upstream demo reaches.
//!
//! The C side is `oracle/harness/ApiSweep.c`, and it is the only scenario in
//! this corpus whose C was written here rather than compiled verbatim from
//! FreeRTOS. That is stated at the top of both files because it changes what
//! the differential is worth.
//!
//! # Why it exists
//!
//! `docs/HOLES.md` H2 counts public kernel APIs that no corpus scenario
//! reaches, so nobody has asked whether the Rust answers what the C answers.
//! Six of them have a direct C twin and **no upstream demo calls any of
//! them** — checked against `FreeRTOS/Demo/Common/Minimal`, not assumed:
//!
//! | C | here |
//! |---|---|
//! | `pcTaskGetName` | [`SimKernel::name_of`] |
//! | `xQueueSendToFrontFromISR` | `queue_send_to_front_from_isr` |
//! | `xTimerGetPeriod` | `timer_period` |
//! | `xTimerGetExpiryTime` | `timer_expiry_time` |
//! | `xTimerPendFunctionCall` | `timer_pend_function_call` |
//! | `xStreamBufferSetTriggerLevel` | `stream_buffer_set_trigger_level` |
//!
//! So there was nothing to port. **The oracle does not have to be an
//! upstream DEMO — it has to be the C KERNEL**, and it still is.
//!
//! # What a trace can judge here
//!
//! Most of these are queries that emit no trace event, which makes it look
//! as though a trace cannot judge them. It can, twice:
//!
//! * every one takes a critical section, and an outermost exit is a
//!   sixteenth of a tick on the sim — an API that takes a different NUMBER
//!   of sections than the C moves every event after it; and
//! * the C side checks the VALUES and latches a failure into its own
//!   `xAreApiSweepTasksStillRunning`, so a kernel that answers differently
//!   fails the scenario's check even where the trace would not move.
//!
//! # Where the shared statics live
//!
//! As `intqueue.rs` and `qset.rs`: the interrupt half runs from the tick
//! hook and the pended-function half runs from the daemon's dispatch, and
//! **neither is handed anything but the kernel**. So everything more than
//! one half touches lives in [`Isr`], reached through a short
//! `tick_hook_mut()` borrow.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{QueueHandle, StreamBufferHandle, TaskHandle, TimerHandle};
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `apiswpTASK_NAME`.
pub const TASK_NAME: &str = "ApiSweep";
/// `apiswpPRIORITY`.
pub const PRIORITY: u8 = 1;
/// `apiswpTIMER_PERIOD`.
const TIMER_PERIOD: u64 = 50;
/// `apiswpQUEUE_LENGTH`.
const QUEUE_LENGTH: usize = 4;
/// `apiswpBUFFER_BYTES`.
const BUFFER_BYTES: usize = 32;
/// `apiswpTRIGGER_OK`.
const TRIGGER_OK: usize = 4;
/// `apiswpTRIGGER_TOO_BIG`: one more than the buffer holds, which must be
/// refused. That refusal is half of what the call promises and the half a
/// port is likely to get wrong.
const TRIGGER_TOO_BIG: usize = BUFFER_BYTES + 1;
/// `apiswpSWEEP_DELAY`.
const SWEEP_DELAY: u64 = 20;
/// `apiswpISR_PERIOD`.
const ISR_PERIOD: u32 = 7;
/// `apiswpPEND_BLOCK`.
const PEND_BLOCK: u64 = 0;

/// The timer callback's id. It does nothing: the timer exists to be
/// QUERIED, not to fire usefully, and is auto-reload so that
/// `xTimerGetExpiryTime` always has a next expiry to answer with.
pub const CB_SWEEP_TIMER: u16 = 0x5701;
/// `prvPendedFunction`'s id, dispatched by the runner's `pended` hook.
pub const PENDED_SWEEP: u16 = 0x5702;

/// The interrupt half, the pended half, and every static they share with
/// the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isr {
    /// `xSweepQueue`, which the interrupt writes to the FRONT of.
    pub queue: QueueHandle,
    /// `xSweepReady`, so the interrupt does not touch a null handle.
    pub ready: bool,
    /// `ulCallCount`, the static inside `vApiSweepAccessFromISR`.
    call_count: u32,
    /// `ulPendedCalls`, incremented by the function the daemon runs. This
    /// is what proves `xTimerPendFunctionCall` DELIVERED rather than merely
    /// returned success.
    pub pended_calls: u32,
    /// `ulLastPendedParameter`, so the ARGUMENTS are checked too.
    pub last_pended_parameter: u64,
    /// `xApiSweepStatus`, which either half can fail.
    pub status: bool,
}

impl Default for Isr {
    fn default() -> Self {
        Self {
            queue: QueueHandle::NULL,
            ready: false,
            call_count: 0,
            pended_calls: 0,
            last_pended_parameter: 0,
            status: true,
        }
    }
}

impl Isr {
    /// `vApiSweepAccessFromISR`.
    pub(crate) fn tick<W: fmt::Write>(self, k: &mut SimKernel<W>) -> Self {
        let send = with_isr(k, |i| {
            if !i.ready {
                return None;
            }
            i.call_count = i.call_count.wrapping_add(1);
            if i.call_count.checked_rem(ISR_PERIOD) == Some(0) {
                // Never zero: the task checks for it.
                Some((i.queue, u64::from(i.call_count)))
            } else {
                None
            }
        })
        .flatten();

        if let Some((queue, value)) = send {
            // To the FRONT, which is the call under test.
            let _ = k.queue_send_to_front_from_isr(queue, value);
        }
        with_isr(k, |i| *i).unwrap_or(self)
    }
}

/// A short mutable borrow of the shared half. The closure must not call the
/// kernel; the borrow enforces it.
fn with_isr<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> Option<R> {
    match k.tick_hook_mut() {
        TickIsr::ApiSweep(isr) => Some(f(isr)),
        _ => None,
    }
}

/// `prvPendedFunction`, run by the timer daemon on the task's behalf.
pub(crate) fn pended_call<W: fmt::Write>(k: &mut SimKernel<W>, param2: u64) {
    with_isr(k, |i| {
        i.last_pended_parameter = param2;
        i.pended_calls = i.pended_calls.wrapping_add(1);
    });
}

/// `ApiSweep.c`'s task-side statics.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xSweepTimer`.
    pub timer: TimerHandle,
    /// `xSweepBuffer`.
    pub buffer: StreamBufferHandle,
    /// `xSweepTask`.
    pub task: TaskHandle,
    /// `ulSweepCycles`.
    pub cycles: u32,
    /// `ulLastSweepCycles`, the static inside the check function.
    last_cycles: u32,
}

impl State {
    /// `xAreApiSweepTasksStillRunning`.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        let mut pass = true;
        if self.cycles == self.last_cycles {
            // The sweep task has stalled.
            pass = false;
        }
        self.last_cycles = self.cycles;
        if !isr.status {
            pass = false;
        }
        pass
    }
}

/// The scenario's one task.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u16,
    /// `xNow`, read before the expiry so the window is one-sided.
    now: u64,
    /// `ulExpectedPended`, the value handed to the pended call.
    expected_pended: u64,
}

impl Body {
    /// `#[inline(never)]`, as every body in this corpus is.
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::ApiSweep(state) = &mut s.state else {
            return Step::Finish(false);
        };
        self.run(k, state)
    }

    /// Latch a wrong answer, as `prvFail` does.
    fn fail<W: fmt::Write>(k: &mut SimKernel<W>) {
        with_isr(k, |i| i.status = false);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kernel call, in the C's order"
    )]
    fn run<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // xTimerStart( xSweepTimer, portMAX_DELAY )
            //
            // The expiry time means nothing until the timer runs: a dormant
            // timer answers with whatever is left in the field, which is not
            // a promise either kernel makes.
            0 => {
                match k.timer_start(s.timer, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(true)) => {}
                    Ok(Wait::Blocked) => return Step::Continue,
                    Ok(Wait::Ready(false)) | Err(_) => Self::fail(k),
                }
                // Only now may the interrupt touch the queue.
                with_isr(k, |i| i.ready = true);
                self.pc = 1;
            }
            // pcTaskGetName( NULL ) -- the calling task.
            1 => {
                let current = k.current();
                let ok = k.name_of(current).is_ok_and(|n| n.as_str() == TASK_NAME);
                if !ok {
                    Self::fail(k);
                }
                self.pc = 2;
            }
            // pcTaskGetName( xSweepTask ) -- by handle, and it must agree.
            2 => {
                let ok = k.name_of(s.task).is_ok_and(|n| n.as_str() == TASK_NAME);
                if !ok {
                    Self::fail(k);
                }
                self.pc = 3;
            }
            // xTimerGetPeriod( xSweepTimer )
            3 => {
                if k.timer_period(s.timer) != Ok(TIMER_PERIOD) {
                    Self::fail(k);
                }
                self.pc = 4;
            }
            // xNow = xTaskGetTickCount(); read FIRST, so that a timer which
            // expires and reloads between the two calls can only make the
            // difference smaller -- the bound below is then a real bound
            // rather than a race.
            4 => {
                self.now = k.tick_count();
                self.pc = 5;
            }
            // xTimerGetExpiryTime( xSweepTimer )
            //
            // An auto-reload timer's next expiry is always within one period
            // of now. Written as an unsigned difference because that wraps
            // the same way on both sides.
            5 => {
                match k.timer_expiry_time(s.timer) {
                    Ok(expiry) => {
                        if expiry.wrapping_sub(self.now) > TIMER_PERIOD {
                            Self::fail(k);
                        }
                    }
                    Err(_) => Self::fail(k),
                }
                self.pc = 6;
            }
            // xStreamBufferSetTriggerLevel( buffer, 4 ) -- accepted.
            6 => {
                if k.stream_buffer_set_trigger_level(s.buffer, TRIGGER_OK) != Ok(true) {
                    Self::fail(k);
                }
                self.pc = 7;
            }
            // ...and one larger than the buffer, which must be REFUSED.
            7 => {
                if k.stream_buffer_set_trigger_level(s.buffer, TRIGGER_TOO_BIG) == Ok(true) {
                    Self::fail(k);
                }
                self.pc = 8;
            }
            // xTimerPendFunctionCall( prvPendedFunction, NULL, cycles, 0 )
            8 => {
                self.expected_pended = u64::from(s.cycles);
                match k.timer_pend_function_call(PENDED_SWEEP, 0, self.expected_pended, PEND_BLOCK)
                {
                    Ok(Wait::Ready(true)) => self.pc = 9,
                    Ok(Wait::Blocked) => {}
                    Ok(Wait::Ready(false)) | Err(_) => {
                        Self::fail(k);
                        self.pc = 9;
                    }
                }
            }
            // Drain whatever the interrupt put on the queue. Not blocking:
            // the point is the SEND, which happened in the interrupt.
            9 => {
                let queue = with_isr(k, |i| i.queue).unwrap_or_default();
                match k.queue_receive(queue, 0) {
                    Ok(Wait::Ready(value)) => {
                        // The interrupt sends its own call count, which only
                        // ever increases, so a zero means the queue handed
                        // back something that was never sent.
                        if value == 0 {
                            Self::fail(k);
                        }
                    }
                    // Empty, which ends the while loop.
                    Ok(Wait::Blocked) | Err(_) => {
                        s.cycles = s.cycles.saturating_add(1);
                        self.pc = 10;
                    }
                }
            }
            // vTaskDelay( apiswpSWEEP_DELAY )
            10 => {
                let _ = k.delay(SWEEP_DELAY);
                self.pc = 11;
            }
            // The pended call must have arrived: the daemon runs at the top
            // priority and this task has just slept. Checked AFTER the delay
            // so the daemon has had its chance. No kernel call, so it shares
            // the arm that returns to the head.
            _ => {
                let (calls, param) =
                    with_isr(k, |i| (i.pended_calls, i.last_pended_parameter)).unwrap_or((0, 0));
                if calls == 0 || param != self.expected_pended {
                    Self::fail(k);
                }
                self.pc = 1;
            }
        }
        Step::Continue
    }
}

/// `vStartApiSweepTasks`, in the C's order: the queue, the buffer, the
/// timer, then the task.
///
/// # Errors
///
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (queue, timer, buffer, task) = {
        let mut k = runner.kernel_mut();
        let queue = k.queue_create(QUEUE_LENGTH)?;
        let buffer = k.stream_buffer_create(BUFFER_BYTES, 1)?;
        let timer = k.timer_create("SwpTmr", TIMER_PERIOD, true, 0, CB_SWEEP_TIMER)?;
        let task = k.create_task(TASK_NAME, PRIORITY)?;
        (queue, timer, buffer, task)
    };

    runner.shared_mut().state = runner::State::ApiSweep(State {
        timer,
        buffer,
        task,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::ApiSweep(Isr {
        queue,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(task, runner::Body::ApiSweep(Body::default()));
    Ok(())
}
