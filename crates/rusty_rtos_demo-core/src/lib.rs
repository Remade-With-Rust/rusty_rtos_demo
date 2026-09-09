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
//! | [`pollq`] | `PollQ.c` | a queue polled from both ends, never blocking |
//! | [`blockq`] | `BlockQ.c` | blocking sends and receives, three task pairs |
//! | [`semtest`] | `semtest.c` | two binary semaphores guarding a shared variable |
//! | [`countsem`] | `countsem.c` | counting semaphores, driven to both ends |
//! | [`recmutex`] | `recmutex.c` | a recursive mutex and its priority inheritance |
//! | [`blocktim`] | `blocktim.c` | block times and `xTaskDelayUntil`, to the tick |
//! | [`qpeek`] | `QPeek.c` | peeking, and the order four priorities wake in |
//! | [`genqtest`] | `GenQTest.c` | both queue ends, and priority inheritance |
//!
//! The other eight scenarios of the K1 corpus follow the same shape and
//! land as they are written; the plan's kill test is the whole nine.

pub mod blockq;
pub mod blocktim;
pub mod countsem;
pub mod dynamic;
pub mod genqtest;
pub mod pollq;
pub mod qpeek;
pub mod recmutex;
pub mod runner;
pub mod semtest;
pub mod trace;

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
    /// `QPeek.c`.
    QPeek,
    /// `GenQTest.c`.
    GenQTest,
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
            Self::QPeek => "QPeek",
            Self::GenQTest => "GenQTest",
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
            "QPeek" => Some(Self::QPeek),
            "GenQTest" => Some(Self::GenQTest),
            _ => None,
        }
    }

    /// Every scenario, for a runner that wants to sweep the corpus.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Dynamic,
            Self::PollQ,
            Self::BlockQ,
            Self::SemTest,
            Self::CountSem,
            Self::RecMutex,
            Self::BlockTim,
            Self::QPeek,
            Self::GenQTest,
        ]
    }
}

/// The names a scenario or a firmware wants in scope.
pub mod prelude {
    pub use crate::runner::{Body, Runner, Shared, SimKernel, Step, Verdict};
    pub use crate::trace::LineTrace;
    pub use crate::{DEFAULT_STEP_LIMIT, Scenario};
}
