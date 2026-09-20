//! The trace sink: one line per kernel event, in the contract's format.
//!
//! The format is fixed by the umbrella's `ORACLES.md` and implemented on
//! the C side by `oracle/harness/kairos_trace.c`:
//!
//! ```text
//! <tick> <EVENT> [<subject>] [<arg>...]
//! ```
//!
//! Tasks and timers are named by their own names; queues, event groups and
//! stream buffers by a **creation ordinal per kind** (`q1`, `g1`, `s1`),
//! because a C `Queue_t` has no name and its address is not reproducible.
//!
//! The ordinal is read straight off the handle: a handle's index is its
//! creation order, so the two sides agree without either keeping a table.
//!
//! # Identity is CREATION ORDER, and that took two goes to get right
//!
//! The ordinal used to be read straight off the handle -- a handle's index
//! is its arena slot -- and the C harness keyed its own ordinal on the
//! object's ADDRESS, counting position among the same-kind pointers it had
//! ever seen. Those are two different rules, and they agreed only where the
//! allocator happened to behave like the arena. They disagreed in BOTH
//! directions at once:
//!
//! * `EventGroupsDemo` deletes and recreates a same-sized group, `malloc`
//!   hands back the same block, and the C said `g2` every time -- which the
//!   arena's reused index matched.
//! * `AbortDelay` deletes a binary semaphore and creates a 1-item queue,
//!   `malloc` hands back a different block, and the C said `q3` where the
//!   arena's reused index said `q2`.
//!
//! A running count was tried on this side alone and recorded as a dead end,
//! because it fixed `AbortDelay` and broke `EventGroupsDemo`. That was the
//! right measurement and the wrong conclusion: **no rule on one side can
//! satisfy both**, because the disagreement is about an allocator the two
//! kernels do not share. The fix had to change the CONTRACT, on both sides.
//!
//! So both sides now use a monotonic per-kind counter: the n-th object of a
//! kind ever created is `<kind>n`, and an ordinal is never reused. That is
//! strictly better evidence, not a workaround -- identity now depends only
//! on CREATION ORDER, which is a thing the differential already proves
//! identical line by line, instead of on a heap layout that was never
//! checking anything about the kernel under test.
//!
//! It costs this sink a small table, because an ordinal assigned at
//! creation has to be recoverable at every later event that names the
//! object.
//!
//! Nothing here allocates: the sink writes through a [`fmt::Write`], which
//! on the host is a stderr adapter and on a chip is a UART.

use core::fmt;

use rusty_rtos_core::trace::{Event, Trace};

/// The three kinds of object the contract names by ordinal: queues (`q`),
/// event groups (`g`) and stream buffers (`s`).
const KINDS: usize = 3;
const KIND_QUEUE: usize = 0;
const KIND_GROUP: usize = 1;
const KIND_BUFFER: usize = 2;

/// How many arena slots of one kind this sink can name.
///
/// The corpus needs far fewer (`runner` declares 12 queues, 4 groups, 8
/// buffers). It is sized for a firmware cell with a bigger geometry, so
/// that an object past the end prints `<kind>0` -- which no real object
/// gets -- rather than colliding with a live one. That is the same
/// out-of-table behaviour the C harness has.
const NAMED_SLOTS: usize = 64;

/// A [`Trace`] that writes the contract's lines to `W`.
#[derive(Debug)]
pub struct LineTrace<W: fmt::Write> {
    out: W,
    lines: u64,
    failed: bool,
    /// Print the exit-count column the C harness prints under
    /// `KAIROS_TRACE_EXITS`. Off by default: the contract's line format has
    /// no such column, and a trace with one is not comparable.
    debug_exits: bool,
    exits: u64,
    /// Slot -> ordinal, per kind. See the module documentation: this is
    /// what makes an ordinal a function of creation ORDER rather than of
    /// which arena slot the object happens to sit in.
    ordinals: [[u32; NAMED_SLOTS]; KINDS],
    /// How many objects of each kind have ever been created.
    created: [u32; KINDS],
}

/// "00" through "99", so two decimal digits are a slice of a `str`
/// rather than bytes that have to be validated back into one.
///
/// Sized for every value a `u8` group could hold rather than every value
/// one does. A group is `v % 100`, so 100 through 255 are unreachable --
/// but the compiler only knows the type, and with a 200-byte table it had
/// to bounds-check `g..g + 2` on every group of every number printed. At
/// 512 bytes the offset of any `u8` is provably inside, and the check
/// folds away. The tail costs nothing but the rodata it sits in.
const PAIRS: &str = "00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

impl<W: fmt::Write> LineTrace<W> {
    /// A sink writing to `out`.
    pub const fn new(out: W) -> Self {
        Self {
            out,
            lines: 0,
            failed: false,
            debug_exits: false,
            exits: 0,
            ordinals: [[0; NAMED_SLOTS]; KINDS],
            created: [0; KINDS],
        }
    }

    /// Give a newly created object the next ordinal of its kind, and
    /// remember it against the arena slot so every later event naming that
    /// object can find it again.
    ///
    /// A freed slot is reused by the arena, and the rebind here is what
    /// stops the successor inheriting its predecessor's name.
    fn bind(&mut self, kind: usize, index: u16) -> u32 {
        let n = match self.created.get_mut(kind) {
            Some(count) => {
                *count = count.wrapping_add(1);
                *count
            }
            None => return 0,
        };
        if let Some(slot) = self
            .ordinals
            .get_mut(kind)
            .and_then(|k| k.get_mut(index as usize))
        {
            *slot = n;
        }
        n
    }

    /// The ordinal an object was given at creation.
    fn ordinal(&self, kind: usize, index: u16) -> u32 {
        self.ordinals
            .get(kind)
            .and_then(|k| k.get(index as usize))
            .copied()
            .unwrap_or(0)
    }

    /// Add the ` #<exits>` column the C harness adds under
    /// `KAIROS_TRACE_EXITS`, for a side-by-side conformance session.
    pub const fn with_exit_column(mut self, on: bool) -> Self {
        self.debug_exits = on;
        self
    }

    /// How many lines have been written — the `lines=` counter of the
    /// scenario's verdict.
    #[must_use]
    pub const fn lines(&self) -> u64 {
        self.lines
    }

    /// Whether any write failed. A trace with a dropped line is not a
    /// trace, so the runner reports this rather than diffing a lie.
    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }

    /// The writer, for the verdict line the runner appends.
    pub const fn writer_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Take the writer back, to flush it once when the run is over.
    pub fn into_writer(self) -> W {
        self.out
    }

    /// Write a `u64` in decimal, without `Display`.
    ///
    /// `Display for u64` goes through `pad_integral`, which carries sign,
    /// width, fill and alignment that no trace line asks for.
    ///
    /// Two digits come out per step, sliced from [`PAIRS`]. A slice of a
    /// `str` is already a `str`, so nothing is validated -- the first version
    /// built a byte buffer and then asked `from_utf8` to check digits it had
    /// just written, which the profile put at 20,122,806 instructions.
    fn num(&mut self, value: u64) {
        // At most ten two-digit groups in a `u64`.
        let mut groups = [0u8; 10];
        let mut at = groups.len();
        let mut v = value;

        while v >= 100 {
            at = at.saturating_sub(1);
            if let Some(slot) = groups.get_mut(at) {
                *slot = u8::try_from(v % 100).unwrap_or(0);
            }
            v /= 100;
        }

        // The leading group is one character below ten, which is what keeps a
        // number from picking up a leading zero.
        let lead = usize::try_from(v).unwrap_or(0).saturating_mul(2);
        if v < 10 {
            self.raw(
                PAIRS
                    .get(lead.saturating_add(1)..lead.saturating_add(2))
                    .unwrap_or("0"),
            );
        } else {
            self.raw(PAIRS.get(lead..lead.saturating_add(2)).unwrap_or("00"));
        }

        for group in groups.get(at..).unwrap_or(&[]) {
            let g = usize::from(*group).saturating_mul(2);
            self.raw(PAIRS.get(g..g.saturating_add(2)).unwrap_or("00"));
        }
    }

    /// Write an `i64` in decimal, sign included.
    ///
    /// `command` is signed, and `Display` would print a minus for a negative.
    /// `unsigned_abs` is used rather than negating, so i64::MIN is not a
    /// special case.
    fn inum(&mut self, value: i64) {
        if value < 0 {
            self.raw("-");
        }
        self.num(value.unsigned_abs());
    }

    /// Write a string through, recording a writer failure the way the rest of
    /// this printer does.
    fn raw(&mut self, text: &str) {
        if self.out.write_str(text).is_err() {
            self.failed = true;
        }
    }

    /// `<tick> <NAME>`, which every line starts with.
    fn head(&mut self, tick: u64, name: &str) {
        self.num(tick);
        self.raw(" ");
        self.raw(name);
    }

    /// Count the line, the way `write_line` does.
    fn done(&mut self) {
        self.lines = self.lines.wrapping_add(1);
    }

    /// `<tick> <NAME> <arg>` without the formatting machinery.
    fn line_tick_name_str(&mut self, tick: u64, name: &str, arg: &str) {
        self.num(tick);
        self.raw(" ");
        self.raw(name);
        self.raw(" ");
        self.raw(arg);
        self.lines = self.lines.wrapping_add(1);
    }

    /// Every printer above ends its line with this, so the debug column
    /// goes on exactly once and the newline is never forgotten.
    fn end_line(&mut self) {
        if self.debug_exits {
            self.end_line_with_exits();
            return;
        }
        if self.out.write_str("\n").is_err() {
            self.failed = true;
        }
    }

    /// The ` #<exits>` column, for a side-by-side conformance session.
    ///
    /// Out of line because `format_args!` builds an `Arguments` on the
    /// stack, and inlining that here put it in the frame of every line
    /// the trace writes -- for a column that is off unless somebody has
    /// asked for it.
    #[cold]
    #[inline(never)]
    fn end_line_with_exits(&mut self) {
        let exits = self.exits;
        if self.out.write_fmt(format_args!(" #{exits}\n")).is_err() {
            self.failed = true;
        }
    }
}

impl<W: fmt::Write> Trace for LineTrace<W> {
    fn note_exits(&mut self, exits: u64) {
        self.exits = exits;
    }

    fn event(&mut self, tick: u64, event: Event<'_>) {
        let name = event.name();
        match event {
            // `<tick> <EVENT>`
            Event::StartingScheduler
            | Event::TaskCreateFailed
            | Event::LowPowerIdleBegin
            | Event::LowPowerIdleEnd => {
                self.head(tick, name);
                self.done();
                self.end_line();
            }
            // `<tick> <EVENT> <arg>`
            Event::TaskIncrementTick { tick: value } => {
                self.head(tick, name);
                self.raw(" ");
                self.num(value);
                self.done();
                self.end_line();
            }
            // `<tick> <EVENT> <task>`
            Event::TaskDelete { name: task, .. }
            | Event::TaskSwitchedIn { name: task, .. }
            | Event::TaskSwitchedOut { name: task, .. }
            | Event::TaskSuspend { name: task, .. }
            | Event::TaskResume { name: task, .. }
            | Event::TaskResumeFromIsr { name: task, .. }
            | Event::MovedTaskToReadyState { name: task, .. }
            | Event::MovedTaskToDelayedList { name: task, .. }
            | Event::MovedTaskToOverflowDelayedList { name: task, .. }
            | Event::TimerCreate { name: task, .. }
            | Event::TimerExpired { name: task, .. } => {
                self.line_tick_name_str(tick, name, task);
                self.end_line();
            }
            // `<tick> <EVENT> <task> <arg>`
            Event::TaskCreate {
                name: task,
                priority,
                ..
            }
            | Event::TaskPrioritySet {
                name: task,
                priority,
                ..
            }
            | Event::TaskPriorityInherit {
                name: task,
                priority,
                ..
            }
            | Event::TaskPriorityDisinherit {
                name: task,
                priority,
                ..
            } => {
                let value = priority.get();
                self.head(tick, name);
                self.raw(" ");
                self.raw(task);
                self.raw(" ");
                self.num(u64::from(value));
                self.done();
                self.end_line();
            }
            Event::TaskDelay {
                name: task, ticks, ..
            } => {
                self.head(tick, name);
                self.raw(" ");
                self.raw(task);
                self.raw(" ");
                self.num(ticks);
                self.done();
                self.end_line();
            }
            Event::TaskDelayUntil {
                name: task,
                wake_at,
                ..
            } => {
                self.head(tick, name);
                self.raw(" ");
                self.raw(task);
                self.raw(" ");
                self.num(wake_at);
                self.done();
                self.end_line();
            }
            Event::TaskNotify {
                name: task, index, ..
            }
            | Event::TaskNotifyWait {
                name: task, index, ..
            }
            | Event::TaskNotifyWaitBlock {
                name: task, index, ..
            }
            | Event::TaskNotifyTake {
                name: task, index, ..
            }
            | Event::TaskNotifyTakeBlock {
                name: task, index, ..
            } => {
                self.head(tick, name);
                self.raw(" ");
                self.raw(task);
                self.raw(" ");
                self.num(u64::try_from(index).unwrap_or(0));
                self.done();
                self.end_line();
            }
            // `<tick> <EVENT> q<n> <length>`
            Event::QueueCreate { queue, length, .. } => {
                let n = self.bind(KIND_QUEUE, queue.index());
                self.head(tick, name);
                self.raw(" q");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::try_from(length).unwrap_or(0));
                self.done();
                self.end_line();
            }
            // `<tick> <EVENT> q<n>`
            Event::QueueSend { queue, .. }
            | Event::QueueSendFailed { queue, .. }
            | Event::QueueSendFromIsr { queue, .. }
            | Event::QueueReceive { queue, .. }
            | Event::QueueReceiveFailed { queue, .. }
            | Event::QueueReceiveFromIsr { queue, .. }
            | Event::QueuePeek { queue, .. }
            | Event::BlockingOnQueueSend { queue, .. }
            | Event::BlockingOnQueueReceive { queue, .. }
            | Event::BlockingOnQueuePeek { queue, .. } => {
                let n = self.ordinal(KIND_QUEUE, queue.index());
                self.num(tick);
                self.raw(" ");
                self.raw(name);
                self.raw(" q");
                self.num(u64::from(n));
                self.lines = self.lines.wrapping_add(1);
                self.end_line();
            }
            // `<tick> <EVENT> g<n> [<arg>...]`
            Event::EventGroupCreate { group } => {
                let n = self.bind(KIND_GROUP, group.index());
                self.head(tick, name);
                self.raw(" g");
                self.num(u64::from(n));
                self.done();
                self.end_line();
            }
            Event::EventGroupSetBits { group, bits } => {
                let n = self.ordinal(KIND_GROUP, group.index());
                self.head(tick, name);
                self.raw(" g");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::from(bits));
                self.done();
                self.end_line();
            }
            Event::EventGroupWaitBitsBlock { group, bits } => {
                let n = self.ordinal(KIND_GROUP, group.index());
                self.head(tick, name);
                self.raw(" g");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::from(bits));
                self.done();
                self.end_line();
            }
            Event::EventGroupWaitBitsEnd {
                group,
                bits,
                timed_out,
            } => {
                let n = self.ordinal(KIND_GROUP, group.index());
                let t = u8::from(timed_out);
                self.head(tick, name);
                self.raw(" g");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::from(bits));
                self.raw(" ");
                self.num(u64::from(t));
                self.done();
                self.end_line();
            }
            // `<tick> <EVENT> s<n> <arg>`
            Event::StreamBufferCreate {
                buffer,
                is_message_buffer,
            } => {
                let n = self.bind(KIND_BUFFER, buffer.index());
                let m = u8::from(is_message_buffer);
                self.head(tick, name);
                self.raw(" s");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::from(m));
                self.done();
                self.end_line();
            }
            Event::StreamBufferSend { buffer, bytes }
            | Event::StreamBufferReceive { buffer, bytes } => {
                let n = self.ordinal(KIND_BUFFER, buffer.index());
                self.head(tick, name);
                self.raw(" s");
                self.num(u64::from(n));
                self.raw(" ");
                self.num(u64::try_from(bytes).unwrap_or(0));
                self.done();
                self.end_line();
            }
            Event::TimerCommandSend {
                name: task,
                command,
                value,
                ..
            } => {
                self.head(tick, name);
                self.raw(" ");
                self.raw(task);
                self.raw(" ");
                self.inum(i64::from(command));
                self.raw(" ");
                self.num(value);
                self.done();
                self.end_line();
            }
            // The event set is `#[non_exhaustive]`: a variant this sink has
            // not learned prints its name alone rather than vanishing, so a
            // diff shows a wrong line instead of a missing one.
            _ => {
                self.head(tick, name);
                self.raw("\n");
                self.done();
            }
        }
    }
}
