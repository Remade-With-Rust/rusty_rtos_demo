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
//! | scenario | C file | state |
//! |---|---|---|
//! | [`dynamic`] | `dynamic.c` | remade |
//!
//! The other eight scenarios of the K1 corpus follow the same shape and
//! land as they are written; the plan's kill test is the whole nine.

pub mod dynamic;
pub mod runner;
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
pub const DEFAULT_STEP_LIMIT: u64 = 50_000_000;

/// The scenarios this crate can run.
// Deliberately exhaustive: adding a scenario should fail to compile
// everywhere it has to be wired, starting with the binary's `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// `dynamic.c`.
    Dynamic,
}

impl Scenario {
    /// The scenario's name, as the trace's verdict line prints it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dynamic => "dynamic",
        }
    }

    /// Look a scenario up by name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "dynamic" => Some(Self::Dynamic),
            _ => None,
        }
    }

    /// Every scenario, for a runner that wants to sweep the corpus.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Dynamic]
    }
}

/// The names a scenario or a firmware wants in scope.
pub mod prelude {
    pub use crate::runner::{Body, Runner, Shared, SimKernel, Step, Verdict};
    pub use crate::trace::LineTrace;
    pub use crate::{DEFAULT_STEP_LIMIT, Scenario};
}
