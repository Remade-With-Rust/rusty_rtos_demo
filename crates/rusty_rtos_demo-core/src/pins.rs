//! The pinned corpus: what the C kernel printed, in one place.
//!
//! Three things check the Rust kernel against these numbers — the host's
//! `tests/conformance.rs`, the Cortex-M3 cell and the RV32 cell — and for
//! a while each carried its own copy of the table. Three hand-written
//! copies of twenty rows of hex is a drift waiting to happen, and a
//! cell that silently disagrees with the host is worse than no cell: it
//! reports PASS against numbers nobody is comparing.
//!
//! So the table lives here and they all read it. A pin that moves, moves
//! once.
//!
//! # Where the numbers come from
//!
//! Each row is the `KAIROS_RESULT` line the **C kernel** printed for that
//! scenario at 2000 ticks (umbrella `docs/LEDGER.md`, 2026-09-09), and
//! each digest is of the C kernel's own trace file, not of ours. Changing
//! one means either the oracle was re-pinned or the sim contract changed
//! — both decision-log rows, not edits.
//!
//! `exits` is the one to watch: it is sim time itself, and anything that
//! changed *when* the scheduler ran moves it long before it moves a digest.
//!
//! # These are contract v2 numbers
//!
//! Under v1 `exits` counted outermost critical-section exits and nothing
//! else. v2 adds one more kind of kernel-visible point -- a kernel call
//! that returned without taking a section, which costs one empty section --
//! so `exits` is now the count of kernel-visible points of both kinds.
//!
//! The change moved exactly **two** rows, `StreamBufferDemo` and
//! `MessageBufferAMP`, because stream and message buffers are the only
//! objects with a call that can return blind. Every other row below is the
//! same number it was under v1, which is the evidence that the widening was
//! as narrow as it was meant to be. See `docs/HOLES.md`, H9.

use core::fmt;

use rusty_rtos_core::error::Result;

use crate::runner::Runner;
use crate::{
    abortdelay, apisweep, blockq, blocktim, countsem, death, dynamic, eventgroups, genqtest,
    intqueue, intsem, mbamp, messagebuffer, pollq, pollq_typed, qoverwrite, qpeek, qset, qsetpoll,
    recmutex, sbint, semtest, streambuffer, tasknotify, timerdemo,
};

/// The DEFAULT length of a pinned run. The check task ends the run at the
/// first wake-up on or after this, so a scenario can report a tick or two
/// more.
///
/// It is a default and no longer a universal: see [`Pin::run_ticks`].
pub const PIN_TICKS: u64 = 2000;

/// How many scenarios are pinned.
pub const COUNT: usize = 25;

/// One scenario's pinned verdict and trace digest.
pub struct Pin<W: fmt::Write> {
    /// The name `kairos conform` knows it by.
    pub name: &'static str,
    /// The scenario's `vStart...Tasks` equivalent.
    pub start: fn(&mut Runner<'_, W>, u64) -> Result<()>,
    /// How long this scenario must be RUN for, which is not the same
    /// question as how many ticks it ended on.
    ///
    /// Almost every scenario is doing its job by the first tick, so
    /// [`PIN_TICKS`] is enough. `death` is not: its creator waits a whole
    /// second before it counts tasks, another before it spawns any, and the
    /// spawned tasks wait 200 ticks more before killing anything — so at
    /// 2,000 ticks it ends ON the first creation having deleted nothing,
    /// and would pin a trace that never exercises the feature it exists to
    /// test. A run length that is too short is not a weaker gate; it is a
    /// gate that cannot fail.
    pub run_ticks: u64,
    /// `xTaskGetTickCount()` when the check task ended the run.
    pub ticks: u64,
    /// `ulKairosYields`.
    pub yields: u64,
    /// `ulKairosExits` — sim time itself.
    pub exits: u64,
    /// Trace lines, the verdict line excluded.
    pub lines: u64,
    /// FNV-1a/64 of those lines, newline-separated.
    pub digest: u64,
    /// How many bytes that is.
    pub bytes: usize,
}

/// A sink that digests as it goes, so the whole trace never has to be held
/// in RAM — which is what lets a 780 KB trace be checked on a chip with
/// far less than that.
///
/// It is here rather than in each consumer for the same reason the table
/// is: two hand-copied FNV constants that drift produce two digests that
/// can never agree, and the failure looks like a kernel bug.
pub struct Digest {
    hash: u64,
    bytes: usize,
}

impl Digest {
    /// The FNV-1a/64 offset basis.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hash: 0xcbf2_9ce4_8422_2325,
            bytes: 0,
        }
    }

    /// The digest of everything written so far.
    #[must_use]
    pub const fn hash(&self) -> u64 {
        self.hash
    }

    /// How many bytes that was.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Default for Digest {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for Digest {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // The accumulator rides in a local. The loop this replaces did a
        // read-modify-write of TWO struct fields for every byte -- through
        // `&mut self`, where the compiler will not keep them in registers --
        // and the byte counter is just the length, known before the loop.
        // Same FNV-1a over the same bytes, same count.
        let mut hash = self.hash;
        for byte in s.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        self.hash = hash;
        self.bytes = self.bytes.wrapping_add(s.len());
        Ok(())
    }
}

/// The pinned corpus, over any sink.
///
/// A function rather than a `const`, because `start` is generic over the
/// sink the trace is written to: the host digests into its own buffer, a
/// firmware cell digests into RAM it does not have to keep.
#[must_use]
pub fn pins<W: fmt::Write>() -> [Pin<W>; COUNT] {
    [
        Pin {
            name: "dynamic",
            start: dynamic::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 3589,
            exits: 21346,
            lines: 24402,
            digest: 0x6bee_a9f4_66e5_1e2d,
            bytes: 757_383,
        },
        Pin {
            name: "AbortDelay",
            start: abortdelay::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 86,
            exits: 2198,
            lines: 2548,
            digest: 0x54b8_aee9_7f55_5577,
            bytes: 74_868,
        },
        Pin {
            name: "PollQ",
            start: pollq::start,
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 28,
            exits: 2085,
            lines: 2273,
            digest: 0xfd91_ee4f_e21d_dd17,
            bytes: 66_145,
        },
        Pin {
            name: "StreamBufferDemo",
            start: streambuffer::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 2424,
            exits: 28002,
            lines: 20927,
            digest: 0xf159_2634_a152_db89,
            bytes: 676_662,
        },
        Pin {
            name: "MessageBufferDemo",
            start: messagebuffer::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 2560,
            exits: 28097,
            lines: 19587,
            digest: 0x5cb3_12f2_6ca3_18f9,
            bytes: 620_657,
        },
        Pin {
            name: "QueueSet",
            start: qset::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 624,
            exits: 6137,
            lines: 6280,
            digest: 0x2f8a_570d_f721_e2bc,
            bytes: 167_059,
        },
        Pin {
            name: "IntQueue",
            start: intqueue::start,
            run_ticks: PIN_TICKS,
            ticks: 2002,
            yields: 6999,
            exits: 31827,
            lines: 44284,
            digest: 0x595e_402b_5576_5d65,
            bytes: 1_279_335,
        },
        Pin {
            // KAIROS-authored rather than ported; see oracle/harness/ApiSweep.c.
            name: "ApiSweep",
            start: apisweep::start,
            run_ticks: PIN_TICKS,
            ticks: 2003,
            yields: 354,
            exits: 3505,
            lines: 4897,
            digest: 0x2438_48b2_612f_ff91,
            bytes: 143_106,
        },
        Pin {
            name: "TaskNotify",
            start: tasknotify::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 227,
            exits: 2845,
            lines: 3518,
            digest: 0x54c5_765b_0638_11ef,
            bytes: 105_221,
        },
        Pin {
            name: "TimerDemo",
            start: timerdemo::start,
            run_ticks: PIN_TICKS,
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
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 4991,
            exits: 16_577,
            lines: 24_157,
            digest: 0x8951_8b1d_73e2_6404,
            bytes: 707_007,
        },
        Pin {
            name: "MessageBufferAMP",
            start: mbamp::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 75,
            exits: 2090,
            lines: 2445,
            digest: 0x0f72_591f_32d8_4bec,
            bytes: 71_529,
        },
        // `PollQ` again, written against the Rust face (mission plan, K2.1).
        // Every number here is `PollQ`'s own, deliberately and to the digit —
        // the same digest, the same byte count, the same exits. The typed
        // queue moves a `u16` where the C-shaped one copies a `u64`, and if
        // that cost so much as one critical-section exit these two rows would
        // differ. This is the zero-cost claim, checkable with no C toolchain.
        Pin {
            name: "PollQ-typed",
            start: pollq_typed::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 43,
            exits: 2116,
            lines: 2362,
            digest: 0xf50b_bbbd_22ec_16d1,
            bytes: 68_195,
        },
        // `death.c`: the only scenario that deletes a task, and the only one
        // that creates one with the scheduler already running. Its numbers
        // are the C kernel's at 4,000 ticks — two full create/kill/self-kill
        // cycles — and no shorter run reaches a `vTaskDelete` at all.
        Pin {
            name: "death",
            start: death::start,
            run_ticks: 4_000,
            ticks: 4000,
            yields: 55,
            exits: 3890,
            lines: 4369,
            digest: 0xec10_62cf_1c36_07ea,
            bytes: 129_014,
        },
    ]
}
