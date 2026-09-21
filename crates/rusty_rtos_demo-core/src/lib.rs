#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_demo-core` — the FreeRTOS standard demo tasks, remade.
//!
//! `FreeRTOS/Demo/Common/Minimal` is the corpus every FreeRTOS port has
//! been judged against for twenty years: each file starts a handful of
//! tasks that torture one part of the kernel and then answers one question,
//! `xAre...StillRunning()`. Kairos remakes them because they are the
//! conformance suite, and runs them on the deterministic sim port so that
//! every scheduling decision the Rust kernel makes can be diffed, line for
//! line, against the C kernel making the same decisions
//! (umbrella `ORACLES.md`).
//!
//! # What a scenario is here
//!
//! A C demo task is a function with an infinite loop that blocks. This
//! crate cannot block — the kernel is `forbid(unsafe)` and has no stacks to
//! switch — so each task is a state machine whose `step` runs one C
//! statement. [`runner::Runner`] steps whichever task the kernel says is
//! current, which makes the sequence of kernel calls, and therefore the
//! trace, identical to the C's.
//!
//! # The corpus
//!
//! | scenario | C file | what it tortures |
//! |---|---|---|
//! | [`dynamic`] | `dynamic.c` | suspend, resume, priority set, suspend-all |
//! | [`abortdelay`] | `AbortDelay.c` | `xTaskAbortDelay` against every way a task can block |
//! | [`pollq`] | `PollQ.c` | a queue polled from both ends, never blocking |
//! | [`blockq`] | `BlockQ.c` | blocking sends and receives, three task pairs |
//! | [`semtest`] | `semtest.c` | two binary semaphores guarding a shared variable |
//! | [`countsem`] | `countsem.c` | counting semaphores, driven to both ends |
//! | [`recmutex`] | `recmutex.c` | a recursive mutex and its priority inheritance |
//! | [`blocktim`] | `blocktim.c` | block times and `xTaskDelayUntil`, to the tick |
//! | [`qpeek`] | `QPeek.c` | peeking, and the order four priorities wake in |
//! | [`genqtest`] | `GenQTest.c` | both queue ends, and priority inheritance |
//! | [`qoverwrite`] | `QueueOverwrite.c` | a queue of one, and the first interrupt half |
//! | [`qsetpoll`] | `QueueSetPolling.c` | a queue set polled by a task, written by an interrupt |
//! | [`intsem`] | `IntSemTest.c` | semaphores and a mutex given from an interrupt |
//! | [`sbint`] | `StreamBufferInterrupt.c` | a string streamed from the tick, byte by byte |
//! | [`streambuffer`] | `StreamBufferDemo.c` | the whole stream-buffer face: two echo pairs, the head-and-tail tests, trigger levels |
//! | [`tasknotify`] | `TaskNotify.c` | every notification method, and a notify to a suspended task |
//! | [`timerdemo`] | `TimerDemo.c` | software timers, checked against the tick they fire on |
//!
//! | [`eventgroups`] | `EventGroupsDemo.c` | event bits, a rendezvous, and the deferred-interrupt path |
//! | [`mbamp`] | `MessageBufferAMP.c` | message buffers across a replaced send-completed seam |
//! | [`death`] | `death.c` | tasks created and deleted while the scheduler runs |
//!
//! Plus [`pollq_typed`] and [`pollq_async`], which are `PollQ` rewritten
//! against the Rust face and as `async fn` bodies, and diffed against
//! `PollQ`'s own C oracle trace -- because the point is that the *same*
//! trace comes out, so the face is proved to cost nothing rather than
//! asserted to.
//!
//! Every one of them is in `kairos conform --all`, and nothing is excluded
//! from it.

pub mod abortdelay;
pub mod apisweep;
pub mod blockq;
pub mod blocktim;
pub mod countsem;
pub mod death;
pub mod dynamic;
pub mod eventgroups;
pub mod genqtest;
pub mod intqueue;
pub mod intsem;
pub mod mbamp;
pub mod messagebuffer;
pub mod pins;
pub mod pollq;
pub mod pollq_async;
pub mod pollq_typed;
pub mod qoverwrite;
pub mod qpeek;
pub mod qset;
pub mod qsetpoll;
pub mod recmutex;
pub mod runner;
pub mod sbint;
pub mod semtest;
pub mod streambuffer;
pub mod tasknotify;
pub mod timerdemo;
pub mod trace;

pub use pins::{Digest, PIN_TICKS, Pin, pins};
pub use runner::{Body, Runner, Shared, SimKernel, Step, Verdict};
pub use trace::LineTrace;

/// Crate version, for manifests and logs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The runner's safety net: the most steps a scenario may take before the
/// run is called a runaway.
///
/// The first C oracle run taught us to want one — an unbounded scenario
/// wrote an 8 GB trace before anybody noticed — so the Rust side refuses
/// to be the second lesson.
///
/// It has to scale with the run, because a step is a C statement and some
/// scenarios spend thousands of them per tick without making a kernel call
/// at all: `semtest`'s guarded loop counts to 0xfff between one semaphore
/// take and the next, which is about four thousand steps a tick. A fixed
/// limit stopped it at tick 12,429 of a 100,000-tick run and called the
/// result a failure, which is exactly the kind of lie a guard is supposed
/// to prevent.
#[must_use]
pub const fn step_limit_for(max_ticks: u64) -> u64 {
    max_ticks.saturating_mul(100_000).saturating_add(10_000_000)
}

/// The limit for the C harness's default run length.
pub const DEFAULT_STEP_LIMIT: u64 = step_limit_for(2000);

/// The scenarios this crate can run.
// Deliberately exhaustive: adding a scenario should fail to compile
// everywhere it has to be wired, starting with the binary's `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// `dynamic.c`.
    Dynamic,
    /// `PollQ.c`.
    PollQ,
    /// `BlockQ.c`.
    BlockQ,
    /// `semtest.c`.
    SemTest,
    /// `countsem.c`.
    CountSem,
    /// `recmutex.c`.
    RecMutex,
    /// `blocktim.c`.
    BlockTim,
    /// `AbortDelay.c`.
    AbortDelay,
    /// `death.c`.
    Death,
    /// `QPeek.c`.
    QPeek,
    /// `GenQTest.c`.
    GenQTest,
    /// `QueueOverwrite.c`.
    QOverwrite,
    /// `QueueSetPolling.c`.
    QSetPoll,
    /// `IntSemTest.c`.
    IntSem,
    /// `StreamBufferInterrupt.c`.
    SbInt,
    /// `StreamBufferDemo.c`.
    StreamBuffer,
    /// `TaskNotify.c`.
    TaskNotify,
    /// `TimerDemo.c`.
    TimerDemo,
    /// `EventGroupsDemo.c`.
    EventGroups,
    /// `MessageBufferAMP.c`.
    MbAmp,
    /// `MessageBufferDemo.c`.
    MessageBuffer,
    /// `IntQueue.c`.
    IntQueue,
    /// `QueueSet.c`.
    QueueSet,
    /// `ApiSweep`, which is ours rather than a port.
    ApiSweep,
    /// `PollQ.c` against the Rust face — diffed against `PollQ`'s own
    /// C oracle trace, because the face is supposed to cost nothing.
    PollQTyped,
    /// `PollQ.c` with `async fn` task bodies (K2.2), diffed against the
    /// same trace for the same reason.
    PollQAsync,
}

impl Scenario {
    /// The scenario's name, as the trace's verdict line prints it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dynamic => "dynamic",
            Self::PollQ => "PollQ",
            Self::BlockQ => "BlockQ",
            Self::SemTest => "semtest",
            Self::CountSem => "countsem",
            Self::RecMutex => "recmutex",
            Self::BlockTim => "blocktim",
            Self::AbortDelay => "AbortDelay",
            Self::Death => "death",
            Self::QPeek => "QPeek",
            Self::GenQTest => "GenQTest",
            Self::QOverwrite => "QueueOverwrite",
            Self::QSetPoll => "QueueSetPolling",
            Self::IntSem => "IntSemTest",
            Self::SbInt => "StreamBufferInterrupt",
            Self::StreamBuffer => "StreamBufferDemo",
            Self::TaskNotify => "TaskNotify",
            Self::TimerDemo => "TimerDemo",
            Self::EventGroups => "EventGroupsDemo",
            Self::MbAmp => "MessageBufferAMP",
            Self::MessageBuffer => "MessageBufferDemo",
            Self::IntQueue => "IntQueue",
            Self::QueueSet => "QueueSet",
            Self::ApiSweep => "ApiSweep",
            Self::PollQTyped => "PollQ-typed",
            Self::PollQAsync => "PollQ-async",
        }
    }

    /// Look a scenario up by name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "dynamic" => Some(Self::Dynamic),
            "PollQ" => Some(Self::PollQ),
            "BlockQ" => Some(Self::BlockQ),
            "semtest" => Some(Self::SemTest),
            "countsem" => Some(Self::CountSem),
            "recmutex" => Some(Self::RecMutex),
            "blocktim" => Some(Self::BlockTim),
            "AbortDelay" => Some(Self::AbortDelay),
            "death" => Some(Self::Death),
            "QPeek" => Some(Self::QPeek),
            "GenQTest" => Some(Self::GenQTest),
            "QueueOverwrite" => Some(Self::QOverwrite),
            "QueueSetPolling" => Some(Self::QSetPoll),
            "IntSemTest" => Some(Self::IntSem),
            "StreamBufferInterrupt" => Some(Self::SbInt),
            "StreamBufferDemo" => Some(Self::StreamBuffer),
            "TaskNotify" => Some(Self::TaskNotify),
            "TimerDemo" => Some(Self::TimerDemo),
            "EventGroupsDemo" => Some(Self::EventGroups),
            "MessageBufferAMP" => Some(Self::MbAmp),
            "MessageBufferDemo" => Some(Self::MessageBuffer),
            "IntQueue" => Some(Self::IntQueue),
            "QueueSet" => Some(Self::QueueSet),
            "ApiSweep" => Some(Self::ApiSweep),
            "PollQ-typed" => Some(Self::PollQTyped),
            "PollQ-async" => Some(Self::PollQAsync),
            _ => None,
        }
    }

    /// Every scenario, for a runner that wants to sweep the corpus.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Dynamic,
            Self::AbortDelay,
            Self::PollQ,
            Self::BlockQ,
            Self::SemTest,
            Self::CountSem,
            Self::RecMutex,
            Self::BlockTim,
            Self::QPeek,
            Self::GenQTest,
            Self::QOverwrite,
            Self::QSetPoll,
            Self::IntSem,
            Self::SbInt,
            Self::StreamBuffer,
            Self::TaskNotify,
            Self::TimerDemo,
            Self::EventGroups,
            Self::MbAmp,
            Self::PollQTyped,
            Self::PollQAsync,
        ]
    }
}

/// The names a scenario or a firmware wants in scope.
pub mod prelude {
    pub use crate::runner::{Body, Runner, Shared, SimKernel, Step, Verdict};
    pub use crate::trace::LineTrace;
    pub use crate::{DEFAULT_STEP_LIMIT, Scenario};
    // Every scenario's `start` answers this, so a firmware that holds them
    // in a table has to be able to name it without depending on
    // `rusty_rtos_core` itself.
    pub use rusty_rtos_core::error::{Error, Result};
}
