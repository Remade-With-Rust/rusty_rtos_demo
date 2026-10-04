//! The two-core corpus: twenty-three standard demo scenarios, on a two-core Kairos,
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
//! Eight verdicts are failures on BOTH sides, and that is correct: `blocktim`,
//! `QPeek`, `AbortDelay`, `ApiSweep` and `IntQueue` measure timing or
//! ordering that a second core changes, and `recmutex`, `GenQTest`,
//! `TimerDemo` and `EventGroupsDemo` assert single-core assumptions in their
//! own code. What is pinned is that Kairos fails them identically, at the
//! same line.
//!
//! `death` is the one scenario of the twenty-four not here: on two cores its
//! own code is undefined. `vCreateTasks` hands `SUICID1` a pointer to the
//! handle it writes only when it creates `SUICID2`; one core cannot run
//! `SUICID1` in between, two can, and it deletes the previous cycle's freed
//! TCB (the C segfaults). There is no "fails identically" for that.
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

/// A 64-bit build. The C kernel's trace depends on the machine's width (a
/// message buffer's length prefix is `size_t`), and so does
/// `PosixDemoSmpConfig`; where the two-core C runs differ, a row carries the
/// 64-bit number first and the `-m32` C kernel's second (umbrella
/// `docs/HOLES.md` H13). Three rows do: the stream and message buffers. In
/// `MessageBufferDemo` the four-byte prefix even changes WHEN things run --
/// more messages fit -- so its verdict moves too.
const W64: bool = cfg!(target_pointer_width = "64");

/// `(scenario, lines, bytes, digest, verdict)`, from the C oracle's traces.
const PINS: [(&str, u64, usize, u64, Outcome); 23] = [
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
    (
        "AbortDelay",
        103101,
        3041909,
        0x5c22_25fb_7fae_7c76,
        Outcome::Fail {
            ticks: 20002,
            yields: 747,
        },
    ),
    (
        "ApiSweep",
        102104,
        3031038,
        0xff27_da44_ea37_e7f8,
        Outcome::Fail {
            ticks: 20003,
            yields: 2808,
        },
    ),
    (
        "EventGroupsDemo",
        1910,
        51822,
        0xb25c_5de6_094a_8615,
        Outcome::Assert("EventGroupsDemo.c", 260),
    ),
    (
        "IntQueue",
        269922,
        7859155,
        0x0291_ff45_7612_4942,
        Outcome::Fail {
            ticks: 20065,
            yields: 17965,
        },
    ),
    (
        "IntSemTest",
        105505,
        3106388,
        0xd7f5_1f15_64d8_6f42,
        Outcome::Pass {
            ticks: 20002,
            yields: 1793,
        },
    ),
    (
        "MessageBufferAMP",
        103422,
        3048548,
        if W64 {
            0x64c2_b155_8782_3f36
        } else {
            0xbd38_8864_9a3b_4d7e
        },
        Outcome::Pass {
            ticks: 20002,
            yields: 901,
        },
    ),
    (
        "MessageBufferDemo",
        if W64 { 134044 } else { 135512 },
        if W64 { 4347844 } else { 4392918 },
        if W64 {
            0x6506_439b_7681_3dc3
        } else {
            0x6367_bf74_2e1a_a296
        },
        Outcome::Pass {
            ticks: if W64 { 20011 } else { 20002 },
            yields: if W64 { 12654 } else { 12594 },
        },
    ),
    (
        "QueueOverwrite",
        94709,
        2669597,
        0xc1ac_cd77_3363_df91,
        Outcome::Pass {
            ticks: 20002,
            yields: 203,
        },
    ),
    (
        "QueueSet",
        111923,
        3222034,
        0x9a2d_daea_ed22_82e3,
        Outcome::Pass {
            ticks: 20003,
            yields: 6059,
        },
    ),
    (
        "QueueSetPolling",
        137179,
        4069821,
        0xd529_ec9b_54aa_8da9,
        Outcome::Pass {
            ticks: 20002,
            yields: 13453,
        },
    ),
    (
        "StreamBufferDemo",
        118590,
        if W64 { 3894233 } else { 3894575 },
        if W64 {
            0x9100_da4d_fd57_de30
        } else {
            0xc5d4_bf70_6674_de9c
        },
        Outcome::Pass {
            ticks: 20005,
            yields: 8943,
        },
    ),
    (
        "StreamBufferInterrupt",
        100569,
        2957818,
        0xbee7_4aa2_f0b9_ad2b,
        Outcome::Pass {
            ticks: 20003,
            yields: 269,
        },
    ),
    (
        "TaskNotify",
        107488,
        3206594,
        0xffb7_3ebd_b4f6_1e54,
        Outcome::Pass {
            ticks: 20002,
            yields: 3237,
        },
    ),
    (
        "TimerDemo",
        8327,
        243858,
        0x390d_ec54_424c_8527,
        Outcome::Assert("TimerDemo.c", 427),
    ),
];

#[test]
fn twenty_three_scenarios_trace_identically_to_the_c_kernel_on_two_cores() {
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
