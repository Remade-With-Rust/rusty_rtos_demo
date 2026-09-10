//! The pinned corpus: what the C kernel printed, in one place.
//!
//! Three things check the Rust kernel against these numbers — the host's
//! `tests/conformance.rs`, the Cortex-M3 cell and the RV32 cell — and for
//! a while each carried its own copy of the table. Three hand-written
//! copies of seventeen rows of hex is a drift waiting to happen, and a
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
//! `exits` is the one to watch: it is sim time itself, the count of
//! outermost critical-section exits, so anything that changed *when* the
//! scheduler ran moves it long before it moves a digest.

use core::fmt;

use rusty_rtos_core::error::Result;

use crate::runner::Runner;
use crate::{
    blockq, blocktim, countsem, dynamic, eventgroups, genqtest, intsem, mbamp, pollq, pollq_typed,
    qoverwrite, qpeek, qsetpoll, recmutex, sbint, semtest, timerdemo,
};

/// How long every pinned run is. The check task ends the run at the first
/// wake-up on or after this, so a scenario can report a tick or two more.
pub const PIN_TICKS: u64 = 2000;

/// How many scenarios are pinned.
pub const COUNT: usize = 17;

/// One scenario's pinned verdict and trace digest.
pub struct Pin<W: fmt::Write> {
    /// The name `kairos conform` knows it by.
    pub name: &'static str,
    /// The scenario's `vStart...Tasks` equivalent.
    pub start: fn(&mut Runner<'_, W>, u64) -> Result<()>,
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
        for byte in s.as_bytes() {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(0x100_0000_01b3);
            self.bytes = self.bytes.wrapping_add(1);
        }
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
        // `PollQ` again, written against the Rust face (mission plan, K2.1).
        // Every number here is `PollQ`'s own, deliberately and to the digit —
        // the same digest, the same byte count, the same exits. The typed
        // queue moves a `u16` where the C-shaped one copies a `u64`, and if
        // that cost so much as one critical-section exit these two rows would
        // differ. This is the zero-cost claim, checkable with no C toolchain.
        Pin {
            name: "PollQ-typed",
            start: pollq_typed::start,
            ticks: 2001,
            yields: 43,
            exits: 2116,
            lines: 2362,
            digest: 0xf50b_bbbd_22ec_16d1,
            bytes: 68_195,
        },
    ]
}
