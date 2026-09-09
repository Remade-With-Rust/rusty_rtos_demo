#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_rtos_demo` — The FreeRTOS standard demo tasks (Demo/Common/Minimal) remade in Rust as the Kairos conformance corpus: every scenario self-checks like the C original and, on the sim port, diffs its trace against the C kernel's.
//!
//! This is the facade: it re-exports the `no_std` core. Depend on this crate;
//! reach into the sub-crates only when you are building a port or a backend.
//!
//! Part of Kairos (Remade With Rust). Plan: `docs/plans/rusty_rtos_demo.md`.

pub use rusty_rtos_demo_core::*;

/// The names a firmware wants in scope.
pub mod prelude {
    pub use rusty_rtos_demo_core::prelude::*;
}
