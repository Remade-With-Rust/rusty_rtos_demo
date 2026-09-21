//! `StreamBufferDemo` — the stream buffer tortured from every side.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/StreamBufferDemo.c`. It
//! is the only demo that exercises the whole stream-buffer face rather than
//! one call of it, which is why it is the scenario that closes six of H2's
//! APIs at once: `Reset`, `IsEmpty`, `IsFull`, `BytesAvailable`,
//! `SpacesAvailable` and `ReceiveFromISR`.
//!
//! Three things run:
//!
//! * two **echo pairs**, a server and a client each, one with the server
//!   the higher priority and one with it the lower, so the data crosses a
//!   priority boundary in both directions;
//! * [`Single`], the C's `prvSingleTaskTests`, which the higher-priority
//!   server runs once before it creates its client — three hundred lines of
//!   straight-line arithmetic on one buffer's head and tail;
//! * the **trigger-level test**, which streams bytes in from the tick
//!   interrupt and checks a blocked reader wakes at exactly the trigger
//!   level it asked for.
//!
//! * the **non-blocking pair**, which spins at `tskIDLE_PRIORITY` with
//!   `sbDONT_BLOCK` and relies on being preempted to interleave.
//!
//! # The pair is why the sim contract has a version 2
//!
//! Under contract v1 this scenario could not run at all. Time advanced
//! only at the idle hook and at outermost critical-section exits, and
//! `xStreamBufferReceive`'s zero-wait path takes neither when the buffer is
//! empty -- so `prvNonBlockingReceiverTask` took no time, never yielded,
//! and froze the whole run at `ticks=1 exits=16`. It was worked around by
//! patching the pair out of the C.
//!
//! v2 says a kernel call that took no critical section is itself a
//! kernel-visible point, and costs one empty section. The pair runs, the
//! workaround is gone, and `docs/HOLES.md` H9 is closed.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::StreamBufferHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Spawn, Step, TickIsr};

/// `sbSTREAM_BUFFER_LENGTH_BYTES`.
pub const BUFFER_BYTES: usize = 30;
/// `sbSTREAM_BUFFER_LENGTH_ONE`.
pub const BUFFER_LENGTH_ONE: usize = 1;
/// `sbTRIGGER_LEVEL_1`.
pub const TRIGGER_LEVEL_1: usize = 1;
/// `sbNUMBER_OF_ECHO_CLIENTS`.
pub const ECHO_CLIENTS: usize = 2;
/// `sbNUMBER_OF_SENDER_TASKS`, which sizes `ucBufferStorage` and so
/// `xTrueSize` — the array is at file scope, *outside* the static-allocation
/// guard, so it exists even though the tasks that use it do not.
pub const SENDER_TASKS: usize = 2;
/// `sizeof( ucBufferStorage ) / sbNUMBER_OF_SENDER_TASKS`: two rows of
/// `BUFFER_BYTES + 1`, halved. One more than the buffer holds, which is the
/// point of the two calls that use it.
pub const TRUE_SIZE: usize = ((BUFFER_BYTES + 1) * SENDER_TASKS) / SENDER_TASKS;
/// `sbLOWER_PRIORITY` (`tskIDLE_PRIORITY`).
pub const LOWER_PRIORITY: u8 = 0;
/// `sbHIGHER_PRIORITY` (`tskIDLE_PRIORITY + 1`).
pub const HIGHER_PRIORITY: u8 = 1;
/// `configMAX_PRIORITIES - 1`, which the trigger task runs at and which
/// `prvSingleTaskTests` raises itself to around its two timed calls.
pub const TOP_PRIORITY: u8 = 6;
/// `sbRX_TX_BLOCK_TIME`, `pdMS_TO_TICKS( 125 )` at 1 kHz.
pub const RX_TX_BLOCK_TIME: u64 = 125;
/// `sbDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;
/// `sbASCII_SPACE`.
const ASCII_SPACE: u8 = 32;
/// `sbASCII_TILDA`.
const ASCII_TILDA: u8 = 126;

/// `pc55ByteString`.
const PC55: &[u8] = b"One two three four five six seven eight nine ten eleven";
/// `pc54ByteString`.
const PC54: &[u8] = b"01234567891abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
/// `pcDataSentFromInterrupt`.
const FROM_INTERRUPT: &[u8] = b"0123456789";

/// `xBlockTime` in `prvSingleTaskTests`, `pdMS_TO_TICKS( 15 )`.
const BLOCK_TIME: u64 = 15;
/// `xMinimalBlockTime`.
const MINIMAL_BLOCK_TIME: u64 = 2;
/// `x6ByteLength`.
const SIX: usize = 6;
/// `x17ByteLength`.
const SEVENTEEN: usize = 17;
/// `xFullBufferSize`, and so the size of the one allocation the test makes.
const FULL_BUFFER: usize = BUFFER_BYTES * 2;
/// `sizeof( cRxString )` in `prvNonBlockingReceiverTask`.
const RX_STRING: usize = 12;
/// `xMax6ByteMessages`.
const MAX_6_BYTE_MESSAGES: usize = BUFFER_BYTES / SIX;
/// `sbSTREAM_BUFFER_LENGTH_BYTES - 1`, the length the tests fill and
/// drain the buffer by when they want it one byte from an end.
const BUFFER_LESS_ONE: usize = BUFFER_BYTES.saturating_sub(1);
/// Where `pucReadData` ends, seventeen bytes into the allocation.
const READ_SIX_END: usize = SEVENTEEN.saturating_add(SIX);

/// `xTicksToBlock` in `prvEchoServer`, `pdMS_TO_TICKS( 350 )`. The server
/// waits this long for data that cannot arrive before it creates its
/// client, so nothing in this scenario happens for the first 350 ticks.
const SERVER_FIRST_BLOCK: u64 = 350;
/// `xTicksToWait` in `prvEchoClient`, `pdMS_TO_TICKS( 50 )`.
const CLIENT_SEND_WAIT: u64 = 50;

/// `xReadBlockTime` in `prvInterruptTriggerLevelTest`.
const READ_BLOCK_TIME: u64 = 5;
/// `xCycleBlockTime`, `pdMS_TO_TICKS( 100 )`.
const CYCLE_BLOCK_TIME: u64 = 100;
/// `xStreamBufferSizeBytes` for the trigger-level buffer.
const TRIGGER_BUFFER_BYTES: usize = 9;
/// `xMinTriggerLevel`.
const MIN_TRIGGER_LEVEL: usize = 2;
/// `xMaxTriggerLevel`, exclusive.
const MAX_TRIGGER_LEVEL: usize = 7;
/// `xAllowableMargin`. `configSTREAM_BUFFER_TRIGGER_LEVEL_TEST_MARGIN` is
/// not defined by the harness, so upstream's `#else` arm does not apply and
/// the test demands the trigger level exactly.
const ALLOWABLE_MARGIN: usize = 0;

/// A `pc` that has run off the end of its state machine.
const DONE: u16 = u16::MAX;
/// The first of the two arms that close `prvSingleTaskTests`'
/// hundred-iteration loop. Numbered well clear of the straight-line arms so
/// that those keep the C's own order.
const LOOP_TAIL: u16 = 200;
/// The second, which jumps back to the loop head.
const LOOP_TAIL_END: u16 = LOOP_TAIL + 1;

/// `EchoStreamBuffers_t`.
///
/// In the C the server keeps this on its own stack and hands the client a
/// pointer to it. A `forbid(unsafe)` port cannot lend a task's locals to
/// another task, so both pairs live in [`State`] and each client is told
/// which one is its own when it is created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EchoBuffers {
    /// `xEchoClientBuffer` — client to server.
    pub client: StreamBufferHandle,
    /// `xEchoServerBuffer` — server back to client.
    pub server: StreamBufferHandle,
}

/// `StreamBufferDemo.c`'s file-scope variables.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// `xErrorStatus`.
    pub error_status: bool,
    /// `ulEchoLoopCounters`.
    pub echo_loop_counters: [u32; ECHO_CLIENTS],
    /// `ulInterruptTriggerCounter`.
    pub interrupt_trigger_counter: u32,
    /// `ulNonBlockingRxCounter`.
    pub non_blocking_rx_counter: u32,
    /// The two servers' buffer pairs. See [`EchoBuffers`].
    pub echo: [EchoBuffers; ECHO_CLIENTS],
    /// `ulLastEchoLoopCounters`, a static inside the check function.
    last_echo_loop_counters: [u32; ECHO_CLIENTS],
    /// `ulLastInterruptTriggerCounter`, likewise.
    last_interrupt_trigger_counter: u32,
    /// `ulLastNonBlockingRxCounter`, likewise.
    last_non_blocking_rx_counter: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            error_status: true,
            echo_loop_counters: [0; ECHO_CLIENTS],
            interrupt_trigger_counter: 0,
            non_blocking_rx_counter: 0,
            echo: [EchoBuffers::default(); ECHO_CLIENTS],
            last_echo_loop_counters: [0; ECHO_CLIENTS],
            last_interrupt_trigger_counter: 0,
            last_non_blocking_rx_counter: 0,
        }
    }
}

impl State {
    /// `prvCheckExpectedState`: a failure latches, it never clears.
    ///
    /// The C also passes the condition to `configASSERT`, which the harness
    /// defines as a no-op on the passing path, so there is nothing else to
    /// model.
    fn expect(&mut self, ok: bool) {
        if !ok {
            self.error_status = false;
        }
    }

    /// `xAreStreamBufferTasksStillRunning`, all three clauses.
    ///
    /// As everywhere in this corpus, the remembered count only advances
    /// when the demo *is* moving, so a stall latches rather than clears.
    pub fn still_running(&mut self) -> bool {
        for index in 0..ECHO_CLIENTS {
            let now = self.echo_loop_counters.get(index).copied().unwrap_or(0);
            let last = self.last_echo_loop_counters.get(index).copied().unwrap_or(0);
            if last == now {
                self.error_status = false;
            } else if let Some(slot) = self.last_echo_loop_counters.get_mut(index) {
                *slot = now;
            }
        }

        if self.last_non_blocking_rx_counter == self.non_blocking_rx_counter {
            self.error_status = false;
        } else {
            self.last_non_blocking_rx_counter = self.non_blocking_rx_counter;
        }

        if self.last_interrupt_trigger_counter == self.interrupt_trigger_counter {
            self.error_status = false;
        } else {
            self.last_interrupt_trigger_counter = self.interrupt_trigger_counter;
        }

        self.error_status
    }
}

/// `vPeriodicStreamBufferProcessing` — one byte per tick, when the trigger
/// test has a buffer installed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `xNextChar`.
    next_char: usize,
    /// `xInterruptStreamBuffer`, which the trigger task writes under a
    /// critical section exactly as the C does.
    pub buffer: StreamBufferHandle,
}

impl Isr {
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        if self.buffer == StreamBufferHandle::NULL {
            // The C's `else` arm: start the string again next time.
            self.next_char = 0;
            return self;
        }
        // `&pcDataSentFromInterrupt[ xNextChar ]`, one character. The index
        // is allowed to reach `strlen`, where it stops — so once the string
        // runs out the interrupt sends the terminating NUL over and over,
        // which the reader never gets far enough to see.
        let byte = FROM_INTERRUPT.get(self.next_char).copied().unwrap_or(0);
        let _ = k.stream_buffer_send_from_isr(self.buffer, &[byte]);
        if self.next_char < FROM_INTERRUPT.len() {
            self.next_char = self.next_char.saturating_add(1);
        }
        self
    }
}

/// The scenario's tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvEchoServer`.
    EchoServer(EchoServer),
    /// `prvEchoClient`.
    EchoClient(EchoClient),
    /// `prvInterruptTriggerLevelTest`.
    Trigger(Trigger),
    /// `prvNonBlockingSenderTask`.
    NonBlockingSender(NonBlockingSender),
    /// `prvNonBlockingReceiverTask`.
    NonBlockingReceiver(NonBlockingReceiver),
}

impl Body {
    /// `#[inline(never)]`, as every body in this corpus is, and it earns it.
    ///
    /// Removing it was measured: `StreamBufferDemo` gained 253,476 and
    /// `StreamBufferInterrupt` and `MessageBufferAMP` -- which never run a
    /// line of this file -- each LOST about 585,000. Inlined, this state
    /// machine lands inside `runner::Body::step`, the one dispatch every
    /// scenario goes through, and they all pay for its size.
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let spawn = &mut s.spawn;
        let runner::State::StreamBuffer(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::EchoServer(b) => b.step(k, state, spawn),
            Self::EchoClient(b) => b.step(k, state),
            Self::Trigger(b) => b.step(k, state),
            Self::NonBlockingSender(b) => b.step(k, state),
            Self::NonBlockingReceiver(b) => b.step(k, state),
        }
    }
}

// ------------------------------------------------- the non-blocking pair --

/// `prvNonBlockingSenderTask`.
///
/// Sends as much of `pc54ByteString` as will fit, never blocking, and wraps
/// at the end. One kernel call a pass, so one `pc` arm; everything else in
/// the C's loop is arithmetic on the caller's own index.
#[derive(Debug, Clone, Copy, Default)]
pub struct NonBlockingSender {
    /// `xStreamBuffer`, passed to the C through the task's parameter.
    buffer: StreamBufferHandle,
    /// `xNextChar`.
    next_char: usize,
}

impl NonBlockingSender {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        // xBytesToSend = xStringLength - xNextChar;
        let to_send = PC54.len().saturating_sub(self.next_char);
        let end = self.next_char.saturating_add(to_send).min(PC54.len());
        let chunk = PC54.get(self.next_char..end).unwrap_or(&[]);
        // xStreamBufferSend( ..., sbDONT_BLOCK );
        match k.stream_buffer_send(self.buffer, chunk, DONT_BLOCK) {
            Ok(Wait::Ready(sent)) => {
                s.expect(sent <= to_send);
                self.next_char = self.next_char.saturating_add(sent);
                s.expect(self.next_char <= PC54.len());
                if self.next_char == PC54.len() {
                    self.next_char = 0;
                }
            }
            // `sbDONT_BLOCK` cannot block, and a stale handle is the
            // scenario's problem rather than this loop's.
            Ok(Wait::Blocked) | Err(_) => {}
        }
        Step::Continue
    }
}

/// `prvNonBlockingReceiverTask`.
///
/// Expects `pc54ByteString` over and over. Neither end blocks, so what it
/// actually reads is whatever the sender managed to put in between the two
/// being scheduled -- which is the point, and which is why this pair is the
/// one that needed contract v2.
#[derive(Debug, Clone, Copy)]
pub struct NonBlockingReceiver {
    /// `xStreamBuffer`.
    buffer: StreamBufferHandle,
    /// `xNextChar`, the index into `pc54ByteString` already received.
    next_char: usize,
    /// `cRxString`.
    rx: [u8; RX_STRING],
    /// `xNonBlockingReceiveError`. A local of the C's `for(;;)`, so it is
    /// set once and never cleared: one bad compare stops the counter for
    /// the rest of the run.
    error: bool,
}

impl Default for NonBlockingReceiver {
    fn default() -> Self {
        Self {
            buffer: StreamBufferHandle::NULL,
            next_char: 0,
            rx: [0; RX_STRING],
            error: false,
        }
    }
}

impl NonBlockingReceiver {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let mut rx = [0_u8; RX_STRING];
        // xStreamBufferReceive( ..., sizeof( cRxString ), sbDONT_BLOCK );
        let received = match k.stream_buffer_receive(self.buffer, &mut rx, DONT_BLOCK) {
            Ok(Wait::Ready(count)) => count,
            Ok(Wait::Blocked) | Err(_) => return Step::Continue,
        };
        self.rx = rx;
        if received == 0 {
            return Step::Continue;
        }

        // The C compares in one or two pieces: if the received data runs
        // past the end of the string it checks up to the end, wraps the
        // index, and checks the remainder from the start.
        let mut to_test = received;
        let start_index;
        if self.next_char.saturating_add(to_test) > PC54.len() {
            to_test = PC54.len().saturating_sub(self.next_char);
            let want = PC54.get(self.next_char..self.next_char.saturating_add(to_test));
            if want != self.rx.get(..to_test) {
                self.error = true;
            }
            self.next_char = 0;
            start_index = to_test;
            to_test = received.saturating_sub(to_test);
        } else {
            start_index = 0;
        }

        let want = PC54.get(self.next_char..self.next_char.saturating_add(to_test));
        let got = self.rx.get(start_index..start_index.saturating_add(to_test));
        if want != got {
            self.error = true;
        }

        if !self.error {
            s.non_blocking_rx_counter = s.non_blocking_rx_counter.wrapping_add(1);
        }

        self.next_char = self.next_char.saturating_add(to_test);
        if self.next_char >= PC54.len() {
            self.next_char = 0;
        }
        Step::Continue
    }
}

// ------------------------------------------------------------ echo server --

/// `prvEchoServer`.
#[derive(Debug, Clone, Copy)]
pub struct EchoServer {
    pc: u16,
    /// Which of [`State::echo`] is this server's pair, and the index its
    /// client will count loops under.
    which: usize,
    /// `pcReceivedString`.
    received: [u8; BUFFER_BYTES],
    /// How many bytes the last receive brought in.
    received_len: usize,
    /// `xTimeOnEntering`.
    time_on_entering: u64,
    /// Which arm of the C's single `uxTaskPriorityGet( NULL )` test this
    /// server took. The C reads the priority ONCE and branches on it; a
    /// second read would be a second critical section, and so a second
    /// exit, which on the sim is the clock.
    lower: bool,
    /// `prvSingleTaskTests`, which only the higher-priority server runs.
    single: Single,
}

impl EchoServer {
    /// A server for one of the two echo pairs; `which` indexes
    /// [`State::echo`].
    #[must_use]
    pub fn new(which: usize) -> Self {
        Self {
            pc: 0,
            which,
            received: [0; BUFFER_BYTES],
            received_len: 0,
            time_on_entering: 0,
            lower: false,
            single: Single::default(),
        }
    }

    fn step<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        spawn: &mut Option<(rusty_rtos_core::handle::TaskHandle, Spawn)>,
    ) -> Step {
        let pair = s.echo.get(self.which).copied().unwrap_or_default();
        match self.pc {
            // xStreamBuffers.xEchoClientBuffer = xStreamBufferCreate(
            //     sbSTREAM_BUFFER_LENGTH_BYTES, sbTRIGGER_LEVEL_1 );
            0 => {
                if let Ok(buffer) = k.stream_buffer_create(BUFFER_BYTES, TRIGGER_LEVEL_1) {
                    if let Some(pair) = s.echo.get_mut(self.which) {
                        pair.client = buffer;
                    }
                }
                self.pc = 1;
            }
            // xStreamBuffers.xEchoServerBuffer = xStreamBufferCreate( ... );
            1 => {
                if let Ok(buffer) = k.stream_buffer_create(BUFFER_BYTES, TRIGGER_LEVEL_1) {
                    if let Some(pair) = s.echo.get_mut(self.which) {
                        pair.server = buffer;
                    }
                }
                self.pc = 2;
            }
            // pcReceivedString = pvPortMalloc( sbSTREAM_BUFFER_LENGTH_BYTES );
            2 => {
                allocate(k);
                self.pc = 3;
            }
            // xTimeOnEntering = xTaskGetTickCount();
            3 => {
                self.time_on_entering = k.tick_count();
                self.pc = 4;
            }
            // xReceivedLength = xStreamBufferReceive( xEchoClientBuffer,
            //     pcReceivedString, sbSTREAM_BUFFER_LENGTH_BYTES, xTicksToBlock );
            //
            // Nothing can have sent yet — the client does not exist — so this
            // is 350 ticks of nothing, deliberately, and it is why the
            // scenario needs the gate's full 2000-tick budget to show
            // anything at all.
            4 => match k.stream_buffer_receive(pair.client, &mut self.received, SERVER_FIRST_BLOCK)
            {
                Ok(Wait::Ready(count)) => {
                    // prvCheckExpectedState( ( xTaskGetTickCount() -
                    //     xTimeOnEntering ) >= xTicksToBlock );
                    let elapsed = k.tick_count().wrapping_sub(self.time_on_entering);
                    s.expect(elapsed >= SERVER_FIRST_BLOCK);
                    // prvCheckExpectedState( xReceivedLength == 0 );
                    s.expect(count == 0);
                    self.pc = 5;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 5,
            },
            // if( uxTaskPriorityGet( NULL ) == sbLOWER_PRIORITY )
            //
            // `task_priority_get` and not `priority_of`: the C API takes a
            // critical section and the internal field read does not.
            5 => {
                let priority = k.task_priority_get(None).unwrap_or(LOWER_PRIORITY);
                self.lower = priority == LOWER_PRIORITY;
                if self.lower {
                    // The lower-priority server skips the single-task tests
                    // and goes straight to creating its client.
                    self.pc = 7;
                } else {
                    self.pc = 6;
                }
            }
            // prvSingleTaskTests( xStreamBuffers.xEchoClientBuffer );
            //
            // Delegated rather than inlined: it is three hundred lines of
            // its own, and it has to be able to block in the middle.
            6 => {
                let step = self.single.step(k, s, pair.client);
                if self.single.pc == DONE {
                    self.pc = 7;
                }
                return step;
            }
            // xTaskCreate( prvEchoClient, "EchoClient", ..., &xStreamBuffers,
            //              sbLOWER_PRIORITY or sbHIGHER_PRIORITY, NULL );
            //
            // The client's priority is the opposite of this server's, so the
            // data crosses the boundary in both directions across the pair.
            7 => {
                let priority = if self.lower {
                    HIGHER_PRIORITY
                } else {
                    LOWER_PRIORITY
                };
                if let Ok(task) = k.create_task("EchoClient", priority) {
                    *spawn = Some((
                        task,
                        Spawn::StreamBuffer(Body::EchoClient(EchoClient::new(self.which))),
                    ));
                }
                self.pc = 8;
            }
            // memset( pcReceivedString, 0x00, sbSTREAM_BUFFER_LENGTH_BYTES );
            // xReceivedLength = xStreamBufferReceive( xEchoClientBuffer,
            //     pcReceivedString, sbSTREAM_BUFFER_LENGTH_BYTES, portMAX_DELAY );
            8 => {
                self.received = [0; BUFFER_BYTES];
                match k.stream_buffer_receive(
                    pair.client,
                    &mut self.received,
                    SimKernel::<W>::MAX_DELAY,
                ) {
                    Ok(Wait::Ready(count)) => {
                        // prvCheckExpectedState( xReceivedLength > 0 );
                        s.expect(count > 0);
                        self.received_len = count;
                        self.pc = 9;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 9,
                }
            }
            // xStreamBufferSend( xEchoServerBuffer, pcReceivedString,
            //                    xReceivedLength, portMAX_DELAY );
            _ => {
                let len = self.received_len.min(self.received.len());
                let echoed = self.received;
                let payload = echoed.get(..len).unwrap_or(&[]);
                match k.stream_buffer_send(pair.server, payload, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(_)) | Err(_) => self.pc = 8,
                    Ok(Wait::Blocked) => {}
                }
            }
        }
        Step::Continue
    }
}

// ------------------------------------------------------------ echo client --

/// `prvEchoClient`.
#[derive(Debug, Clone, Copy)]
pub struct EchoClient {
    pc: u16,
    /// Which of [`State::echo`] to talk to.
    which: usize,
    /// `uxIndex`, the task's own priority, used to index the loop counters.
    index: usize,
    /// `pcStringToSend`.
    to_send: [u8; BUFFER_BYTES],
    /// `pcStringReceived`.
    received: [u8; BUFFER_BYTES],
    /// `xSendLength`.
    send_length: usize,
    /// `cNextChar`.
    next_char: u8,
    /// `xTempStreamBuffer`, the buffer of size one the loop tail exercises.
    temp: StreamBufferHandle,
}

impl EchoClient {
    /// The client of the pair its server was given, which is the pair it
    /// talks to.
    #[must_use]
    pub fn new(which: usize) -> Self {
        Self {
            pc: 0,
            which,
            index: 0,
            to_send: [0; BUFFER_BYTES],
            received: [0; BUFFER_BYTES],
            send_length: 0,
            next_char: ASCII_SPACE,
            temp: StreamBufferHandle::NULL,
        }
    }

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let pair = s.echo.get(self.which).copied().unwrap_or_default();
        match self.pc {
            // uxIndex = uxTaskPriorityGet( NULL );
            //
            // The two clients run at priorities 0 and 1, so the priority IS
            // the index into ulEchoLoopCounters. Upstream says so.
            0 => {
                self.index = usize::from(k.task_priority_get(None).unwrap_or(0)).min(ECHO_CLIENTS - 1);
                self.pc = 1;
            }
            // pcStringToSend = pvPortMalloc( sbSTREAM_BUFFER_LENGTH_BYTES );
            1 => {
                allocate(k);
                self.pc = 2;
            }
            // pcStringReceived = pvPortMalloc( sbSTREAM_BUFFER_LENGTH_BYTES );
            2 => {
                allocate(k);
                self.pc = 3;
            }
            // xSendLength++;
            // if( xSendLength > ( sbSTREAM_BUFFER_LENGTH_BYTES - sizeof( size_t ) ) )
            //     xSendLength = sizeof( char );
            // memset( ... ); for( ux = 0; ux < xSendLength; ux++ ) { ... }
            //
            // `sizeof( size_t )` is 8 on the 64-bit oracle, so the length
            // walks 1..=22 and then starts again. None of this touches the
            // kernel, so it is one arm however many statements it is.
            3 => {
                self.send_length = self.send_length.saturating_add(1);
                if self.send_length > BUFFER_BYTES.saturating_sub(core::mem::size_of::<usize>()) {
                    self.send_length = 1;
                }
                self.to_send = [0; BUFFER_BYTES];
                for index in 0..self.send_length.min(BUFFER_BYTES) {
                    if let Some(slot) = self.to_send.get_mut(index) {
                        *slot = self.next_char;
                    }
                    self.next_char = self.next_char.saturating_add(1);
                    if self.next_char > ASCII_TILDA {
                        self.next_char = ASCII_SPACE;
                    }
                }
                self.pc = 4;
            }
            // do { ux = xStreamBufferSend( xEchoClientBuffer, pcStringToSend,
            //      xSendLength, xTicksToWait ); } while( ux == 0 );
            //
            // A do-while around a call that can time out: the arm loops back
            // to itself until something is sent.
            4 => {
                let len = self.send_length.min(BUFFER_BYTES);
                let payload = self.to_send;
                let slice = payload.get(..len).unwrap_or(&[]);
                match k.stream_buffer_send(pair.client, slice, CLIENT_SEND_WAIT) {
                    Ok(Wait::Ready(sent)) => {
                        if sent != 0 {
                            self.pc = 5;
                        }
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 5,
                }
            }
            // memset( pcStringReceived, 0x00, sbSTREAM_BUFFER_LENGTH_BYTES );
            // xStreamBufferReceive( xEchoServerBuffer, pcStringReceived,
            //                       xSendLength, portMAX_DELAY );
            5 => {
                let len = self.send_length.min(BUFFER_BYTES);
                let mut scratch = [0_u8; BUFFER_BYTES];
                let Some(window) = scratch.get_mut(..len) else {
                    self.pc = 6;
                    return Step::Continue;
                };
                match k.stream_buffer_receive(pair.server, window, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(_)) => {
                        self.received = scratch;
                        // prvCheckExpectedState( strcmp( pcStringToSend,
                        //                                pcStringReceived ) == 0 );
                        s.expect(self.received == self.to_send);
                        // ulEchoLoopCounters[ uxIndex ]++;
                        if let Some(counter) = s.echo_loop_counters.get_mut(self.index) {
                            *counter = counter.wrapping_add(1);
                        }
                        self.pc = 6;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 6,
                }
            }
            // xTempStreamBuffer = xStreamBufferCreate(
            //     sbSTREAM_BUFFER_LENGTH_BYTES, sbTRIGGER_LEVEL_1 );
            6 => {
                self.temp = k
                    .stream_buffer_create(BUFFER_BYTES, TRIGGER_LEVEL_1)
                    .unwrap_or(StreamBufferHandle::NULL);
                self.pc = 7;
            }
            // vStreamBufferDelete( xTempStreamBuffer );
            //
            // Created and deleted for no other reason than to prove the pair
            // leaks nothing.
            7 => {
                let _ = k.stream_buffer_delete(self.temp);
                self.pc = 8;
            }
            // xTempStreamBuffer = xStreamBufferCreate(
            //     sbSTREAM_BUFFER_LENGTH_ONE, sbTRIGGER_LEVEL_1 );
            8 => {
                self.temp = k
                    .stream_buffer_create(BUFFER_LENGTH_ONE, TRIGGER_LEVEL_1)
                    .unwrap_or(StreamBufferHandle::NULL);
                self.pc = 9;
            }
            // ux = xStreamBufferSend( xTempStreamBuffer, pcStringToSend, 1,
            //                         sbDONT_BLOCK );  configASSERT( ux == 1 );
            9 => {
                let payload = self.to_send;
                if let Ok(Wait::Ready(sent)) =
                    k.stream_buffer_send(self.temp, &payload[..1], DONT_BLOCK)
                {
                    s.expect(sent == 1);
                }
                self.pc = 10;
            }
            // ux = xStreamBufferSend( ... 1 ... );  configASSERT( ux == 0 );
            // The buffer holds one byte and already has it.
            10 => {
                let payload = self.to_send;
                if let Ok(Wait::Ready(sent)) =
                    k.stream_buffer_send(self.temp, &payload[..1], DONT_BLOCK)
                {
                    s.expect(sent == 0);
                }
                self.pc = 11;
            }
            // memset( pcStringReceived, ... );
            // ux = xStreamBufferReceive( xTempStreamBuffer, pcStringReceived,
            //                            1, sbDONT_BLOCK );
            // configASSERT( ux == 1 );
            // configASSERT( pcStringToSend[ 0 ] == pcStringReceived[ 0 ] );
            11 => {
                self.received = [0; BUFFER_BYTES];
                let mut one = [0_u8; 1];
                if let Ok(Wait::Ready(count)) =
                    k.stream_buffer_receive(self.temp, &mut one, DONT_BLOCK)
                {
                    s.expect(count == 1);
                    let byte = one.first().copied().unwrap_or(0);
                    s.expect(self.to_send.first().copied() == Some(byte));
                }
                self.pc = 12;
            }
            // ux = xStreamBufferReceive( ... 1 ... );  configASSERT( ux == 0 );
            12 => {
                let mut one = [0_u8; 1];
                if let Ok(Wait::Ready(count)) =
                    k.stream_buffer_receive(self.temp, &mut one, DONT_BLOCK)
                {
                    s.expect(count == 0);
                }
                self.pc = 13;
            }
            // ux = xStreamBufferSend( ... 2 ... );  configASSERT( ux == 1 );
            // Two bytes into a one-byte buffer takes one.
            13 => {
                let payload = self.to_send;
                if let Ok(Wait::Ready(sent)) =
                    k.stream_buffer_send(self.temp, &payload[..2], DONT_BLOCK)
                {
                    s.expect(sent == 1);
                }
                self.pc = 14;
            }
            // memset( pcStringReceived, ... );
            // ux = xStreamBufferReceive( ... 2 ... );  configASSERT( ux == 1 );
            // configASSERT( pcStringToSend[ 0 ] == pcStringReceived[ 0 ] );
            14 => {
                self.received = [0; BUFFER_BYTES];
                let mut two = [0_u8; 2];
                if let Ok(Wait::Ready(count)) =
                    k.stream_buffer_receive(self.temp, &mut two, DONT_BLOCK)
                {
                    s.expect(count == 1);
                    let byte = two.first().copied().unwrap_or(0);
                    s.expect(self.to_send.first().copied() == Some(byte));
                }
                self.pc = 15;
            }
            // vStreamBufferDelete( xTempStreamBuffer );  and round again.
            _ => {
                let _ = k.stream_buffer_delete(self.temp);
                self.pc = 3;
            }
        }
        Step::Continue
    }
}

// --------------------------------------------------------- trigger levels --

/// `prvInterruptTriggerLevelTest`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Trigger {
    pc: u16,
    /// `xTriggerLevel`.
    trigger_level: usize,
    /// `xStreamBuffer`, this pass's buffer.
    buffer: StreamBufferHandle,
    /// `ucRxData`.
    rx: [u8; TRIGGER_BUFFER_BYTES],
    /// `xBytesReceived`.
    bytes_received: usize,
    /// `xErrorDetected`.
    ///
    /// A local of the C's outer `for( ;; )`, so it is set once and never
    /// cleared: one bad pass stops the counter for the rest of the run.
    error_detected: bool,
}

impl Trigger {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        match self.pc {
            // for( xTriggerLevel = xMinTriggerLevel; ... ) — entry, and the
            // point the outer forever-loop comes back to.
            0 => {
                self.trigger_level = MIN_TRIGGER_LEVEL;
                self.pc = 1;
            }
            1 => {
                if self.trigger_level >= MAX_TRIGGER_LEVEL {
                    self.pc = 0;
                } else {
                    self.pc = 2;
                }
            }
            // vTaskDelay( xCycleBlockTime );
            //
            // At the top of every pass so the interrupt half sees the buffer
            // go NULL and restarts the string from the beginning.
            2 => {
                let _ = k.delay(CYCLE_BLOCK_TIME);
                self.pc = 3;
            }
            // memset( ucRxData, 0x00, sizeof( ucRxData ) );
            // xStreamBuffer = xStreamBufferCreate( xStreamBufferSizeBytes,
            //                                      xTriggerLevel );
            3 => {
                self.rx = [0; TRIGGER_BUFFER_BYTES];
                self.buffer = k
                    .stream_buffer_create(TRIGGER_BUFFER_BYTES, self.trigger_level)
                    .unwrap_or(StreamBufferHandle::NULL);
                self.pc = 4;
            }
            // taskENTER_CRITICAL(); xInterruptStreamBuffer = xStreamBuffer;
            // taskEXIT_CRITICAL();
            //
            // The interrupt half keeps the handle, so the assignment reaches
            // it through the tick hook — under the same critical section the
            // C uses, because that is where a tick may fire.
            4 => {
                k.enter_critical();
                if let TickIsr::StreamBuffer(isr) = k.tick_hook_mut() {
                    isr.buffer = self.buffer;
                }
                k.exit_critical();
                self.pc = 5;
            }
            // xBytesReceived = xStreamBufferReceive( xStreamBuffer, ucRxData,
            //     sizeof( ucRxData ), xReadBlockTime );
            5 => {
                let mut rx = [0_u8; TRIGGER_BUFFER_BYTES];
                match k.stream_buffer_receive(self.buffer, &mut rx, READ_BLOCK_TIME) {
                    Ok(Wait::Ready(count)) => {
                        self.rx = rx;
                        self.bytes_received = count;
                        self.pc = 6;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.bytes_received = 0;
                        self.pc = 6;
                    }
                }
            }
            // taskENTER_CRITICAL(); xInterruptStreamBuffer = NULL;
            // taskEXIT_CRITICAL();
            6 => {
                k.enter_critical();
                if let TickIsr::StreamBuffer(isr) = k.tick_hook_mut() {
                    isr.buffer = StreamBufferHandle::NULL;
                }
                k.exit_critical();
                self.pc = 7;
            }
            // The three-way comparison of the trigger level against the read
            // block time, then the data check and the counter. No kernel
            // call in any of it, so it is one arm.
            7 => {
                self.judge();
                self.pc = 8;
            }
            // vStreamBufferDelete( xStreamBuffer );  xTriggerLevel++;
            _ => {
                let _ = k.stream_buffer_delete(self.buffer);
                self.buffer = StreamBufferHandle::NULL;
                if !self.error_detected {
                    s.interrupt_trigger_counter = s.interrupt_trigger_counter.wrapping_add(1);
                }
                self.trigger_level = self.trigger_level.saturating_add(1);
                self.pc = 1;
            }
        }
        Step::Continue
    }

    /// The C's verdict on one pass.
    ///
    /// One byte arrives per tick, so with a block time of `xReadBlockTime`
    /// the reader wakes either at the trigger level or at the timeout,
    /// whichever comes first — and the three arms are the three orderings.
    /// Each allows the single case of "the interrupt got there first", in
    /// which the receive returned immediately with exactly one byte.
    fn judge(&mut self) {
        let received = self.bytes_received;
        let block = READ_BLOCK_TIME as usize;
        let trigger = self.trigger_level;

        if trigger > block {
            // Timed out before the trigger level was reached.
            if received > block {
                if received.saturating_sub(block) > ALLOWABLE_MARGIN {
                    self.error_detected = true;
                }
            } else if block != received && received != 1 {
                self.error_detected = true;
            }
        } else if trigger < block {
            // Woken at the trigger level.
            if received < trigger {
                if received != 1 {
                    self.error_detected = true;
                }
            } else if received.saturating_sub(trigger) > ALLOWABLE_MARGIN {
                self.error_detected = true;
            }
        } else {
            // The two are equal, so either could have fired first -- and
            // the C's arm for that case is, statement for statement, the
            // one above with `xReadBlockTime` in place of `xTriggerLevel`.
            // They are the same test because the two values are the same
            // number here, which is what makes this the `else`.
            if received < block {
                if received != 1 {
                    self.error_detected = true;
                }
            } else if received.saturating_sub(block) > ALLOWABLE_MARGIN {
                self.error_detected = true;
            }
        }

        // The C guards the compare with the length test for a reason:
        // `memcmp` over more bytes than `ucRxData` holds would read off the
        // end. A slice cannot, but `get` would answer `None` on both sides
        // and the mismatch would pass unnoticed -- so the guard has to stay,
        // and short-circuit, rather than becoming one comparison.
        let too_many = received > self.rx.len();
        let wrong_data =
            !too_many && self.rx.get(..received) != FROM_INTERRUPT.get(..received);
        if too_many || wrong_data {
            self.error_detected = true;
        }
    }
}

// ------------------------------------------------------ single-task tests --

/// `prvSingleTaskTests` — the head-and-tail arithmetic, run once by the
/// higher-priority echo server before it creates its client.
///
/// This is where five of the six APIs the scenario exists to close are
/// called from. It walks the buffer's head past its tail and back, fills
/// and empties it by one byte, by six, by seventeen and in one go, and
/// times two calls that must block for exactly as long as they were asked
/// to.
///
/// The C overlays two pointers on one 60-byte allocation — `pucData` at the
/// start and `pucReadData` seventeen bytes in — and the port keeps that
/// layout, because a read back into the same block at an offset is part of
/// what the test is checking.
#[derive(Debug, Clone, Copy)]
pub struct Single {
    pc: u16,
    /// `pucFullBuffer`, with `pucData` at 0 and `pucReadData` at 17.
    full: [u8; FULL_BUFFER],
    /// `xExpectedSpaces`.
    expected_spaces: usize,
    /// `xExpectedBytes`.
    expected_bytes: usize,
    /// `xExpected`, which the loops carry.
    expected: usize,
    /// `xItem`.
    item: usize,
    /// `uxOriginalPriority`.
    original_priority: u8,
    /// `xTimeBeforeCall`.
    time_before: u64,
    /// `xReturned`, where an arm has to outlive the call that set it.
    returned: usize,
}

impl Default for Single {
    fn default() -> Self {
        Self {
            pc: 0,
            full: [0; FULL_BUFFER],
            expected_spaces: 0,
            expected_bytes: 0,
            expected: 0,
            item: 0,
            original_priority: HIGHER_PRIORITY,
            time_before: 0,
            returned: 0,
        }
    }
}

impl Single {
    /// `xExpectedSpaces -= n; xExpectedBytes += n;` — bytes went IN.
    ///
    /// Saturating rather than plain, because the house lint forbids a bare
    /// `-` on a `usize` and this is bookkeeping the test compares against
    /// the kernel: a wrap here would hide the mismatch it exists to find.
    fn took(&mut self, bytes: usize) {
        self.expected_spaces = self.expected_spaces.saturating_sub(bytes);
        self.expected_bytes = self.expected_bytes.saturating_add(bytes);
    }

    /// `xExpectedSpaces += n; xExpectedBytes -= n;` — bytes came OUT.
    fn gave(&mut self, bytes: usize) {
        self.expected_spaces = self.expected_spaces.saturating_add(bytes);
        self.expected_bytes = self.expected_bytes.saturating_sub(bytes);
    }

    /// `pucData`, the first `len` bytes of the allocation.
    fn data(&self, len: usize) -> &[u8] {
        let end = len.min(FULL_BUFFER);
        self.full.get(..end).unwrap_or(&[])
    }

    /// One arm of the machine. `self.pc == DONE` when the test is over.
    #[allow(clippy::too_many_lines)]
    fn step<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
    ) -> Step {
        match self.pc {
            // pucFullBuffer = pvPortMalloc( xFullBufferSize );
            0 => {
                allocate(k);
                self.expected_spaces = BUFFER_BYTES;
                self.expected_bytes = 0;
                self.pc = 1;
            }

            // --- nothing added or removed yet; head and tail both at 0 ---
            1 => self.check_spaces(k, s, buffer, self.expected_spaces, 2),
            2 => self.check_bytes(k, s, buffer, self.expected_bytes, 3),
            3 => self.check_empty(k, s, buffer, true, 4),
            4 => self.check_full(k, s, buffer, false, 5),

            // --- add a single item; head in front of tail ---
            5 => {
                self.took(1);
                self.send_expecting(k, s, buffer, 1, DONT_BLOCK, 1, 6);
            }
            6 => self.check_spaces(k, s, buffer, self.expected_spaces, 7),
            7 => self.check_bytes(k, s, buffer, self.expected_bytes, 8),
            8 => self.check_empty(k, s, buffer, false, 9),
            9 => self.check_full(k, s, buffer, false, 10),

            // --- fill it with another 29; head 30, tail 0 ---
            10 => {
                self.took(BUFFER_LESS_ONE);
                self.send_expecting(k, s, buffer, BUFFER_LESS_ONE, DONT_BLOCK, BUFFER_LESS_ONE, 11);
            }
            11 => self.check_spaces(k, s, buffer, self.expected_spaces, 12),
            12 => self.check_bytes(k, s, buffer, self.expected_bytes, 13),
            13 => self.check_empty(k, s, buffer, false, 14),
            14 => self.check_full(k, s, buffer, true, 15),

            // --- should not be able to add another byte now ---
            15 => self.send_expecting(k, s, buffer, 1, DONT_BLOCK, 0, 16),

            // --- remove one so the tail moves off 0 ---
            16 => {
                self.gave(1);
                self.receive_expecting(k, s, buffer, 0, 1, DONT_BLOCK, 1, 17);
            }
            17 => self.check_spaces(k, s, buffer, self.expected_spaces, 18),
            18 => self.check_bytes(k, s, buffer, self.expected_bytes, 19),
            19 => self.check_empty(k, s, buffer, false, 20),
            20 => self.check_full(k, s, buffer, false, 21),

            // --- and fill it again ---
            21 => {
                self.took(1);
                self.send_expecting(k, s, buffer, 1, DONT_BLOCK, 1, 22);
            }
            22 => self.check_spaces(k, s, buffer, self.expected_spaces, 23),
            23 => self.check_bytes(k, s, buffer, self.expected_bytes, 24),
            24 => self.check_empty(k, s, buffer, false, 25),
            25 => self.check_full(k, s, buffer, true, 26),

            // --- head is now behind tail; read 29 so the tail reaches the end ---
            26 => {
                self.gave(BUFFER_LESS_ONE);
                self.receive_expecting(
                    k,
                    s,
                    buffer,
                    0,
                    BUFFER_LESS_ONE,
                    DONT_BLOCK,
                    BUFFER_LESS_ONE,
                    27,
                );
            }
            27 => self.check_spaces(k, s, buffer, self.expected_spaces, 28),
            28 => self.check_bytes(k, s, buffer, self.expected_bytes, 29),
            29 => self.check_empty(k, s, buffer, false, 30),
            30 => self.check_full(k, s, buffer, false, 31),

            // --- one more to wrap the tail back to the start ---
            31 => {
                self.gave(1);
                self.receive_expecting(k, s, buffer, 0, 1, DONT_BLOCK, 1, 32);
            }
            32 => self.check_spaces(k, s, buffer, self.expected_spaces, 33),
            33 => self.check_bytes(k, s, buffer, self.expected_bytes, 34),
            34 => self.check_empty(k, s, buffer, true, 35),
            35 => self.check_full(k, s, buffer, false, 36),

            // --- fill in one write, blocking for ever; one byte less goes in ---
            36 => {
                self.expected_spaces = 0;
                self.send_expecting(
                    k,
                    s,
                    buffer,
                    TRUE_SIZE,
                    SimKernel::<W>::MAX_DELAY,
                    TRUE_SIZE - 1,
                    37,
                );
            }
            37 => self.check_spaces(k, s, buffer, 0, 38),
            38 => self.check_empty(k, s, buffer, false, 39),
            39 => self.check_full(k, s, buffer, true, 40),

            // --- empty it again, asking for more than can be there ---
            40 => self.receive_expecting(
                k,
                s,
                buffer,
                0,
                TRUE_SIZE,
                SimKernel::<W>::MAX_DELAY,
                TRUE_SIZE - 1,
                41,
            ),
            41 => self.check_spaces(k, s, buffer, BUFFER_BYTES, 42),
            42 => self.check_bytes(k, s, buffer, 0, 43),
            43 => self.check_empty(k, s, buffer, true, 44),
            44 => self.check_full(k, s, buffer, false, 45),

            // --- five six-byte messages fill the buffer exactly ---
            45 => {
                self.expected = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                self.item = 0;
                self.pc = 46;
            }
            46 => {
                if self.item >= MAX_6_BYTE_MESSAGES {
                    self.pc = 50;
                } else {
                    self.check_full(k, s, buffer, false, 47);
                }
            }
            // The FromISR form inside a critical section, which is how a
            // port without interrupt-safe critical sections would have to
            // call it — upstream uses it here purely for the exercise.
            47 => {
                self.fill(SIX);
                let payload = self.data(SIX);
                k.enter_critical();
                let sent = k
                    .stream_buffer_send_from_isr(buffer, payload)
                    .map_or(0, |(count, _)| count);
                k.exit_critical();
                s.expect(sent == SIX);
                self.pc = 48;
            }
            48 => {
                self.expected = self.expected.saturating_sub(SIX);
                self.check_spaces(k, s, buffer, self.expected, 49);
            }
            49 => {
                let want = self.item.saturating_add(1).saturating_mul(SIX);
                self.check_bytes(k, s, buffer, want, 46);
                self.item = self.item.saturating_add(1);
            }

            // --- full now, so a send must fail ---
            50 => self.check_full(k, s, buffer, true, 51),
            51 => self.send_expecting(k, s, buffer, 1, DONT_BLOCK, 0, 52),

            // --- and a send with a timeout must fail after exactly that long.
            //     The priority is boosted so nothing else can delay the wake. ---
            52 => {
                self.original_priority = k.task_priority_get(None).unwrap_or(HIGHER_PRIORITY);
                self.pc = 53;
            }
            53 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 54;
            }
            54 => {
                self.time_before = k.tick_count();
                self.pc = 55;
            }
            55 => {
                let payload = self.data(1);
                match k.stream_buffer_send(buffer, payload, BLOCK_TIME) {
                    Ok(Wait::Ready(sent)) => {
                        self.returned = sent;
                        self.pc = 56;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.returned = 0;
                        self.pc = 56;
                    }
                }
            }
            56 => {
                let elapsed = k.tick_count().wrapping_sub(self.time_before);
                let _ = k.set_priority(None, self.original_priority);
                s.expect(elapsed >= BLOCK_TIME);
                s.expect(self.returned == 0);
                self.item = 0;
                self.pc = 57;
            }

            // --- read the five messages back, again through the ISR form ---
            57 => {
                if self.item >= MAX_6_BYTE_MESSAGES {
                    self.pc = 60;
                } else {
                    self.fill(SIX);
                    let mut read = [0_u8; SIX];
                    k.enter_critical();
                    let count = k
                        .stream_buffer_receive_from_isr(buffer, &mut read)
                        .map_or(0, |(count, _)| count);
                    k.exit_critical();
                    s.expect(count == SIX);
                    // pucReadData is pucData + 17, so the compare is between
                    // two windows on the same allocation.
                    self.write_at(SEVENTEEN, &read);
                    let matched = self.data(SIX) == self.full.get(SEVENTEEN..READ_SIX_END).unwrap_or(&[]);
                    s.expect(matched);
                    self.pc = 58;
                }
            }
            58 => {
                self.expected = self.expected.saturating_add(SIX);
                self.check_spaces(k, s, buffer, self.expected, 59);
            }
            59 => {
                let want = BUFFER_BYTES.saturating_sub(self.expected);
                self.check_bytes(k, s, buffer, want, 57);
                self.item = self.item.saturating_add(1);
            }

            // --- empty again ---
            60 => self.check_empty(k, s, buffer, true, 61),
            61 => self.check_spaces(k, s, buffer, BUFFER_BYTES, 62),

            // --- and a receive with a timeout must also fail after
            //     exactly that long ---
            62 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 63;
            }
            63 => {
                self.time_before = k.tick_count();
                self.pc = 64;
            }
            64 => {
                let mut read = [0_u8; SIX];
                match k.stream_buffer_receive(buffer, &mut read, BLOCK_TIME) {
                    Ok(Wait::Ready(count)) => {
                        self.returned = count;
                        self.pc = 65;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.returned = 0;
                        self.pc = 65;
                    }
                }
            }
            65 => {
                let elapsed = k.tick_count().wrapping_sub(self.time_before);
                let _ = k.set_priority(None, self.original_priority);
                s.expect(elapsed >= BLOCK_TIME);
                s.expect(self.returned == 0);
                self.expected = BUFFER_BYTES - SEVENTEEN;
                self.item = 0;
                self.pc = 66;
            }

            // --- 17 bytes in and out a hundred times; 30 is not divisible by
            //     17, so the data wraps at a different place every pass ---
            66 => {
                if self.item >= 100 {
                    self.pc = 74;
                } else {
                    self.fill(SEVENTEEN);
                    self.send_expecting(k, s, buffer, SEVENTEEN, DONT_BLOCK, SEVENTEEN, 67);
                }
            }
            67 => self.check_spaces(k, s, buffer, self.expected, 68),
            68 => self.check_bytes(k, s, buffer, SEVENTEEN, 69),
            69 => self.check_full(k, s, buffer, false, 70),
            70 => self.check_empty(k, s, buffer, false, 71),
            71 => {
                let mut read = [0_u8; SEVENTEEN];
                let count = match k.stream_buffer_receive(buffer, &mut read, DONT_BLOCK) {
                    Ok(Wait::Ready(count)) => count,
                    Ok(Wait::Blocked) => return Step::Continue,
                    Err(_) => 0,
                };
                s.expect(count == SEVENTEEN);
                self.write_at(SEVENTEEN, &read);
                let matched = self.data(SEVENTEEN)
                    == self.full.get(SEVENTEEN..SEVENTEEN * 2).unwrap_or(&[]);
                s.expect(matched);
                self.pc = 72;
            }
            72 => self.check_spaces(k, s, buffer, BUFFER_BYTES, 73),
            73 => self.check_bytes(k, s, buffer, 0, LOOP_TAIL),

            // Everything past the hundred-iteration loop, plus that loop's
            // own last two arms, lives in `step_tail` — see its note.
            _ => return self.step_tail(k, s, buffer),
        }
        Step::Continue
    }

    /// The arms past the hundred-iteration loop, split out only so that one
    /// `match` does not grow past what is readable in one screen.
    ///
    /// The loop's own last two arms are here too, numbered from
    /// [`LOOP_TAIL`] rather than squeezed in below 74, so that the
    /// post-loop arms keep the order they have in the C.
    #[allow(clippy::too_many_lines)]
    fn step_tail<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
    ) -> Step {
        match self.pc {
            // prvCheckExpectedState( xStreamBufferIsFull( ... ) == pdFALSE );
            LOOP_TAIL => self.check_full(k, s, buffer, false, LOOP_TAIL + 1),
            // prvCheckExpectedState( xStreamBufferIsEmpty( ... ) == pdTRUE );
            // ... and round again.
            LOOP_TAIL_END => {
                self.check_empty(k, s, buffer, true, 66);
                self.item = self.item.saturating_add(1);
            }

            // --- fill the buffer with one message, then read it back ---
            74 => {
                let mut chunk = [0_u8; BUFFER_BYTES];
                chunk.copy_from_slice(PC55.get(..BUFFER_BYTES).unwrap_or(&[0; BUFFER_BYTES]));
                match k.stream_buffer_send(buffer, &chunk, DONT_BLOCK) {
                    Ok(Wait::Ready(_)) | Err(_) => self.pc = 75,
                    Ok(Wait::Blocked) => {}
                }
            }
            75 => {
                let mut read = [0_u8; BUFFER_BYTES];
                match k.stream_buffer_receive(buffer, &mut read, DONT_BLOCK) {
                    Ok(Wait::Ready(_)) | Err(_) => {
                        self.write_at(0, &read);
                        s.expect(
                            self.data(BUFFER_BYTES) == PC55.get(..BUFFER_BYTES).unwrap_or(&[]),
                        );
                        self.item = 0;
                        self.pc = 76;
                    }
                    Ok(Wait::Blocked) => {}
                }
            }
            // --- fill it one byte at a time from pc54ByteString. The block
            //     time is there for coverage; the task never actually
            //     blocks, because the buffer starts empty. ---
            76 => {
                if self.item >= BUFFER_BYTES {
                    self.pc = 77;
                } else {
                    let byte = PC54.get(self.item).copied().unwrap_or(0);
                    match k.stream_buffer_send(buffer, &[byte], RX_TX_BLOCK_TIME) {
                        Ok(Wait::Ready(_)) | Err(_) => self.item = self.item.saturating_add(1),
                        Ok(Wait::Blocked) => {}
                    }
                }
            }
            77 => self.check_full(k, s, buffer, true, 79),

            // --- read the whole thing back in one go, asking for twice what
            //     can be there ---
            79 => {
                let mut read = [0_u8; FULL_BUFFER];
                match k.stream_buffer_receive(buffer, &mut read, RX_TX_BLOCK_TIME) {
                    Ok(Wait::Ready(count)) => {
                        self.write_at(0, &read);
                        s.expect(count == BUFFER_BYTES);
                        s.expect(self.data(BUFFER_BYTES) == PC54.get(..BUFFER_BYTES).unwrap_or(&[]));
                        self.pc = 80;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 80,
                }
            }

            // --- now the opposite: one write, then read it out byte by byte ---
            80 => {
                let mut chunk = [0_u8; BUFFER_BYTES];
                chunk.copy_from_slice(PC55.get(..BUFFER_BYTES).unwrap_or(&[0; BUFFER_BYTES]));
                match k.stream_buffer_send(buffer, &chunk, RX_TX_BLOCK_TIME) {
                    Ok(Wait::Ready(sent)) => {
                        s.expect(sent == BUFFER_BYTES);
                        self.pc = 81;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 81,
                }
            }
            81 => self.check_full(k, s, buffer, true, 82),
            82 => self.check_empty(k, s, buffer, false, 83),
            83 => self.check_bytes(k, s, buffer, BUFFER_BYTES, 84),
            84 => {
                self.check_spaces(k, s, buffer, 0, 85);
                self.item = 0;
            }
            85 => {
                if self.item >= BUFFER_BYTES {
                    self.pc = 86;
                } else {
                    let mut one = [0_u8; 1];
                    match k.stream_buffer_receive(buffer, &mut one, RX_TX_BLOCK_TIME) {
                        Ok(Wait::Ready(_)) | Err(_) => {
                            let want = PC55.get(self.item).copied().unwrap_or(0);
                            s.expect(Some(want) == one.first().copied());
                            self.item = self.item.saturating_add(1);
                        }
                        Ok(Wait::Blocked) => {}
                    }
                }
            }
            86 => self.check_empty(k, s, buffer, true, 87),
            87 => self.check_full(k, s, buffer, false, 88),

            // --- write more bytes than there is space for ---
            88 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 89;
            }
            89 => {
                let source = oversized(PC54);
                match k.stream_buffer_send(buffer, &source, MINIMAL_BLOCK_TIME) {
                    Ok(Wait::Ready(sent)) => {
                        self.returned = sent;
                        self.pc = 90;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.returned = 0;
                        self.pc = 90;
                    }
                }
            }
            90 => {
                let _ = k.set_priority(None, self.original_priority);
                s.expect(self.returned == BUFFER_BYTES);
                self.pc = 91;
            }
            91 => self.check_full(k, s, buffer, true, 92),
            92 => self.check_empty(k, s, buffer, false, 93),

            // --- no space now, so the same call takes nothing ---
            93 => {
                let source = oversized(PC54);
                match k.stream_buffer_send(buffer, &source, MINIMAL_BLOCK_TIME) {
                    Ok(Wait::Ready(sent)) => {
                        s.expect(sent == 0);
                        self.pc = 94;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 94,
                }
            }

            // --- and the data went in as expected even so ---
            94 => {
                let mut read = [0_u8; FULL_BUFFER];
                match k.stream_buffer_receive(buffer, &mut read, MINIMAL_BLOCK_TIME) {
                    Ok(Wait::Ready(_)) | Err(_) => {
                        self.write_at(0, &read);
                        s.expect(self.data(BUFFER_BYTES) == PC54.get(..BUFFER_BYTES).unwrap_or(&[]));
                        self.pc = 95;
                    }
                    Ok(Wait::Blocked) => {}
                }
            }
            95 => self.check_full(k, s, buffer, false, 96),
            96 => self.check_empty(k, s, buffer, true, 97),

            // --- leave data behind, so the tests that follow would see it if
            //     the reset did not discard it ---
            97 => {
                let half = BUFFER_BYTES / 2;
                let mut chunk = [0_u8; BUFFER_BYTES / 2];
                chunk.copy_from_slice(PC55.get(..half).unwrap_or(&[0; BUFFER_BYTES / 2]));
                match k.stream_buffer_send(buffer, &chunk, DONT_BLOCK) {
                    Ok(Wait::Ready(_)) | Err(_) => self.pc = 98,
                    Ok(Wait::Blocked) => {}
                }
            }
            // vPortFree( pucFullBuffer );
            98 => {
                allocate(k);
                self.pc = 99;
            }
            // xStreamBufferReset( xStreamBuffer );
            _ => {
                let _ = k.stream_buffer_reset(buffer);
                self.pc = DONE;
            }
        }
        Step::Continue
    }

    // -- the four one-line checks, which between them are most of the test --

    fn check_spaces<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        want: usize,
        next: u16,
    ) {
        let got = k.stream_buffer_spaces_available(buffer).unwrap_or(usize::MAX);
        s.expect(got == want);
        self.pc = next;
    }

    fn check_bytes<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        want: usize,
        next: u16,
    ) {
        let got = k.stream_buffer_bytes_available(buffer).unwrap_or(usize::MAX);
        s.expect(got == want);
        self.pc = next;
    }

    fn check_empty<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        want: bool,
        next: u16,
    ) {
        let got = k.stream_buffer_is_empty(buffer).unwrap_or(!want);
        s.expect(got == want);
        self.pc = next;
    }

    fn check_full<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        want: bool,
        next: u16,
    ) {
        let got = k.stream_buffer_is_full(buffer).unwrap_or(!want);
        s.expect(got == want);
        self.pc = next;
    }

    /// `xReturned = xStreamBufferSend( ... ); prvCheckExpectedState( ... );`
    ///
    /// Every argument is one the C spells out at the call site -- the
    /// length, the block time, the length it must return and the line to
    /// go to next -- so folding any two of them into a struct would
    /// hide which C argument each one is.
    #[allow(clippy::too_many_arguments)]
    fn send_expecting<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        len: usize,
        ticks: u64,
        want: usize,
        next: u16,
    ) {
        let payload = self.data(len);
        match k.stream_buffer_send(buffer, payload, ticks) {
            Ok(Wait::Ready(sent)) => {
                s.expect(sent == want);
                self.pc = next;
            }
            Ok(Wait::Blocked) => {}
            Err(_) => self.pc = next,
        }
    }

    /// `xReturned = xStreamBufferReceive( ... ); prvCheckExpectedState( ... );`
    ///
    /// `at` is the offset into the allocation the C's pointer named.
    #[allow(clippy::too_many_arguments)]
    fn receive_expecting<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
        at: usize,
        len: usize,
        ticks: u64,
        want: usize,
        next: u16,
    ) {
        let mut read = [0_u8; FULL_BUFFER];
        let end = len.min(FULL_BUFFER);
        let Some(window) = read.get_mut(..end) else {
            self.pc = next;
            return;
        };
        match k.stream_buffer_receive(buffer, window, ticks) {
            Ok(Wait::Ready(count)) => {
                let copied = read;
                self.write_at(at, copied.get(..end).unwrap_or(&[]));
                s.expect(count == want);
                self.pc = next;
            }
            Ok(Wait::Blocked) => {}
            Err(_) => self.pc = next,
        }
    }

    /// `memset( pucData, '0' + xItem, len )`.
    ///
    /// The C passes an `int` to `memset`, which truncates it to an
    /// `unsigned char` — so the hundredth pass writes 147, not a wrapped
    /// digit, and the comparison that follows must see the same.
    fn fill(&mut self, len: usize) {
        let byte = (u32::from(b'0').wrapping_add(self.item as u32)) as u8;
        for index in 0..len.min(FULL_BUFFER) {
            if let Some(slot) = self.full.get_mut(index) {
                *slot = byte;
            }
        }
    }

    /// Copy into the allocation at `at`, which is how the C's two
    /// overlapping pointers write into one block.
    fn write_at(&mut self, at: usize, source: &[u8]) {
        for (offset, byte) in source.iter().enumerate() {
            if let Some(slot) = self.full.get_mut(at.saturating_add(offset)) {
                *slot = *byte;
            }
        }
    }
}

/// The C asks to send `sbSTREAM_BUFFER_LENGTH_BYTES * 2` bytes from a
/// 54-byte string literal — it reads past the end, and gets away with it
/// because the buffer only has room for 30 and never copies the rest.
/// A port cannot read past a slice, so the tail is zeroed: it is never
/// copied and never compared.
fn oversized(source: &[u8]) -> [u8; FULL_BUFFER] {
    let mut out = [0_u8; FULL_BUFFER];
    for (slot, byte) in out.iter_mut().zip(source.iter()) {
        *slot = *byte;
    }
    out
}

/// `pvPortMalloc` / `vPortFree`.
///
/// `heap_4` brackets both with `vTaskSuspendAll()` / `xTaskResumeAll()`, and
/// `xTaskResumeAll` takes a critical section — which on the sim is where
/// time passes. A demo that allocates and does not account for it drifts
/// from the C by one exit per allocation, which is exactly the defect the
/// timer service's `Delete` arm had.
fn allocate<W: fmt::Write>(k: &mut SimKernel<W>) {
    k.suspend_all();
    let _ = k.resume_all();
}

/// `vStartStreamBufferTasks`, in the C's order.
///
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (first, second, shared, rx, tx, trigger) = {
        let mut k = runner.kernel_mut();
        let first = k.create_task("1StrEchoSer", HIGHER_PRIORITY)?;
        let second = k.create_task("2StrEchoSer", LOWER_PRIORITY)?;
        // The buffer the non-blocking pair shares, created before them as
        // the C creates it and passed to both through the task parameter.
        let shared = k.stream_buffer_create(BUFFER_BYTES, TRIGGER_LEVEL_1)?;
        let rx = k.create_task("StrNonBlkRx", LOWER_PRIORITY)?;
        let tx = k.create_task("StrNonBlkTx", LOWER_PRIORITY)?;
        let trigger = k.create_task("StrTrig", TOP_PRIORITY)?;
        (first, second, shared, rx, tx, trigger)
    };

    runner.shared_mut().state = runner::State::StreamBuffer(State::default());
    *runner.kernel_mut().tick_hook_mut() = TickIsr::StreamBuffer(Isr::default());
    runner.start_common(max_ticks)?;

    runner.attach(
        first,
        runner::Body::StreamBuffer(Body::EchoServer(EchoServer::new(0))),
    );
    runner.attach(
        second,
        runner::Body::StreamBuffer(Body::EchoServer(EchoServer::new(1))),
    );
    runner.attach(
        rx,
        runner::Body::StreamBuffer(Body::NonBlockingReceiver(NonBlockingReceiver {
            buffer: shared,
            ..NonBlockingReceiver::default()
        })),
    );
    runner.attach(
        tx,
        runner::Body::StreamBuffer(Body::NonBlockingSender(NonBlockingSender {
            buffer: shared,
            ..NonBlockingSender::default()
        })),
    );
    runner.attach(
        trigger,
        runner::Body::StreamBuffer(Body::Trigger(Trigger::default())),
    );
    Ok(())
}
