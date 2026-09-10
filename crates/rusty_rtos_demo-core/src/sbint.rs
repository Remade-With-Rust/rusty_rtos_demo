//! `StreamBufferInterrupt` — a string sent from the tick, read a byte at a time.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/StreamBufferInterrupt.c`.
//! Every hundred-and-first tick the interrupt writes four more bytes of
//! `"_____Hello FreeRTOS_____"` into a stream buffer, wrapping at the end;
//! one task reads *one byte at a time* with an infinite block time, hunts
//! for the `H` that starts the message and the `S` that ends it, and
//! compares what it collected against `"Hello FreeRTOS"`.
//!
//! Reading one byte at a time against a trigger level of ten is the point:
//! the reader blocks on a task notification until at least ten bytes have
//! arrived, then drains them one call at a time without blocking again.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::StreamBufferHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `sbiSTREAM_BUFFER_LENGTH_BYTES`.
pub const BUFFER_BYTES: usize = 100;
/// `sbiSTREAM_BUFFER_TRIGGER_LEVEL_10`.
pub const TRIGGER_LEVEL: usize = 10;
/// The priority the C creates the task at: `tskIDLE_PRIORITY + 2`.
pub const PRIORITY: u8 = 2;

/// `pcStringToSend`.
pub const STRING_TO_SEND: &[u8] = b"_____Hello FreeRTOS_____";
/// `pcStringToReceive`.
pub const STRING_TO_RECEIVE: &[u8] = b"Hello FreeRTOS";
/// `sizeof( cRxBuffer )`.
const RX_CAPACITY: usize = 20;

/// `StreamBufferInterrupt.c`'s task-half file-scope variables.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `xStreamBuffer`.
    pub buffer: StreamBufferHandle,
    /// `xDemoStatus`.
    pub status_ok: bool,
    /// `ulCycleCount`.
    pub cycles: u32,
    /// `ulLastCycleCount`, a static inside the check function.
    pub last_cycles: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            buffer: StreamBufferHandle::NULL,
            status_ok: true,
            cycles: 0,
            last_cycles: 0,
        }
    }
}

impl State {
    /// `xIsInterruptStreamBufferDemoStillRunning`.
    ///
    /// Note the C only advances its remembered count when the demo *is*
    /// moving, so a stall latches the failure rather than clearing it.
    pub fn still_running(&mut self) -> bool {
        if self.last_cycles == self.cycles {
            self.status_ok = false;
        } else {
            self.last_cycles = self.cycles;
        }
        self.status_ok
    }
}

/// `vBasicStreamBufferSendFromISR`: four bytes every hundred-and-first tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `xNextByteToSend`.
    next_byte: usize,
    /// `xCallCount`.
    call_count: i32,
    /// `xStreamBuffer`.
    buffer: StreamBufferHandle,
}

impl Isr {
    /// `xBytesToSend`.
    const BYTES_TO_SEND: usize = 4;
    /// `xCallsBetweenSends`.
    const CALLS_BETWEEN_SENDS: i32 = 100;

    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        self.call_count = self.call_count.wrapping_add(1);
        if self.call_count <= Self::CALLS_BETWEEN_SENDS {
            return self;
        }
        self.call_count = 0;
        // The C reads four bytes from `pcStringToSend + xNextByteToSend`
        // and does not stop at the end of the string: the last send of a
        // pass runs past the final underscore into the NUL and whatever
        // follows it. Since it only ever *sends* bytes the reader ignores
        // until the next `H`, the slice is clamped rather than wrapped.
        let end = self
            .next_byte
            .saturating_add(Self::BYTES_TO_SEND)
            .min(STRING_TO_SEND.len());
        let chunk = STRING_TO_SEND.get(self.next_byte..end).unwrap_or(&[]);
        let _ = k.stream_buffer_send_from_isr(self.buffer, chunk);
        self.next_byte = self.next_byte.saturating_add(Self::BYTES_TO_SEND);
        if self.next_byte >= STRING_TO_SEND.len() {
            self.next_byte = 0;
        }
        self
    }
}

/// `prvReceivingTask`.
#[derive(Debug, Clone, Copy)]
pub struct Body {
    pc: u8,
    /// `cRxBuffer`.
    rx: [u8; RX_CAPACITY],
    /// `xNextByte`.
    next_byte: usize,
}

impl Default for Body {
    fn default() -> Self {
        Self {
            pc: 0,
            rx: [0; RX_CAPACITY],
            next_byte: 0,
        }
    }
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::SbInt(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self.pc {
            // xStreamBufferReceive( xStreamBuffer, &cRxBuffer[ xNextByte ],
            //                       sizeof( char ), portMAX_DELAY );
            0 => {
                let mut one = [0_u8; 1];
                match k.stream_buffer_receive(s.buffer, &mut one, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(count)) => {
                        if count == 1 {
                            if let Some(slot) = self.rx.get_mut(self.next_byte) {
                                *slot = one[0];
                            }
                        }
                        self.pc = 1;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 1,
                }
            }
            // if( xNextByte == 0 ) { if( cRxBuffer[ 0 ] == 'H' ) xNextByte++; }
            // else { ... }
            _ => {
                let byte = self.rx.get(self.next_byte).copied().unwrap_or(0);
                if self.next_byte == 0 {
                    if byte == b'H' {
                        self.next_byte = 1;
                    }
                } else if byte == b'S' {
                    // The string is complete: compare it, NUL included.
                    let collected = self.rx.get(..=self.next_byte).unwrap_or(&[]);
                    if collected != STRING_TO_RECEIVE {
                        s.status_ok = false;
                    }
                    self.rx = [0; RX_CAPACITY];
                    self.next_byte = 0;
                    if s.status_ok {
                        s.cycles = s.cycles.wrapping_add(1);
                    }
                } else {
                    self.next_byte = self.next_byte.saturating_add(1);
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }
}

/// `vStartStreamBufferInterruptDemo`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (buffer, task) = {
        let mut k = runner.kernel_mut();
        let buffer = k.stream_buffer_create(BUFFER_BYTES, TRIGGER_LEVEL)?;
        let task = k.create_task("StrIntRx", PRIORITY)?;
        (buffer, task)
    };
    runner.shared_mut().state = runner::State::SbInt(State {
        buffer,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::StreamBufferInterrupt(Isr {
        buffer,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(task, runner::Body::SbInt(Body::default()));
    Ok(())
}
