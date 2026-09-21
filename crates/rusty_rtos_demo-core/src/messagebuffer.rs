//! `MessageBufferDemo` — the message buffer, which is a stream buffer that
//! keeps its lengths.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/MessageBufferDemo.c`. A
//! message buffer stores a length in front of every payload and a read
//! returns one whole message or nothing, so almost every number in this file
//! is `payload + LENGTH_BYTES` rather than `payload` — and that width is a
//! [`Config`](rusty_rtos_core::config::Config) const, not a guess.
//!
//! Three things run:
//!
//! * two **echo pairs**, a server and a client each, one with the server the
//!   higher priority and one with it the lower, so messages cross a priority
//!   boundary in both directions;
//! * [`Single`], the C's `prvSingleTaskTests`, which the higher-priority
//!   server runs once before it creates its client;
//! * a **non-blocking pair** that sends and receives the decimal spelling of
//!   an incrementing number, neither end ever blocking.
//!
//! # Why this scenario could not run before
//!
//! The non-blocking pair is the reason. Both tasks sit at the idle priority
//! and neither takes a critical section on its no-progress path, so under
//! sim contract v1 the sender spun for ever on a full buffer, took no time,
//! never yielded, and stopped the clock for everything else. The C oracle
//! hangs on it too — `timeout 20 ./oracle/build/corpus MessageBufferDemo 1`
//! exits 124 with an empty trace. Contract v2 charges a kernel call that
//! took no critical section, which is what lets both sides advance.
//!
//! # And why it needed the byte arena to give memory back
//!
//! Each echo server creates a message buffer and deletes it again on *every*
//! loop of its echo, purely to prove the C leaks nothing. Against a bump
//! allocator that is a few hundred ticks to exhaustion. `take_bytes` serves
//! first fit from what deleted buffers returned, so the same block comes
//! back each time, exactly as `pvPortMalloc` hands the C the same one.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::StreamBufferHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Spawn, Step};

/// `mbMESSAGE_BUFFER_LENGTH_BYTES`.
pub const BUFFER_BYTES: usize = 50;
/// `mbNUMBER_OF_ECHO_CLIENTS`.
pub const ECHO_CLIENTS: usize = 2;
/// `mbLOWER_PRIORITY` (`tskIDLE_PRIORITY`).
pub const LOWER_PRIORITY: u8 = 0;
/// `mbHIGHER_PRIORITY` (`tskIDLE_PRIORITY + 1`).
pub const HIGHER_PRIORITY: u8 = 1;
/// `configMAX_PRIORITIES - 1`, which `prvSingleTaskTests` raises itself to
/// around its two timed calls so the allowable margin can be tight.
pub const TOP_PRIORITY: u8 = 6;
/// `mbDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;

/// `mbBYTES_TO_STORE_MESSAGE_LENGTH`, which is
/// `sizeof( configMESSAGE_BUFFER_LENGTH_TYPE )` and so `size_t` — eight
/// bytes on the machine the oracle runs on.
///
/// The C's comments in this file are written as though it were four ("a
/// maximum of 5 6 byte items can be added"). They are stale: the code uses
/// `sizeof()` throughout, so on the oracle a six-byte message costs
/// fourteen and only **three** fit. Taking the const rather than the comment
/// is the whole reason this is not a literal.
const LENGTH_BYTES: usize =
    <rusty_rtos_core::config::PosixDemoConfig as rusty_rtos_core::config::Config>::MESSAGE_LENGTH_BYTES;

/// `mbASCII_SPACE`.
const ASCII_SPACE: u8 = 32;
/// `mbASCII_TILDA`.
const ASCII_TILDA: u8 = 126;

/// `pc55ByteString`.
const PC55: &[u8] = b"One two three four five six seven eight nine ten eleven";

/// `x6ByteLength`.
const SIX: usize = 6;
/// `x17ByteLength`.
const SEVENTEEN: usize = 17;
/// `xMax6ByteMessages`, which is three here and not the five the C's comment
/// claims. See [`LENGTH_BYTES`].
const MAX_6_BYTE_MESSAGES: usize = BUFFER_BYTES / (SIX + LENGTH_BYTES);
/// The largest message this buffer can take: the C's
/// `mbMESSAGE_BUFFER_LENGTH_BYTES - sizeof( size_t )`.
const LARGEST_MESSAGE: usize = BUFFER_BYTES.saturating_sub(LENGTH_BYTES);

/// `xBlockTime` in `prvSingleTaskTests`, `pdMS_TO_TICKS( 25 )` at 1 kHz.
const BLOCK_TIME: u64 = 25;
/// `xAllowableMargin`, `pdMS_TO_TICKS( 3 )`.
const ALLOWABLE_MARGIN: u64 = 3;
/// `xTicksToBlock` in `prvEchoServer`, `pdMS_TO_TICKS( 250 )`. The server
/// waits this long for data that cannot arrive before it creates its
/// client, so nothing in this scenario happens for the first 250 ticks.
const SERVER_FIRST_BLOCK: u64 = 250;
/// `xTicksToWait` in `prvEchoClient`, `pdMS_TO_TICKS( 50 )`.
const CLIENT_SEND_WAIT: u64 = 50;

/// `iMaxValue` in the non-blocking pair.
const MAX_VALUE: i32 = 1500;
/// `sizeof( cTxString )` and `sizeof( cRxString )`, "large enough to hold a
/// 32 number in ASCII".
const NUMBER_BYTES: usize = 12;

/// A `pc` that has run off the end of its state machine.
const DONE: u16 = u16::MAX;

/// `sprintf( pc, "%d", value )` for the one shape this file needs: a value
/// in `0..=1500`, which is every value the non-blocking pair produces.
///
/// Returns the length, as the C's `strlen` does on the next line.
fn spell(value: i32, out: &mut [u8; NUMBER_BYTES]) -> usize {
    *out = [0; NUMBER_BYTES];
    let mut digits = [0u8; NUMBER_BYTES];
    let mut count = 0usize;
    let mut rest = value.max(0);
    loop {
        let digit = u8::try_from(rest % 10).unwrap_or(0);
        if let Some(slot) = digits.get_mut(count) {
            *slot = b'0'.saturating_add(digit);
        }
        count = count.saturating_add(1);
        rest /= 10;
        if rest == 0 || count >= NUMBER_BYTES {
            break;
        }
    }
    // The digits came out least significant first.
    for index in 0..count {
        let from = count.saturating_sub(1).saturating_sub(index);
        let byte = digits.get(from).copied().unwrap_or(b'0');
        if let Some(slot) = out.get_mut(index) {
            *slot = byte;
        }
    }
    count
}

/// `EchoMessageBuffers_t`.
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

/// `MessageBufferDemo.c`'s file-scope variables.
#[derive(Debug, Clone, Copy)]
pub struct State {
    /// This file has no `xErrorStatus` of its own — it asserts instead — so
    /// this is the corpus's standing equivalent: a failure latches.
    pub error_status: bool,
    /// `ulEchoLoopCounters`.
    pub echo_loop_counters: [u32; ECHO_CLIENTS],
    /// `ulNonBlockingRxCounter`.
    pub non_blocking_rx_counter: u32,
    /// The two servers' buffer pairs. See [`EchoBuffers`].
    pub echo: [EchoBuffers; ECHO_CLIENTS],
    /// `ulLastEchoLoopCounters`, a static inside the check function.
    last_echo_loop_counters: [u32; ECHO_CLIENTS],
    /// `ulLastNonBlockingRxCounter`, likewise.
    last_non_blocking_rx_counter: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            error_status: true,
            echo_loop_counters: [0; ECHO_CLIENTS],
            non_blocking_rx_counter: 0,
            echo: [EchoBuffers::default(); ECHO_CLIENTS],
            last_echo_loop_counters: [0; ECHO_CLIENTS],
            last_non_blocking_rx_counter: 0,
        }
    }
}

impl State {
    /// A `configASSERT` on the passing path is a no-op in the harness, so
    /// there is nothing to model but the failure.
    fn expect(&mut self, ok: bool) {
        if !ok {
            self.error_status = false;
        }
    }

    /// `xAreMessageBufferTasksStillRunning`, both live clauses.
    ///
    /// The remembered count only advances when the demo *is* moving, so a
    /// stall latches rather than clears.
    pub fn still_running(&mut self) -> bool {
        for index in 0..ECHO_CLIENTS {
            let now = self.echo_loop_counters.get(index).copied().unwrap_or(0);
            let last = self
                .last_echo_loop_counters
                .get(index)
                .copied()
                .unwrap_or(0);
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

        self.error_status
    }
}

/// The scenario's tasks.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    /// `prvEchoServer`.
    EchoServer(EchoServer),
    /// `prvEchoClient`.
    EchoClient(EchoClient),
    /// `prvNonBlockingSenderTask`.
    NonBlockingSender(NonBlockingSender),
    /// `prvNonBlockingReceiverTask`.
    NonBlockingReceiver(NonBlockingReceiver),
}

impl Body {
    /// `#[inline(never)]`, as every body in this corpus is, and for the
    /// reason `streambuffer::Body::step` records: inlined, this state
    /// machine lands inside the one dispatch every scenario goes through,
    /// and scenarios that never run a line of this file pay for its size.
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let spawn = &mut s.spawn;
        let runner::State::MessageBuffer(state) = &mut s.state else {
            return Step::Finish(false);
        };
        match self {
            Self::EchoServer(b) => b.step(k, state, spawn),
            Self::EchoClient(b) => b.step(k, state),
            Self::NonBlockingSender(b) => b.step(k, state),
            Self::NonBlockingReceiver(b) => b.step(k, state),
        }
    }
}

// ------------------------------------------------- the non-blocking pair --

/// `prvNonBlockingSenderTask`.
///
/// Sends the decimal spelling of an incrementing number, never blocking. The
/// string's LENGTH changes as the number crosses a power of ten, which is
/// the point — a message buffer has to carry that length, and the receiver
/// checks it got the one that was sent.
#[derive(Debug, Clone, Copy)]
pub struct NonBlockingSender {
    /// `xMessageBuffer`, passed to the C through the task's parameter.
    pub(crate) buffer: StreamBufferHandle,
    /// `iDataToSend`.
    value: i32,
    /// `cTxString`.
    tx: [u8; NUMBER_BYTES],
    /// `xStringLength`.
    tx_len: usize,
}

impl Default for NonBlockingSender {
    fn default() -> Self {
        let mut tx = [0; NUMBER_BYTES];
        // The C spells the first value before entering its loop.
        let tx_len = spell(0, &mut tx);
        Self {
            buffer: StreamBufferHandle::NULL,
            value: 0,
            tx,
            tx_len,
        }
    }
}

impl NonBlockingSender {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let payload = self.tx.get(..self.tx_len).unwrap_or(&[]);
        // xMessageBufferSend( ..., strlen( cTxString ), mbDONT_BLOCK )
        if let Ok(Wait::Ready(sent)) = k.stream_buffer_send(self.buffer, payload, DONT_BLOCK) {
            if sent == self.tx_len {
                self.value = self.value.saturating_add(1);
                if self.value > MAX_VALUE {
                    // "reset back to 0 to ensure the string being sent does
                    // not remain at the same length for too long"
                    self.value = 0;
                }
                self.tx_len = spell(self.value, &mut self.tx);
            }
        }
        // The C has no else: a full buffer is simply retried next pass. The
        // send took no critical section, which is exactly the call contract
        // v2 exists to charge.
        let _ = s;
        Step::Continue
    }
}

/// `prvNonBlockingReceiverTask`.
///
/// Expects the same spelling the sender produced, in the same order.
#[derive(Debug, Clone, Copy)]
pub struct NonBlockingReceiver {
    /// `xMessageBuffer`.
    pub(crate) buffer: StreamBufferHandle,
    /// `iDataToSend` — the receiver keeps its own copy and walks it in step.
    value: i32,
    /// `cExpectedString`.
    expected: [u8; NUMBER_BYTES],
    /// `xStringLength`.
    expected_len: usize,
    /// `xNonBlockingReceiveError`, which latches for the run.
    error: bool,
}

impl Default for NonBlockingReceiver {
    fn default() -> Self {
        let mut expected = [0; NUMBER_BYTES];
        let expected_len = spell(0, &mut expected);
        Self {
            buffer: StreamBufferHandle::NULL,
            value: 0,
            expected,
            expected_len,
            error: false,
        }
    }
}

impl NonBlockingReceiver {
    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let mut rx = [0u8; NUMBER_BYTES];
        // xMessageBufferReceive( ..., sizeof( cRxString ), mbDONT_BLOCK )
        let Ok(Wait::Ready(len)) = k.stream_buffer_receive(self.buffer, &mut rx, DONT_BLOCK) else {
            return Step::Continue;
        };

        // "Should only ever receive no data is available, or the expected
        // length of data is available."
        if len != 0 && len != self.expected_len {
            self.error = true;
        }

        if len == self.expected_len {
            let got = rx.get(..len).unwrap_or(&[]);
            let want = self.expected.get(..len).unwrap_or(&[]);
            if got != want {
                self.error = true;
            }

            self.value = self.value.saturating_add(1);
            if self.value > MAX_VALUE {
                self.value = 0;
            }
            self.expected_len = spell(self.value, &mut self.expected);

            if !self.error {
                s.non_blocking_rx_counter = s.non_blocking_rx_counter.saturating_add(1);
            }
        }
        s.expect(!self.error);
        Step::Continue
    }
}

// ----------------------------------------------------------- echo server --

/// `prvEchoServer`.
#[derive(Debug, Clone, Copy)]
pub struct EchoServer {
    pc: u16,
    /// Which of [`State::echo`] is this server's pair.
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
    /// `xTempMessageBuffer`, created and deleted on every echo.
    temp: StreamBufferHandle,
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
            temp: StreamBufferHandle::NULL,
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
            // xMessageBuffers.xEchoClientBuffer =
            //     xMessageBufferCreate( mbMESSAGE_BUFFER_LENGTH_BYTES );
            0 => {
                if let Ok(buffer) = k.message_buffer_create(BUFFER_BYTES) {
                    if let Some(pair) = s.echo.get_mut(self.which) {
                        pair.client = buffer;
                    }
                }
                self.pc = 1;
            }
            // xMessageBuffers.xEchoServerBuffer = xMessageBufferCreate( ... );
            1 => {
                if let Ok(buffer) = k.message_buffer_create(BUFFER_BYTES) {
                    if let Some(pair) = s.echo.get_mut(self.which) {
                        pair.server = buffer;
                    }
                }
                self.pc = 2;
            }
            // pcReceivedString = pvPortMalloc( mbMESSAGE_BUFFER_LENGTH_BYTES );
            2 => {
                allocate(k);
                self.pc = 3;
            }
            // xTimeOnEntering = xTaskGetTickCount();
            3 => {
                self.time_on_entering = k.tick_count();
                self.pc = 4;
            }
            // xReceivedLength = xMessageBufferReceive( xEchoClientBuffer, ...,
            //     mbMESSAGE_BUFFER_LENGTH_BYTES, xTicksToBlock );
            //
            // "Don't expect to receive anything yet!" — the client does not
            // exist, so this is 250 ticks of nothing, deliberately.
            4 => match k.stream_buffer_receive(pair.client, &mut self.received, SERVER_FIRST_BLOCK)
            {
                Ok(Wait::Ready(count)) => {
                    let elapsed = k.tick_count().wrapping_sub(self.time_on_entering);
                    s.expect(elapsed >= SERVER_FIRST_BLOCK);
                    s.expect(count == 0);
                    self.pc = 5;
                }
                Ok(Wait::Blocked) => {}
                Err(_) => self.pc = 5,
            },
            // if( uxTaskPriorityGet( NULL ) == mbLOWER_PRIORITY )
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
            // prvSingleTaskTests( xMessageBuffers.xEchoClientBuffer );
            6 => {
                let step = self.single.step(k, s, pair.client);
                if self.single.pc == DONE {
                    self.pc = 7;
                }
                return step;
            }
            // xTaskCreate( prvEchoClient, "EchoClient", ..., &xMessageBuffers,
            //              mbHIGHER_PRIORITY or mbLOWER_PRIORITY, NULL );
            7 => {
                let priority = if self.lower {
                    HIGHER_PRIORITY
                } else {
                    LOWER_PRIORITY
                };
                if let Ok(task) = k.create_task("EchoClient", priority) {
                    *spawn = Some((
                        task,
                        Spawn::MessageBuffer(Body::EchoClient(EchoClient::new(self.which))),
                    ));
                }
                self.pc = 8;
            }
            // memset( pcReceivedString, 0x00, mbMESSAGE_BUFFER_LENGTH_BYTES );
            // xReceivedLength = xMessageBufferReceive( xEchoClientBuffer, ...,
            //     mbMESSAGE_BUFFER_LENGTH_BYTES, portMAX_DELAY );
            8 => {
                self.received = [0; BUFFER_BYTES];
                match k.stream_buffer_receive(
                    pair.client,
                    &mut self.received,
                    SimKernel::<W>::MAX_DELAY,
                ) {
                    Ok(Wait::Ready(count)) => {
                        // configASSERT( xReceivedLength > 0 );
                        s.expect(count > 0);
                        self.received_len = count;
                        self.pc = 9;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 9,
                }
            }
            // xMessageBufferSend( xEchoServerBuffer, pcReceivedString,
            //                     xReceivedLength, portMAX_DELAY );
            9 => {
                let len = self.received_len.min(self.received.len());
                let echoed = self.received;
                let payload = echoed.get(..len).unwrap_or(&[]);
                match k.stream_buffer_send(pair.server, payload, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(_)) | Err(_) => self.pc = 10,
                    Ok(Wait::Blocked) => {}
                }
            }
            // xTempMessageBuffer = xMessageBufferCreate( ... );
            //
            // "This message buffer is just created and deleted to ensure no
            // memory leaks." It is also what makes this scenario need a byte
            // arena that gives memory back: it runs on every echo.
            10 => {
                self.temp = k.message_buffer_create(BUFFER_BYTES).unwrap_or_default();
                self.pc = 11;
            }
            // vMessageBufferDelete( xTempMessageBuffer );
            _ => {
                let _ = k.stream_buffer_delete(self.temp);
                self.temp = StreamBufferHandle::NULL;
                self.pc = 8;
            }
        }
        Step::Continue
    }
}

// ----------------------------------------------------------- echo client --

/// `prvEchoClient`.
#[derive(Debug, Clone, Copy)]
pub struct EchoClient {
    pc: u16,
    /// Which of [`State::echo`] to talk to.
    which: usize,
    /// `uxIndex`, the task's own priority, used to index the loop counters.
    index: usize,
    /// `xSendLength`.
    send_length: usize,
    /// `cNextChar`.
    next_char: u8,
    /// `pcStringToSend`.
    to_send: [u8; BUFFER_BYTES],
    /// `pcStringReceived`.
    received: [u8; BUFFER_BYTES],
}

impl EchoClient {
    /// A client for one of the two echo pairs.
    #[must_use]
    pub fn new(which: usize) -> Self {
        Self {
            pc: 0,
            which,
            index: which,
            send_length: 0,
            next_char: ASCII_SPACE,
            to_send: [0; BUFFER_BYTES],
            received: [0; BUFFER_BYTES],
        }
    }

    fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) -> Step {
        let pair = s.echo.get(self.which).copied().unwrap_or_default();
        match self.pc {
            // UBaseType_t uxIndex = uxTaskPriorityGet( NULL );
            //
            // A kernel call, and so a critical section, exactly once — the C
            // reads it before its loop and never again.
            0 => {
                self.index = usize::from(k.task_priority_get(None).unwrap_or(0))
                    .min(ECHO_CLIENTS.saturating_sub(1));
                self.pc = 1;
            }
            // pcStringToSend = pvPortMalloc( ... );
            // pcStringReceived = pvPortMalloc( ... );
            1 => {
                allocate(k);
                self.pc = 2;
            }
            2 => {
                allocate(k);
                self.pc = 3;
            }
            // The head of the forever loop: grow the string, wrap at the
            // largest message the buffer can carry, and fill it.
            3 => {
                self.send_length = self.send_length.saturating_add(1);
                if self.send_length > LARGEST_MESSAGE {
                    // "Back to a string length of 1."
                    self.send_length = 1;
                    if let Some(slot) = s.echo_loop_counters.get_mut(self.index) {
                        *slot = slot.saturating_add(1);
                    }
                }
                self.to_send = [0; BUFFER_BYTES];
                for offset in 0..self.send_length.min(BUFFER_BYTES) {
                    if let Some(slot) = self.to_send.get_mut(offset) {
                        *slot = self.next_char;
                    }
                    self.next_char = self.next_char.saturating_add(1);
                    if self.next_char > ASCII_TILDA {
                        self.next_char = ASCII_SPACE;
                    }
                }
                self.pc = 4;
            }
            // do { ux = xMessageBufferSend( xEchoClientBuffer, ...,
            //      xSendLength, xTicksToWait ); } while( ux == 0 );
            4 => {
                let payload = self.to_send.get(..self.send_length).unwrap_or(&[]);
                match k.stream_buffer_send(pair.client, payload, CLIENT_SEND_WAIT) {
                    // Zero means it did not go; the C loops and tries again.
                    Ok(Wait::Ready(0)) => {}
                    Ok(Wait::Ready(_)) => {
                        self.received = [0; BUFFER_BYTES];
                        self.pc = 5;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 5,
                }
            }
            // xMessageBufferReceive( xEchoServerBuffer, pcStringReceived,
            //                        xSendLength, portMAX_DELAY );
            _ => {
                let mut into = self.received;
                let window = self.send_length.min(BUFFER_BYTES);
                let Some(slot) = into.get_mut(..window) else {
                    self.pc = 3;
                    return Step::Continue;
                };
                match k.stream_buffer_receive(pair.server, slot, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(count)) => {
                        self.received = into;
                        // configASSERT( strcmp( pcStringToSend,
                        //                       pcStringReceived ) == 0 );
                        let sent = self.to_send.get(..self.send_length).unwrap_or(&[]);
                        let back = self.received.get(..count).unwrap_or(&[]);
                        s.expect(sent == back);
                        self.pc = 3;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 3,
                }
            }
        }
        Step::Continue
    }
}

// ------------------------------------------------------ single-task tests --

/// `prvSingleTaskTests` — the C's straight-line torture of one buffer.
///
/// Every arm below is one kernel call, in the C's order, because on the sim
/// the ORDER and COUNT of critical-section exits is the clock. The three
/// loops carry their index in [`Single::item`] and jump back to their head
/// rather than being unrolled.
#[derive(Debug, Clone, Copy)]
pub struct Single {
    /// Public to its module only so [`EchoServer`] can see when it is done.
    pc: u16,
    /// The index of whichever of the three loops is running.
    item: usize,
    /// `xExpectedSpace`.
    expected_space: usize,
    /// `uxOriginalPriority`.
    original_priority: u8,
    /// `xTimeBeforeCall`.
    time_before: u64,
    /// `pucData`, the pattern written out.
    data: [u8; BUFFER_BYTES],
    /// `pucReadData`, what came back.
    read_data: [u8; BUFFER_BYTES],
}

impl Default for Single {
    fn default() -> Self {
        Self {
            pc: 0,
            item: 0,
            expected_space: 0,
            original_priority: HIGHER_PRIORITY,
            time_before: 0,
            data: [0; BUFFER_BYTES],
            read_data: [0; BUFFER_BYTES],
        }
    }
}

impl Single {
    /// `memset( pucData, '0' + xItem, length )`.
    ///
    /// The C adds two ints and lets `memset` truncate, so past `xItem == 207`
    /// the byte wraps. The hundred-iteration loop only reaches 99, but the
    /// wrapping add is what the C does and costs nothing to match.
    fn fill(&mut self, length: usize) {
        let byte = b'0'.wrapping_add(u8::try_from(self.item % 256).unwrap_or(0));
        self.data = [0; BUFFER_BYTES];
        for offset in 0..length.min(BUFFER_BYTES) {
            if let Some(slot) = self.data.get_mut(offset) {
                *slot = byte;
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one arm per kernel call, in the C's order"
    )]
    fn step<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        s: &mut State,
        buffer: StreamBufferHandle,
    ) -> Step {
        match self.pc {
            // pucFullBuffer = pvPortMalloc( mbMESSAGE_BUFFER_LENGTH_BYTES );
            0 => {
                allocate(k);
                self.pc = 1;
            }
            // xExpectedSpace = xMessageBufferSpaceAvailable( xMessageBuffer );
            // configASSERT( xExpectedSpace == mbMESSAGE_BUFFER_LENGTH_BYTES );
            1 => {
                self.expected_space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(self.expected_space == BUFFER_BYTES);
                self.pc = 2;
            }
            // configASSERT( xMessageBufferIsEmpty( xMessageBuffer ) == pdTRUE );
            2 => {
                s.expect(k.stream_buffer_is_empty(buffer).unwrap_or(false));
                self.pc = 3;
            }
            // xNextLength = xMessageBufferNextLengthBytes( xMessageBuffer );
            // configASSERT( xNextLength == 0 );
            3 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(1) == 0);
                self.pc = 4;
            }
            // "Try sending more bytes than possible, first using the FromISR
            // version, then with an infinite block time to ensure this task
            // does not lock up."
            4 => {
                let payload = self.data;
                let over = payload.get(..BUFFER_BYTES).unwrap_or(&[]);
                // The FromISR calls answer with the woken flag beside the
                // count; nothing here acts on it, as the C passes NULL.
                let (sent, _woken) = k
                    .stream_buffer_send_from_isr(buffer, over)
                    .unwrap_or_default();
                s.expect(sent == 0);
                self.pc = 5;
            }
            // The same length again with portMAX_DELAY. It can never fit, so
            // the kernel must NOT wait for it to — which is the case this
            // arm exists to prove.
            5 => {
                let payload = self.data;
                let over = payload.get(..BUFFER_BYTES).unwrap_or(&[]);
                match k.stream_buffer_send(buffer, over, SimKernel::<W>::MAX_DELAY) {
                    Ok(Wait::Ready(sent)) => {
                        s.expect(sent == 0);
                        self.item = 0;
                        self.pc = 6;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => {
                        self.item = 0;
                        self.pc = 6;
                    }
                }
            }
            // for( xItem = 0; xItem < xMax6ByteMessages; xItem++ )
            6 => {
                if self.item < MAX_6_BYTE_MESSAGES {
                    self.pc = 7;
                } else {
                    self.pc = 12;
                }
            }
            // configASSERT( xMessageBufferIsFull( xMessageBuffer ) == pdFALSE );
            7 => {
                s.expect(!k.stream_buffer_is_full(buffer).unwrap_or(true));
                self.pc = 8;
            }
            // taskENTER_CRITICAL();
            // xReturned = xMessageBufferSendFromISR( ..., x6ByteLength, NULL );
            // taskEXIT_CRITICAL();
            8 => {
                self.fill(SIX);
                let payload = self.data;
                let six = payload.get(..SIX).unwrap_or(&[]);
                k.enter_critical();
                let (sent, _woken) = k
                    .stream_buffer_send_from_isr(buffer, six)
                    .unwrap_or_default();
                k.exit_critical();
                s.expect(sent == SIX);
                self.expected_space = self
                    .expected_space
                    .saturating_sub(SIX.saturating_add(LENGTH_BYTES));
                self.pc = 9;
            }
            // xReturned = xMessageBufferSpaceAvailable( xMessageBuffer );
            // configASSERT( xReturned == xExpectedSpace );
            9 => {
                let space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(space == self.expected_space);
                self.pc = 10;
            }
            // xNextLength = xMessageBufferNextLengthBytes( xMessageBuffer );
            // configASSERT( xNextLength == x6ByteLength );
            10 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(0) == SIX);
                self.pc = 11;
            }
            11 => {
                self.item = self.item.saturating_add(1);
                self.pc = 6;
            }
            // configASSERT( xMessageBufferIsFull( xMessageBuffer ) == pdTRUE );
            12 => {
                s.expect(k.stream_buffer_is_full(buffer).unwrap_or(false));
                self.pc = 13;
            }
            // xReturned = xMessageBufferSend( ..., sizeof( pucData[ 0 ] ),
            //                                 mbDONT_BLOCK );
            13 => {
                let one = [0u8; 1];
                let sent = match k.stream_buffer_send(buffer, &one, DONT_BLOCK) {
                    Ok(Wait::Ready(sent)) => sent,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(sent == 0);
                self.pc = 14;
            }
            // uxOriginalPriority = uxTaskPriorityGet( NULL );
            14 => {
                self.original_priority = k.task_priority_get(None).unwrap_or(HIGHER_PRIORITY);
                self.pc = 15;
            }
            // vTaskPrioritySet( NULL, configMAX_PRIORITIES - 1 );
            15 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 16;
            }
            // xTimeBeforeCall = xTaskGetTickCount();
            16 => {
                self.time_before = k.tick_count();
                self.pc = 17;
            }
            // xReturned = xMessageBufferSend( ..., 1, xBlockTime );
            //
            // The buffer is full, so this blocks for the whole block time
            // and comes back with nothing.
            17 => {
                let one = [0u8; 1];
                match k.stream_buffer_send(buffer, &one, BLOCK_TIME) {
                    Ok(Wait::Ready(sent)) => {
                        s.expect(sent == 0);
                        self.pc = 18;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 18,
                }
            }
            // xTimeAfterCall = xTaskGetTickCount();
            // vTaskPrioritySet( NULL, uxOriginalPriority );
            18 => {
                let elapsed = k.tick_count().wrapping_sub(self.time_before);
                s.expect(elapsed >= BLOCK_TIME);
                s.expect(elapsed < BLOCK_TIME.saturating_add(ALLOWABLE_MARGIN));
                self.pc = 19;
            }
            19 => {
                let _ = k.set_priority(None, self.original_priority);
                self.item = 0;
                self.pc = 20;
            }
            // The read-back loop over the same xMax6ByteMessages messages.
            20 => {
                if self.item < MAX_6_BYTE_MESSAGES {
                    self.pc = 21;
                } else {
                    self.pc = 26;
                }
            }
            // "Try reading the message into a buffer that is too small. The
            // message should remain in the buffer."
            21 => {
                self.fill(SIX);
                let mut small = [0u8; SIX.saturating_sub(1)];
                let got = match k.stream_buffer_receive(buffer, &mut small, DONT_BLOCK) {
                    Ok(Wait::Ready(got)) => got,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(got == 0);
                self.pc = 22;
            }
            // "Should still be at least one 6 byte message still available."
            22 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(0) == SIX);
                self.pc = 23;
            }
            // taskENTER_CRITICAL();
            // xReturned = xMessageBufferReceiveFromISR( ..., x6ByteLength, NULL );
            // taskEXIT_CRITICAL();
            23 => {
                let mut into = [0u8; SIX];
                k.enter_critical();
                let (got, _woken) = k
                    .stream_buffer_receive_from_isr(buffer, &mut into)
                    .unwrap_or_default();
                k.exit_critical();
                s.expect(got == SIX);
                // configASSERT( memcmp( pucData, pucReadData, x6ByteLength ) == 0 );
                let want = self.data.get(..SIX).unwrap_or(&[]);
                s.expect(into.get(..got.min(SIX)).unwrap_or(&[]) == want);
                if let Some(slot) = self.read_data.get_mut(..SIX) {
                    slot.copy_from_slice(&into);
                }
                self.expected_space = self
                    .expected_space
                    .saturating_add(SIX.saturating_add(LENGTH_BYTES));
                self.pc = 24;
            }
            // xReturned = xMessageBufferSpaceAvailable( xMessageBuffer );
            24 => {
                let space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(space == self.expected_space);
                self.pc = 25;
            }
            25 => {
                self.item = self.item.saturating_add(1);
                self.pc = 20;
            }
            // "The buffer should be empty again."
            26 => {
                s.expect(k.stream_buffer_is_empty(buffer).unwrap_or(false));
                self.pc = 27;
            }
            27 => {
                self.expected_space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(self.expected_space == BUFFER_BYTES);
                self.pc = 28;
            }
            28 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(1) == 0);
                self.pc = 29;
            }
            // The timed RECEIVE, which mirrors the timed send above.
            29 => {
                let _ = k.set_priority(None, TOP_PRIORITY);
                self.pc = 30;
            }
            30 => {
                self.time_before = k.tick_count();
                self.pc = 31;
            }
            31 => {
                let mut into = [0u8; SIX];
                match k.stream_buffer_receive(buffer, &mut into, BLOCK_TIME) {
                    Ok(Wait::Ready(got)) => {
                        s.expect(got == 0);
                        self.pc = 32;
                    }
                    Ok(Wait::Blocked) => {}
                    Err(_) => self.pc = 32,
                }
            }
            32 => {
                let elapsed = k.tick_count().wrapping_sub(self.time_before);
                s.expect(elapsed >= BLOCK_TIME);
                s.expect(elapsed < BLOCK_TIME.saturating_add(ALLOWABLE_MARGIN));
                self.pc = 33;
            }
            33 => {
                let _ = k.set_priority(None, self.original_priority);
                // xExpectedSpace = mbMESSAGE_BUFFER_LENGTH_BYTES -
                //     ( x17ByteLength + mbBYTES_TO_STORE_MESSAGE_LENGTH );
                self.expected_space =
                    BUFFER_BYTES.saturating_sub(SEVENTEEN.saturating_add(LENGTH_BYTES));
                self.item = 0;
                self.pc = 34;
            }
            // "Reading and writing 17 bytes at a time will result in 21 bytes
            // being written into the buffer, and as 50 is not divisible by
            // 21, writing multiple times will cause the data to wrap."
            34 => {
                if self.item < 100 {
                    self.pc = 35;
                } else {
                    self.pc = 40;
                }
            }
            35 => {
                self.fill(SEVENTEEN);
                let payload = self.data;
                let seventeen = payload.get(..SEVENTEEN).unwrap_or(&[]);
                let sent = match k.stream_buffer_send(buffer, seventeen, DONT_BLOCK) {
                    Ok(Wait::Ready(sent)) => sent,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(sent == SEVENTEEN);
                self.pc = 36;
            }
            36 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(0) == SEVENTEEN);
                self.pc = 37;
            }
            37 => {
                let space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(space == self.expected_space);
                self.pc = 38;
            }
            38 => {
                let mut into = [0u8; SEVENTEEN];
                let got = match k.stream_buffer_receive(buffer, &mut into, DONT_BLOCK) {
                    Ok(Wait::Ready(got)) => got,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(got == SEVENTEEN);
                let want = self.data.get(..SEVENTEEN).unwrap_or(&[]);
                s.expect(into.get(..got.min(SEVENTEEN)).unwrap_or(&[]) == want);
                self.pc = 39;
            }
            39 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(1) == 0);
                self.item = self.item.saturating_add(1);
                self.pc = 34;
            }
            40 => {
                s.expect(k.stream_buffer_is_empty(buffer).unwrap_or(false));
                self.pc = 41;
            }
            41 => {
                let space = k.stream_buffer_spaces_available(buffer).unwrap_or(0);
                s.expect(space == BUFFER_BYTES);
                self.item = 0;
                self.pc = 42;
            }
            // "Cannot write within sizeof( size_t ) bytes of the full 50
            // bytes, as that would not leave space for the four bytes taken
            // by the data length."
            //
            // ONE send, not four. The C follows this with three more inside
            // `#ifndef configMESSAGE_BUFFER_LENGTH_TYPE`, and that block is
            // DEAD: `FreeRTOS.h` defaults the macro to `size_t`, and
            // `MessageBufferDemo.c` includes it long before the `#ifndef` is
            // read. Writing the three anyway cost three extra blind-call
            // charges and moved a tick two events early, 2,407 lines in.
            42 => {
                let sent = match k.stream_buffer_send(
                    buffer,
                    PC55.get(..BUFFER_BYTES).unwrap_or(PC55),
                    DONT_BLOCK,
                ) {
                    Ok(Wait::Ready(sent)) => sent,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(sent == 0);
                self.pc = 43;
            }
            // "Don't expect any messages to be available as the above were
            // too large to get written."
            43 => {
                s.expect(k.stream_buffer_next_message_length(buffer).unwrap_or(1) == 0);
                self.pc = 44;
            }
            // "Can write mbMESSAGE_BUFFER_LENGTH_BYTES - sizeof( size_t )
            // bytes though."
            44 => {
                let sent = match k.stream_buffer_send(
                    buffer,
                    PC55.get(..LARGEST_MESSAGE).unwrap_or(PC55),
                    DONT_BLOCK,
                ) {
                    Ok(Wait::Ready(sent)) => sent,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(sent == LARGEST_MESSAGE);
                self.pc = 45;
            }
            45 => {
                s.expect(
                    k.stream_buffer_next_message_length(buffer).unwrap_or(0) == LARGEST_MESSAGE,
                );
                self.pc = 46;
            }
            46 => {
                let mut into = [0u8; BUFFER_BYTES];
                let Some(window) = into.get_mut(..LARGEST_MESSAGE) else {
                    self.pc = 47;
                    return Step::Continue;
                };
                let got = match k.stream_buffer_receive(buffer, window, DONT_BLOCK) {
                    Ok(Wait::Ready(got)) => got,
                    Ok(Wait::Blocked) | Err(_) => 0,
                };
                s.expect(got == LARGEST_MESSAGE);
                let want = PC55.get(..LARGEST_MESSAGE).unwrap_or(PC55);
                s.expect(into.get(..got.min(LARGEST_MESSAGE)).unwrap_or(&[]) == want);
                self.pc = 47;
            }
            // vPortFree( pucFullBuffer );
            47 => {
                allocate(k);
                self.pc = 48;
            }
            // xMessageBufferReset( xMessageBuffer );
            _ => {
                let _ = k.stream_buffer_reset(buffer);
                self.pc = DONE;
            }
        }
        Step::Continue
    }
}

/// `pvPortMalloc` / `vPortFree`, for their effect on the clock rather than
/// their result.
///
/// Neither allocation's bytes are ever read by this port — the data lives in
/// the task's own struct — but both take the heap's critical section, and on
/// the sim a critical-section exit is a sixteenth of a tick. Skipping them
/// would run the scenario fast and produce a different trace.
fn allocate<W: fmt::Write>(k: &mut SimKernel<W>) {
    k.suspend_all();
    let _ = k.resume_all();
}

/// `vStartMessageBufferTasks`, in the C's order.
///
/// # Errors
///
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (first, second, shared, rx, tx) = {
        let mut k = runner.kernel_mut();
        // "The echo servers sets up the message buffers before creating the
        // echo client tasks."
        let first = k.create_task("1EchoServer", HIGHER_PRIORITY)?;
        let second = k.create_task("2EchoServer", LOWER_PRIORITY)?;
        // "The message buffer they use is created and passed in using the
        // task's parameter."
        let shared = k.message_buffer_create(BUFFER_BYTES)?;
        let rx = k.create_task("NonBlkRx", LOWER_PRIORITY)?;
        let tx = k.create_task("NonBlkTx", LOWER_PRIORITY)?;
        (first, second, shared, rx, tx)
    };

    runner.shared_mut().state = runner::State::MessageBuffer(State::default());
    runner.start_common(max_ticks)?;

    runner.attach(
        first,
        runner::Body::MessageBuffer(Body::EchoServer(EchoServer::new(0))),
    );
    runner.attach(
        second,
        runner::Body::MessageBuffer(Body::EchoServer(EchoServer::new(1))),
    );
    runner.attach(
        rx,
        runner::Body::MessageBuffer(Body::NonBlockingReceiver(NonBlockingReceiver {
            buffer: shared,
            ..NonBlockingReceiver::default()
        })),
    );
    runner.attach(
        tx,
        runner::Body::MessageBuffer(Body::NonBlockingSender(NonBlockingSender {
            buffer: shared,
            ..NonBlockingSender::default()
        })),
    );
    Ok(())
}
