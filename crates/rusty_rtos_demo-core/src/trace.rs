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
//! **That equivalence is not exact, and `AbortDelay` is where it shows.**
//! The C harness keys its ordinal on the object's ADDRESS
//! (`prvOrdinalPerKind` searches a table of pointers): a freed object
//! leaves its entry behind, so a new object gets a NEW ordinal unless the
//! allocator hands back the same block. Our arena reuses a freed INDEX.
//! The two agree whenever the recreated object is the same size — which is
//! every scenario in the corpus that deletes, `EventGroupsDemo`'s
//! same-size groups included — and part when it is not: `AbortDelay`
//! deletes a binary semaphore and creates a 1-item queue, `malloc` returns
//! a different block, and the C says `q3` where our index says `q2`.
//!
//! A running count was tried and reverted. It makes `AbortDelay` identical
//! for all 2,549 lines and breaks `EventGroupsDemo`, whose C ordinals reuse
//! precisely because its addresses do. Neither rule is right, because the
//! subject identity in the contract is an allocator address. The index rule
//! is kept because it is the one that matches seventeen scenarios; the
//! eighteenth is an owner decision recorded in the ledger.
//!
//! Nothing here allocates: the sink writes through a [`fmt::Write`], which
//! on the host is a stderr adapter and on a chip is a UART.

use core::fmt;

use rusty_rtos_core::trace::{Event, Trace};

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
}

impl<W: fmt::Write> LineTrace<W> {
    /// A sink writing to `out`.
    pub const fn new(out: W) -> Self {
        Self {
            out,
            lines: 0,
            failed: false,
            debug_exits: false,
            exits: 0,
        }
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

    fn write_line(&mut self, args: fmt::Arguments<'_>) {
        if self.out.write_fmt(args).is_err() {
            self.failed = true;
            return;
        }
        self.lines = self.lines.wrapping_add(1);
    }

    /// Every printer above ends its line with this, so the debug column
    /// goes on exactly once and the newline is never forgotten.
    fn end_line(&mut self) {
        let result = if self.debug_exits {
            let exits = self.exits;
            self.out.write_fmt(format_args!(" #{exits}\n"))
        } else {
            self.out.write_str("\n")
        };
        if result.is_err() {
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
                self.write_line(format_args!("{tick} {name}"));
                self.end_line();
            }
            // `<tick> <EVENT> <arg>`
            Event::TaskIncrementTick { tick: value } => {
                self.write_line(format_args!("{tick} {name} {value}"));
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
                self.write_line(format_args!("{tick} {name} {task}"));
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
                self.write_line(format_args!("{tick} {name} {task} {value}"));
                self.end_line();
            }
            Event::TaskDelay {
                name: task, ticks, ..
            } => {
                self.write_line(format_args!("{tick} {name} {task} {ticks}"));
                self.end_line();
            }
            Event::TaskDelayUntil {
                name: task,
                wake_at,
                ..
            } => {
                self.write_line(format_args!("{tick} {name} {task} {wake_at}"));
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
                self.write_line(format_args!("{tick} {name} {task} {index}"));
                self.end_line();
            }
            // `<tick> <EVENT> q<n> <length>`
            Event::QueueCreate { queue, length, .. } => {
                let n = ordinal(queue.index());
                self.write_line(format_args!("{tick} {name} q{n} {length}"));
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
                let n = ordinal(queue.index());
                self.write_line(format_args!("{tick} {name} q{n}"));
                self.end_line();
            }
            // `<tick> <EVENT> g<n> [<arg>...]`
            Event::EventGroupCreate { group } => {
                let n = ordinal(group.index());
                self.write_line(format_args!("{tick} {name} g{n}"));
                self.end_line();
            }
            Event::EventGroupSetBits { group, bits } => {
                let n = ordinal(group.index());
                self.write_line(format_args!("{tick} {name} g{n} {bits}"));
                self.end_line();
            }
            Event::EventGroupWaitBitsBlock { group, bits } => {
                let n = ordinal(group.index());
                self.write_line(format_args!("{tick} {name} g{n} {bits}"));
                self.end_line();
            }
            Event::EventGroupWaitBitsEnd {
                group,
                bits,
                timed_out,
            } => {
                let n = ordinal(group.index());
                let t = u8::from(timed_out);
                self.write_line(format_args!("{tick} {name} g{n} {bits} {t}"));
                self.end_line();
            }
            // `<tick> <EVENT> s<n> <arg>`
            Event::StreamBufferCreate {
                buffer,
                is_message_buffer,
            } => {
                let n = ordinal(buffer.index());
                let m = u8::from(is_message_buffer);
                self.write_line(format_args!("{tick} {name} s{n} {m}"));
                self.end_line();
            }
            Event::StreamBufferSend { buffer, bytes }
            | Event::StreamBufferReceive { buffer, bytes } => {
                let n = ordinal(buffer.index());
                self.write_line(format_args!("{tick} {name} s{n} {bytes}"));
                self.end_line();
            }
            Event::TimerCommandSend {
                name: task,
                command,
                value,
                ..
            } => {
                self.write_line(format_args!("{tick} {name} {task} {command} {value}"));
                self.end_line();
            }
            // The event set is `#[non_exhaustive]`: a variant this sink has
            // not learned prints its name alone rather than vanishing, so a
            // diff shows a wrong line instead of a missing one.
            _ => self.write_line(format_args!("{tick} {name}\n")),
        }
    }
}

/// A handle's creation ordinal, 1-based, as the C harness numbers objects.
const fn ordinal(index: u16) -> u32 {
    (index as u32).wrapping_add(1)
}
