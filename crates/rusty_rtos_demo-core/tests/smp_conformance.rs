//! The two-core corpus: nine standard demo scenarios, on a two-core Kairos,
//! against FreeRTOS V11.3.1 built with `configNUMBER_OF_CORES 2` -- trace for
//! trace.
//!
//! `cargo test --release -p rusty_rtos_demo-core --features smp --test smp_conformance`
//!
//! The C side is `oracle/harness-smp` in the umbrella: the pinned kernel on a
//! deterministic two-core sim port running the unmodified `Demo/Common/Minimal`
//! tasks. The contract the two sides share (`crate::smp`, and the port's
//! `portmacro.h`): cores alternate turns; a turn ends when a top-level kernel
//! call that left a critical section returns, when the core switches task, or
//! after an idle pass; a core whose partner holds the scheduler suspended is
//! skipped; ticks land on core 0 between turns.
//!
//! Each scenario is pinned at 20,000 ticks to its C trace: the line count,
//! the bytes, an FNV-1a/64 digest of every line, and the verdict -- the tick
//! and yield counts on a pass or a fail, or the `configASSERT` the demo's own
//! code stopped at. The critical-exit count is not pinned: under this
//! contract exits decide only WHETHER a call ends a turn, which the trace
//! already proves line by line, and the two kernels' assert probes differ.
//!
//! Three verdicts are failures on BOTH sides, and that is correct: `blocktim`
//! and `QPeek` measure timing that a second core changes, and `recmutex` and
//! `GenQTest` assert single-core priority exclusion in their own code. What
//! is pinned is that Kairos fails them identically, at the same line.
#![cfg(feature = "smp")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{Digest, pins};
use rusty_rtos_demo_core::runner::{Runner, Shared};

const TICKS: u64 = 20_000;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pass { ticks: u64, yields: u64 },
    Fail { ticks: u64, yields: u64 },
    Assert(&'static str, u32),
}

/// `(scenario, lines, bytes, digest, verdict)`, from the C oracle's traces.
const PINS: [(&str, u64, usize, u64, Outcome); 9] = [
    (
        "semtest",
        144008,
        4226023,
        0xbe9c_b269_a5d2_f45a,
        Outcome::Pass {
            ticks: 20002,
            yields: 13243,
        },
    ),
    (
        "dynamic",
        96334,
        2949336,
        0xea73_1d21_090b_c554,
        Outcome::Pass {
            ticks: 20046,
            yields: 10204,
        },
    ),
    (
        "PollQ",
        99538,
        2923665,
        0xe3ad_9077_c32d_ca40,
        Outcome::Pass {
            ticks: 20003,
            yields: 407,
        },
    ),
    (
        "BlockQ",
        163425,
        4937257,
        0xee87_f477_02ca_a5ed,
        Outcome::Pass {
            ticks: 20002,
            yields: 16917,
        },
    ),
    (
        "countsem",
        127851,
        3627739,
        0xc730_dcea_574a_1bae,
        Outcome::Pass {
            ticks: 20002,
            yields: 9201,
        },
    ),
    (
        "recmutex",
        37960,
        1077631,
        0x0408_5cf9_9b8c_e5a4,
        Outcome::Assert("recmutex.c", 330),
    ),
    (
        "blocktim",
        102072,
        3002960,
        0x183c_8fe1_0673_a094,
        Outcome::Fail {
            ticks: 20006,
            yields: 591,
        },
    ),
    (
        "QPeek",
        129882,
        3817680,
        0xa9ac_8a76_1fd4_3563,
        Outcome::Fail {
            ticks: 20003,
            yields: 12250,
        },
    ),
    (
        "GenQTest",
        439,
        11502,
        0xc23b_62cb_1988_e69e,
        Outcome::Assert("GenQTest.c", 564),
    ),
];

#[test]
fn nine_scenarios_trace_identically_to_the_c_kernel_on_two_cores() {
    let table = pins::<Digest>();
    for (name, lines, bytes, digest, want) in &PINS {
        let pin = table
            .iter()
            .find(|p| p.name == *name)
            .expect("a pinned scenario");
        let kernel =
            Runner::kernel_for(Digest::new()).expect("the sim geometry holds the scenario");
        let shared = RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            (pin.start)(&mut runner, TICKS).expect("the scenario starts");
            runner.run(200_000_000)
        };
        assert!(!verdict.runaway, "{name}: the scenario did not finish");
        let got = match shared.borrow().assert {
            Some((file, line)) => Outcome::Assert(file, line),
            None if verdict.pass => Outcome::Pass {
                ticks: verdict.ticks,
                yields: verdict.yields,
            },
            None => Outcome::Fail {
                ticks: verdict.ticks,
                yields: verdict.yields,
            },
        };
        assert_eq!(&got, want, "{name}: the verdict");
        assert_eq!(verdict.lines, *lines, "{name}: trace lines");
        let sink = Runner::into_writer(kernel);
        assert_eq!(sink.bytes(), *bytes, "{name}: trace bytes");
        assert_eq!(
            sink.hash(),
            *digest,
            "{name}: the trace differs from the C kernel's on two cores (oracle/harness-smp)"
        );
    }
}
