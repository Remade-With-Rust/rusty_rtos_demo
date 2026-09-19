//! `MessageBufferAMP` — one message buffer per "core", and a send that
//! wakes the reader the long way round.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/MessageBufferAMP.c`. It
//! is a rehearsal for asymmetric multiprocessing on a single core: a
//! writer task pretends to be core A, two reader tasks pretend to be core
//! B, and the thing being tested is what happens *between* them.
//!
//! # The seam it exists to exercise
//!
//! A stream buffer's send normally ends by notifying the task waiting to
//! receive. On a real two-core system it cannot: the reader is not this
//! kernel's task to notify. FreeRTOS makes that ending a macro,
//! `sbSEND_COMPLETED`, so a port can replace it — and this demo replaces it
//! with:
//!
//! 1. put the *handle* of the buffer that was written on a control buffer,
//! 2. interrupt the other core,
//! 3. and let the other core's handler read the handle back and do the
//!    notify itself, from interrupt context.
//!
//! Here "the other core's interrupt" is a direct call, and step 3 is
//! [`SimKernel::send_completed_from_isr`]. The replacement is
//! [`TickHook::send_completed`], which the kernel offers first refusal and
//! which answers `true` to say it did the whole job.
//!
//! The recursion is the point and it terminates for the reason the C's does:
//! the send in step 1 is itself a completed send, so the hook runs again —
//! and does nothing, because the buffer it is being told about *is* the
//! control buffer.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::StreamBufferHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `mbaCONTROL_MESSAGE_BUFFER_SIZE`.
const CONTROL_MESSAGE_BUFFER_SIZE: usize = 24;
/// `mbaTASK_MESSAGE_BUFFER_SIZE`.
const TASK_MESSAGE_BUFFER_SIZE: usize = 60;
/// `mbaNUMBER_OF_CORE_B_TASKS`.
pub const CORE_B_TASKS: usize = 2;
/// `mbaDONT_BLOCK`.
const DONT_BLOCK: u64 = 0;
/// `pdMS_TO_TICKS( 250 )` at the demo configuration's 1000 Hz.
const CORE_A_DELAY: u64 = 250;

/// The width of a `MessageBufferHandle_t` on the machine the oracle runs
/// on, which is what the control buffer's messages are made of.
///
/// It matters because the trace records how many bytes a send moved: the C
/// puts a pointer on that buffer and the trace says eight. Ours puts a
/// handle in the low four bytes of the same eight, so the line matches and
/// the buffer fills at the same rate.
const HANDLE_BYTES: usize = 8;

/// `MessageBufferAMP.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xCoreBMessageBuffers`.
    pub core_b: [StreamBufferHandle; CORE_B_TASKS],
    /// `ulCycleCounters`.
    pub cycles: [u32; CORE_B_TASKS],
    /// `xDemoStatus`.
    pub status_ok: bool,
    /// `ulLastCycleCounters`, the statics in the still-running check.
    last_cycles: [u32; CORE_B_TASKS],
}

impl State {
    /// `xAreMessageBufferAMPTasksStillRunning`: every core B task must have
    /// gone round since the last time this was asked.
    pub fn still_running(&mut self) -> bool {
        let mut status = self.status_ok;
        for i in 0..CORE_B_TASKS {
            let now = self.cycles.get(i).copied().unwrap_or(0);
            if self.last_cycles.get(i).copied().unwrap_or(0) == now {
                status = false;
            }
            if let Some(slot) = self.last_cycles.get_mut(i) {
                *slot = now;
            }
        }
        status
    }
}

/// What the replaced `sbSEND_COMPLETED` needs: the control buffer's
/// handle, and a program counter.
///
/// # Why a handler needs a program counter
///
/// The C's replacement makes three kernel calls in a row, and the C can
/// afford to: it has a stack, so a tick that lands inside one of them
/// parks the thread there and the rest of the handler runs when the task
/// has the CPU back. A kernel with no stacks cannot park — the call
/// returns, and whatever came after it would run under the *next* task's
/// name, at the wrong tick.
///
/// So the handler is a state machine too, for exactly the reason a task
/// body is, and this is its `pc`. After each call it asks whether the
/// current task changed; if it did, the rest is owed, and [`CoreA::step`]
/// pays it on the first step after AMPCoreA is switched back in — which is
/// where the C's thread wakes up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `xControlMessageBuffer`.
    pub control: StreamBufferHandle,
    /// What the handler still owes: [`OWES_NOTHING`], [`OWES_RECEIVE`] or
    /// [`OWES_NOTIFY`].
    stage: u8,
    /// The buffer [`OWES_NOTIFY`] is about, read off the control buffer
    /// before the switch took the CPU away.
    updated: StreamBufferHandle,
}

/// The handler ran to the end.
const OWES_NOTHING: u8 = 0;
/// A tick landed in the send to the control buffer: the receive and the
/// notify are both still to come.
const OWES_RECEIVE: u8 = 1;
/// A tick landed in the receive: the handle is in hand and only the notify
/// is left.
const OWES_NOTIFY: u8 = 2;

/// One read-modify-write of the handler's state, with no kernel call in it.
fn update<W: fmt::Write, R>(k: &mut SimKernel<W>, f: impl FnOnce(&mut Isr) -> R) -> R {
    match k.tick_hook_mut() {
        TickIsr::MessageBufferAmp(isr) => f(isr),
        _ => f(&mut Isr::default()),
    }
}

/// `vGenerateCoreBInterrupt`, which is what `sbSEND_COMPLETED` becomes.
///
/// It always answers `true`: with the macro replaced, the kernel's own
/// notify does not happen for *any* buffer, including the control one.
pub(crate) fn send_completed<W: fmt::Write>(
    k: &mut SimKernel<W>,
    buffer: StreamBufferHandle,
) -> bool {
    let TickIsr::MessageBufferAmp(isr) = *k.tick_hook() else {
        return false;
    };
    // `if( xUpdatedBuffer != xControlMessageBuffer )`: the control buffer's
    // own sends are what stops this recursing.
    if buffer != isr.control {
        let mut bytes = [0_u8; HANDLE_BYTES];
        let raw = buffer.to_raw().to_le_bytes();
        if let Some(head) = bytes.get_mut(..raw.len()) {
            head.copy_from_slice(&raw);
        }
        let caller = k.current();
        let _ = k.stream_buffer_send(isr.control, &bytes, DONT_BLOCK);
        if k.current() == caller {
            core_b_interrupt_handler(k, isr.control);
        } else {
            update(k, |isr| isr.stage = OWES_RECEIVE);
        }
    }
    true
}

/// Finish whatever the handler was in the middle of when a tick took the
/// CPU away. Answers whether it did anything, so the caller knows this step
/// is spent.
pub(crate) fn resume_handler<W: fmt::Write>(k: &mut SimKernel<W>) -> bool {
    let (stage, control, updated) = update(k, |isr| (isr.stage, isr.control, isr.updated));
    match stage {
        OWES_RECEIVE => {
            update(k, |isr| isr.stage = OWES_NOTHING);
            core_b_interrupt_handler(k, control);
            true
        }
        OWES_NOTIFY => {
            update(k, |isr| isr.stage = OWES_NOTHING);
            notify_and_yield(k, updated);
            true
        }
        _ => false,
    }
}

/// `xMessageBufferSendCompletedFromISR`, and the yield after it.
fn notify_and_yield<W: fmt::Write>(k: &mut SimKernel<W>, updated: StreamBufferHandle) {
    let woken = k
        .send_completed_from_isr(updated)
        .unwrap_or(rusty_rtos_core::isr::Woken::NO);
    // `portYIELD_FROM_ISR( xHigherPriorityTaskWoken )`, which on the Posix
    // port is `portEND_SWITCHING_ISR` and therefore a plain `vPortYield()`
    // — an immediate switch, because this stands in for an interrupt
    // returning.
    if woken.needed() {
        k.task_yield();
    }
}

/// `prvCoreBInterruptHandler`: read the handle back and do the notify the
/// send did not.
fn core_b_interrupt_handler<W: fmt::Write>(k: &mut SimKernel<W>, control: StreamBufferHandle) {
    let mut bytes = [0_u8; HANDLE_BYTES];
    let caller = k.current();
    let received = match k.stream_buffer_receive(control, &mut bytes, DONT_BLOCK) {
        Ok(Wait::Ready(n)) => n,
        Ok(Wait::Blocked) | Err(_) => 0,
    };
    if received != HANDLE_BYTES {
        return;
    }
    let mut raw = [0_u8; 4];
    if let Some(head) = bytes.get(..4) {
        raw.copy_from_slice(head);
    }
    let updated = StreamBufferHandle::from_raw(u32::from_le_bytes(raw));
    if k.current() == caller {
        notify_and_yield(k, updated);
    } else {
        update(k, |isr| {
            isr.stage = OWES_NOTIFY;
            isr.updated = updated;
        });
    }
}

/// `prvCoreATask`: send the same ascending number to every core B buffer,
/// then wait a quarter of a second.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreA {
    pc: u8,
    /// `ulNextValue`.
    next_value: u32,
    /// `x`, the loop variable.
    index: usize,
}

impl CoreA {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::MbAmp(s) = &mut s.state else {
            return Step::Finish(false);
        };
        // The C's handler runs on this task's stack, so this task is where
        // it wakes up when a tick parked it.
        if resume_handler(k) {
            return Step::Continue;
        }
        match self.pc {
            // sprintf( cString, "%lu", ulNextValue ); then the send loop.
            0 => {
                self.index = 0;
                self.pc = 1;
            }
            1 => {
                let mut text = [0_u8; 15];
                let len = decimal(self.next_value, &mut text);
                let buffer = s.core_b.get(self.index).copied().unwrap_or_default();
                let body = text.get(..len).unwrap_or(&[]);
                let _ = k.stream_buffer_send(buffer, body, DONT_BLOCK);
                self.index = self.index.saturating_add(1);
                self.pc = if self.index < CORE_B_TASKS { 1 } else { 2 };
            }
            // vTaskDelay( xDelay );
            2 => {
                let _ = k.delay(CORE_A_DELAY);
                self.pc = 3;
            }
            _ => {
                self.next_value = self.next_value.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `prvCoreBTasks`: block on this task's own buffer, and check what arrives
/// is the number it was expecting.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreB {
    pc: u8,
    /// The task's parameter, `x`.
    index: usize,
    /// `ulNextValue`.
    next_value: u32,
    /// `xReceivedBytes`.
    received: usize,
    /// `cReceivedString`.
    text: [u8; 15],
}

impl CoreB {
    /// The two the C creates, told apart by their parameter.
    #[must_use]
    pub const fn at(index: usize) -> Self {
        Self {
            pc: 0,
            index,
            next_value: 0,
            received: 0,
            text: [0; 15],
        }
    }

    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::MbAmp(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self.pc {
            // xMessageBufferReceive( ..., portMAX_DELAY );
            0 => {
                let buffer = s.core_b.get(self.index).copied().unwrap_or_default();
                self.text = [0; 15];
                match k.stream_buffer_receive(buffer, &mut self.text, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(n)) => {
                        self.received = n;
                        self.pc = 1;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        s.status_ok = false;
                        self.pc = 1;
                    }
                }
            }
            // strcmp( cReceivedString, cExpectedString ) == 0
            _ => {
                let mut expected = [0_u8; 15];
                let len = decimal(self.next_value, &mut expected);
                let got = self.text.get(..self.received).unwrap_or(&[]);
                if self.received == len && got == expected.get(..len).unwrap_or(&[]) {
                    if let Some(slot) = s.cycles.get_mut(self.index) {
                        *slot = slot.wrapping_add(1);
                    }
                } else {
                    s.status_ok = false;
                }
                self.next_value = self.next_value.wrapping_add(1);
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `sprintf( cString, "%lu", value )`, which is all the formatting this
/// demo does. Answers how many bytes it wrote.
fn decimal(value: u32, out: &mut [u8; 15]) -> usize {
    if value == 0 {
        if let Some(slot) = out.first_mut() {
            *slot = b'0';
        }
        return 1;
    }
    let mut digits = [0_u8; 10];
    let mut n = value;
    let mut count = 0_usize;
    while n > 0 && count < digits.len() {
        if let Some(slot) = digits.get_mut(count) {
            *slot = b'0'.saturating_add((n % 10) as u8);
        }
        n /= 10;
        count = count.saturating_add(1);
    }
    for i in 0..count {
        let from = count.saturating_sub(1).saturating_sub(i);
        let byte = digits.get(from).copied().unwrap_or(b'0');
        if let Some(slot) = out.get_mut(i) {
            *slot = byte;
        }
    }
    count
}

/// `vStartMessageBufferAMPTasks`, in the C's order: the control buffer,
/// core A, then a buffer and a task for each core B.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let mut state = State {
        status_ok: true,
        ..State::default()
    };
    let control;
    let core_a;
    let mut core_b_tasks = [rusty_rtos_core::handle::TaskHandle::NULL; CORE_B_TASKS];
    {
        let mut k = runner.kernel_mut();
        control = k.message_buffer_create(CONTROL_MESSAGE_BUFFER_SIZE)?;
        core_a = k.create_task("AMPCoreA", 0)?;
        for index in 0..CORE_B_TASKS {
            let buffer = k.message_buffer_create(TASK_MESSAGE_BUFFER_SIZE)?;
            if let Some(slot) = state.core_b.get_mut(index) {
                *slot = buffer;
            }
            // Both tasks carry the same name in the C, which passes one
            // string literal for the pair.
            if let Some(slot) = core_b_tasks.get_mut(index) {
                *slot = k.create_task("AMPCoreB1", 1)?;
            }
        }
    }
    runner.shared_mut().state = runner::State::MbAmp(state);
    *runner.kernel_mut().tick_hook_mut() = TickIsr::MessageBufferAmp(Isr {
        control,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(core_a, runner::Body::MbAmpCoreA(CoreA::default()));
    for (index, task) in core_b_tasks.iter().enumerate() {
        runner.attach(*task, runner::Body::MbAmpCoreB(CoreB::at(index)));
    }
    Ok(())
}
