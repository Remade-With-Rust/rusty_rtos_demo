//! The K1 conformance regression, without a C toolchain.
//!
//! `kairos conform` is the real gate: it runs the scenario on this kernel
//! and on the instrumented C kernel and compares the two traces line for
//! line. That needs the oracle checked out, patched and built, which CI
//! does not have — so this test pins what the oracle *said*, on the day it
//! said it, and fails if the Rust kernel ever decides something else.
//!
//! The numbers below are not chosen, invented or rounded. They are the
//! `KAIROS_RESULT` line the C kernel printed for `dynamic` at 2000 ticks on
//! 2026-09-09 (umbrella `docs/LEDGER.md`), and the digest is of the trace
//! that came with it. Changing any of them means either the C oracle was
//! re-pinned or the sim contract changed — both of which are decision-log
//! rows, not test edits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::fmt;

use rusty_rtos_demo_core::{DEFAULT_STEP_LIMIT, Runner, dynamic};

/// What the C kernel printed for `dynamic` at 2000 ticks.
const ORACLE_TICKS: u64 = 2000;
const ORACLE_YIELDS: u64 = 3589;
const ORACLE_EXITS: u64 = 21346;
const ORACLE_LINES: u64 = 24402;
/// FNV-1a/64 of the trace bytes, newline-separated, verdict line excluded.
const ORACLE_DIGEST: u64 = 0x6bee_a9f4_66e5_1e2d;
const ORACLE_BYTES: usize = 757_383;

/// A sink that digests as it goes, so the whole trace never has to be held.
#[derive(Default)]
struct Digest {
    hash: u64,
    bytes: usize,
}

impl Digest {
    fn new() -> Self {
        Self {
            hash: 0xcbf2_9ce4_8422_2325,
            bytes: 0,
        }
    }
}

impl fmt::Write for Digest {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.as_bytes() {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(0x100_0000_01b3);
            self.bytes = self.bytes.wrapping_add(1);
        }
        Ok(())
    }
}

#[test]
fn dynamic_reproduces_the_c_kernels_trace_and_counters() {
    let mut runner = Runner::new(Digest::new()).expect("the sim geometry holds the scenario");
    dynamic::start(&mut runner, ORACLE_TICKS).expect("the scenario starts");
    let verdict = runner.run(DEFAULT_STEP_LIMIT);

    assert!(!verdict.runaway, "the scenario did not finish");
    assert!(verdict.pass, "the scenario failed its own check");
    assert_eq!(verdict.ticks, ORACLE_TICKS, "tick count");
    assert_eq!(verdict.yields, ORACLE_YIELDS, "portYIELD() calls");
    assert_eq!(
        verdict.exits, ORACLE_EXITS,
        "outermost critical-section exits: sim time itself"
    );
    assert_eq!(verdict.lines, ORACLE_LINES, "trace lines");

    let digest = runner.into_writer();
    assert_eq!(digest.bytes, ORACLE_BYTES, "trace size in bytes");
    assert_eq!(
        digest.hash, ORACLE_DIGEST,
        "the trace differs from the one the C kernel produced; run \
         `kairos conform dynamic --exits` to see the first line that moved"
    );
}

#[test]
fn the_scenario_is_deterministic() {
    let run = || {
        let mut runner = Runner::new(Digest::new()).unwrap();
        dynamic::start(&mut runner, 500).unwrap();
        let verdict = runner.run(DEFAULT_STEP_LIMIT);
        (verdict.exits, verdict.lines, runner.into_writer().hash)
    };
    assert_eq!(run(), run(), "two runs of the sim must agree");
}
