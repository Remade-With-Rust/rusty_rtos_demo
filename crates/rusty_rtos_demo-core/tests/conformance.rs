//! The K1 conformance regression, without a C toolchain.
//!
//! `kairos conform` is the real gate: it runs a scenario on this kernel and
//! on the instrumented C kernel and compares the two traces line for line.
//! That needs the oracle checked out, patched and built, which CI does not
//! have — so this test pins what the oracle *said*, on the day it said it,
//! and fails if the Rust kernel ever decides something else.
//!
//! The numbers below are not chosen, invented or rounded. Each row is the
//! `KAIROS_RESULT` line the C kernel printed for that scenario at 2000
//! ticks on 2026-09-09 (umbrella `docs/LEDGER.md`), and each digest is of
//! the C kernel's own trace file, not of ours. Changing any of them means
//! either the C oracle was re-pinned or the sim contract changed — both of
//! which are decision-log rows, not test edits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_demo_core::{
    Runner, blockq, blocktim, countsem, dynamic, eventgroups, genqtest, intsem, mbamp, pollq,
    qoverwrite, qpeek, qsetpoll, recmutex, sbint, semtest, step_limit_for, timerdemo,
};

/// How long every pinned run is. The check task ends the run at the first
/// wake-up on or after this, so a scenario can report a tick or two more.
const PIN_TICKS: u64 = 2000;

/// One scenario's pinned verdict and trace digest.
struct Pin {
    /// The name `kairos conform` knows it by.
    name: &'static str,
    /// The scenario's `vStart...Tasks` equivalent.
    start: fn(&mut Runner<Digest>, u64) -> Result<()>,
    /// `xTaskGetTickCount()` when the check task ended the run.
    ticks: u64,
    /// `ulKairosYields`.
    yields: u64,
    /// `ulKairosExits` — sim time itself.
    exits: u64,
    /// Trace lines, the verdict line excluded.
    lines: u64,
    /// FNV-1a/64 of those lines, newline-separated.
    digest: u64,
    /// How many bytes that is.
    bytes: usize,
}

/// What the C kernel printed for all nine scenarios at 2000 ticks.
const PINS: [Pin; 16] = [
    Pin {
        name: "dynamic",
        start: dynamic::start,
        ticks: 2000,
        yields: 3589,
        exits: 21346,
        lines: 24402,
        digest: 0x6bee_a9f4_66e5_1e2d,
        bytes: 757_383,
    },
    Pin {
        name: "PollQ",
        start: pollq::start,
        ticks: 2001,
        yields: 43,
        exits: 2116,
        lines: 2362,
        digest: 0xf50b_bbbd_22ec_16d1,
        bytes: 68_195,
    },
    Pin {
        name: "BlockQ",
        start: blockq::start,
        ticks: 2002,
        yields: 3913,
        exits: 25681,
        lines: 26948,
        digest: 0x8832_7800_3be7_cba9,
        bytes: 762_294,
    },
    Pin {
        name: "semtest",
        start: semtest::start,
        ticks: 2000,
        yields: 1296,
        exits: 23281,
        lines: 30099,
        digest: 0x20b3_c3c7_6b70_9ce8,
        bytes: 684_801,
    },
    Pin {
        name: "countsem",
        start: countsem::start,
        ticks: 2000,
        yields: 421,
        exits: 25602,
        lines: 19344,
        digest: 0x6718_535d_cbf6_5bef,
        bytes: 439_451,
    },
    Pin {
        name: "recmutex",
        start: recmutex::start,
        ticks: 2000,
        yields: 815,
        exits: 21377,
        lines: 27738,
        digest: 0x1699_053b_0e0a_58f5,
        bytes: 782_883,
    },
    Pin {
        name: "blocktim",
        start: blocktim::start,
        ticks: 2000,
        yields: 92,
        exits: 2287,
        lines: 2645,
        digest: 0x8b4b_b185_2e12_8390,
        bytes: 77_387,
    },
    Pin {
        name: "QPeek",
        start: qpeek::start,
        ticks: 2000,
        yields: 1786,
        exits: 8313,
        lines: 9774,
        digest: 0x5f7e_27d4_de97_d2e8,
        bytes: 276_939,
    },
    Pin {
        name: "GenQTest",
        start: genqtest::start,
        ticks: 2000,
        yields: 3013,
        exits: 26017,
        lines: 25126,
        digest: 0x99a0_03aa_8c2a_19b4,
        bytes: 683_187,
    },
    Pin {
        name: "QueueOverwrite",
        start: qoverwrite::start,
        ticks: 2000,
        yields: 21,
        exits: 32001,
        lines: 26021,
        digest: 0x0bc1_5e6e_8a4e_12d3,
        bytes: 532_729,
    },
    Pin {
        name: "QueueSetPolling",
        start: qsetpoll::start,
        ticks: 2000,
        yields: 688,
        exits: 21345,
        lines: 27496,
        digest: 0xf354_2654_312b_9201,
        bytes: 782_296,
    },
    Pin {
        name: "IntSemTest",
        start: intsem::start,
        ticks: 2001,
        yields: 107,
        exits: 2417,
        lines: 2702,
        digest: 0x7e9f_c49f_3acc_645e,
        bytes: 78_407,
    },
    Pin {
        name: "StreamBufferInterrupt",
        start: sbint::start,
        ticks: 2001,
        yields: 28,
        exits: 2085,
        lines: 2273,
        digest: 0xfd91_ee4f_e21d_dd17,
        bytes: 66_145,
    },
    Pin {
        name: "TimerDemo",
        start: timerdemo::start,
        ticks: 2005,
        yields: 111,
        exits: 2672,
        lines: 3091,
        digest: 0x3d09_b339_924c_3819,
        bytes: 90_192,
    },
    Pin {
        name: "EventGroupsDemo",
        start: eventgroups::start,
        ticks: 2001,
        yields: 4991,
        exits: 16_577,
        lines: 24_157,
        digest: 0xfffc_9266_b473_a8f0,
        bytes: 702_507,
    },
    Pin {
        name: "MessageBufferAMP",
        start: mbamp::start,
        ticks: 2001,
        yields: 71,
        exits: 2072,
        lines: 2430,
        digest: 0x1d93_4bbd_dc9e_03d4,
        bytes: 71_089,
    },
];

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
#[cfg_attr(
    miri,
    ignore = "2000 ticks x 9 scenarios is hours under Miri;               `every_scenario_is_deterministic` covers the same code there"
)]
fn every_scenario_reproduces_the_c_kernels_trace_and_counters() {
    for pin in &PINS {
        let mut runner = Runner::new(Digest::new()).expect("the sim geometry holds the scenario");
        (pin.start)(&mut runner, PIN_TICKS).expect("the scenario starts");
        let verdict = runner.run(step_limit_for(PIN_TICKS));

        assert!(
            !verdict.runaway,
            "{}: the scenario did not finish",
            pin.name
        );
        assert!(verdict.pass, "{}: failed its own check", pin.name);
        assert_eq!(verdict.ticks, pin.ticks, "{}: tick count", pin.name);
        assert_eq!(
            verdict.yields, pin.yields,
            "{}: portYIELD() calls",
            pin.name
        );
        assert_eq!(
            verdict.exits, pin.exits,
            "{}: outermost critical-section exits, which is sim time itself",
            pin.name
        );
        assert_eq!(verdict.lines, pin.lines, "{}: trace lines", pin.name);

        let digest = runner.into_writer();
        assert_eq!(digest.bytes, pin.bytes, "{}: trace size in bytes", pin.name);
        assert_eq!(
            digest.hash,
            pin.digest,
            "{name}: the trace differs from the one the C kernel produced; run \
             `kairos conform {name} --exits` to see the first line that moved",
            name = pin.name
        );
    }
}

/// Two runs must agree — and, under Miri, this is also the only test that
/// puts the kernel's arenas and lists through an interpreter that checks
/// them. Twenty ticks is short enough to finish there and long enough for
/// every scenario to have created its objects, started its tasks and gone
/// round its loop.
#[test]
fn every_scenario_is_deterministic() {
    let ticks: u64 = if cfg!(miri) { 20 } else { 500 };
    for pin in &PINS {
        let run = || {
            let mut runner = Runner::new(Digest::new()).unwrap();
            (pin.start)(&mut runner, ticks).unwrap();
            let verdict = runner.run(step_limit_for(ticks));
            (verdict.exits, verdict.lines, runner.into_writer().hash)
        };
        let (exits, lines, hash) = run();
        assert!(lines > 0, "{}: the scenario traced nothing", pin.name);
        assert_eq!(
            (exits, lines, hash),
            run(),
            "{}: two runs of the sim must agree",
            pin.name
        );
    }
}
