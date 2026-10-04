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
//!
//! # Two widths
//!
//! The Posix port's `TickType_t` is `unsigned long` and a message buffer's
//! length prefix is `size_t`, so the C kernel's trace depends on the width
//! of the machine it ran on: `portMAX_DELAY` prints as 2^64-1 on the x86_64
//! host and as 2^32-1 under `-m32`, and a message costs eight bytes of
//! buffer or four. `PosixDemoConfig` follows the build's width, so a pin
//! must too: where the two C runs differ, a row carries both, 64-bit first,
//! and [`w`] picks. Every 32-bit number is the `-m32` C kernel's own
//! (`oracle/pin.py`, umbrella `docs/HOLES.md` H13), and only `lines`,
//! `bytes` and `digest` ever differ -- the two widths agree on every
//! scenario's ticks, yields and exits, which is the evidence that the width
//! changes what the trace SAYS and not when anything runs.
//!
//! The 32-bit rows are what the Cortex-M3 and RV32 cells check, and what
//! `tests/conformance.rs` checks when built for a 32-bit host.

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

/// Which C build this build's pins come from: the x86-64 one on a 64-bit
/// build, the `-m32` one otherwise (see "Two widths" above).
const PINS_64: bool = cfg!(target_pointer_width = "64");

/// `sizeof( size_t )` in the C build the pins come from. A scenario whose C
/// spells a length with it must use this, never `size_of::<usize>()` on its
/// own: the two agree on every target today, but only this one is tied to
/// the table it is checked against.
pub const ORACLE_SIZE_T: usize = if PINS_64 { 8 } else { 4 };

// The config the scenarios run must be the one the pins were taken under.
// `rusty_rtos_core` before H13 typed `PosixDemoConfig` at 64 bits on every
// build; against it a 32-bit build would fail every row on a digest, which
// reads like a kernel bug. This says what it is instead.
const _: () = assert!(
    <rusty_rtos_core::config::PosixDemoConfig as rusty_rtos_core::config::Config>::MESSAGE_LENGTH_BYTES
        == ORACLE_SIZE_T,
    "PosixDemoConfig does not follow this build's width: a 32-bit build needs the rusty_rtos_core that types it at 32 bits (HOLES.md H13)"
);

/// The 64-bit C kernel's number on a 64-bit build, the `-m32` C kernel's on
/// a 32-bit one.
#[inline]
fn w<T>(bits64: T, bits32: T) -> T {
    if PINS_64 { bits64 } else { bits32 }
}

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
            digest: w(0x6bee_a9f4_66e5_1e2d, 0x4733_4b4b_0fac_1d6b),
            bytes: w(757_383, 757_373),
        },
        Pin {
            name: "AbortDelay",
            start: abortdelay::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 86,
            exits: 2198,
            lines: 2548,
            digest: w(0x54b8_aee9_7f55_5577, 0x3965_3b98_89b0_e209),
            bytes: w(74_868, 74_858),
        },
        Pin {
            name: "PollQ",
            start: pollq::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 43,
            exits: 2116,
            lines: 2362,
            digest: w(0xf50b_bbbd_22ec_16d1, 0x7578_7588_bb26_7733),
            bytes: w(68_195, 68_185),
        },
        Pin {
            name: "BlockQ",
            start: blockq::start,
            run_ticks: PIN_TICKS,
            ticks: 2002,
            yields: 3913,
            exits: 25681,
            lines: 26948,
            digest: w(0x8832_7800_3be7_cba9, 0x961c_2d7b_0854_fb5b),
            bytes: w(762_294, 762_284),
        },
        Pin {
            name: "semtest",
            start: semtest::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 1296,
            exits: 23281,
            lines: 30099,
            digest: w(0x20b3_c3c7_6b70_9ce8, 0xa111_63bf_e4db_d28e),
            bytes: w(684_801, 684_791),
        },
        Pin {
            name: "countsem",
            start: countsem::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 421,
            exits: 25602,
            lines: 19344,
            digest: w(0x6718_535d_cbf6_5bef, 0xf8da_dfa4_3fb9_2249),
            bytes: w(439_451, 439_441),
        },
        Pin {
            name: "recmutex",
            start: recmutex::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 815,
            exits: 21377,
            lines: 27738,
            digest: w(0x1699_053b_0e0a_58f5, 0xca2e_aee2_5092_0e8b),
            bytes: w(782_883, 782_873),
        },
        Pin {
            name: "blocktim",
            start: blocktim::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 92,
            exits: 2287,
            lines: 2645,
            digest: w(0x8b4b_b185_2e12_8390, 0x9fca_f641_c78f_1dba),
            bytes: w(77_387, 77_377),
        },
        Pin {
            name: "QPeek",
            start: qpeek::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 1786,
            exits: 8313,
            lines: 9774,
            digest: w(0x5f7e_27d4_de97_d2e8, 0x5167_2ac6_7e20_9bde),
            bytes: w(276_939, 276_929),
        },
        Pin {
            name: "GenQTest",
            start: genqtest::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 3013,
            exits: 26017,
            lines: 25126,
            digest: w(0x99a0_03aa_8c2a_19b4, 0xf5da_f40c_82a8_13a2),
            bytes: w(683_187, 683_177),
        },
        Pin {
            name: "QueueOverwrite",
            start: qoverwrite::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 21,
            exits: 32001,
            lines: 26021,
            digest: w(0x0bc1_5e6e_8a4e_12d3, 0xc296_2498_b571_872d),
            bytes: w(532_729, 532_719),
        },
        Pin {
            name: "QueueSetPolling",
            start: qsetpoll::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 688,
            exits: 21345,
            lines: 27496,
            digest: w(0xf354_2654_312b_9201, 0xd6a0_edec_3166_b217),
            bytes: w(782_296, 782_286),
        },
        Pin {
            name: "IntSemTest",
            start: intsem::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 107,
            exits: 2417,
            lines: 2702,
            digest: w(0x7e9f_c49f_3acc_645e, 0x26d0_88eb_ab8e_5a90),
            bytes: w(78_407, 78_397),
        },
        Pin {
            name: "StreamBufferInterrupt",
            start: sbint::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 28,
            exits: 2085,
            lines: 2273,
            digest: w(0xfd91_ee4f_e21d_dd17, 0xe18d_097b_4bc3_3699),
            bytes: w(66_145, 66_135),
        },
        Pin {
            name: "StreamBufferDemo",
            start: streambuffer::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 2424,
            exits: 28002,
            lines: 20927,
            digest: w(0xf159_2634_a152_db89, 0x6ac3_5c04_333d_c136),
            bytes: w(676_662, 676_846),
        },
        Pin {
            name: "MessageBufferDemo",
            start: messagebuffer::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 2560,
            exits: 28097,
            lines: w(19_587, 22_541),
            digest: w(0x5cb3_12f2_6ca3_18f9, 0x8a0b_f8b9_9c4f_6b55),
            bytes: w(620_657, 708_569),
        },
        Pin {
            name: "QueueSet",
            start: qset::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 624,
            exits: 6137,
            lines: 6280,
            digest: w(0x2f8a_570d_f721_e2bc, 0xc36c_b2cb_ed00_b882),
            bytes: w(167_059, 167_049),
        },
        Pin {
            name: "IntQueue",
            start: intqueue::start,
            run_ticks: PIN_TICKS,
            ticks: 2002,
            yields: 6999,
            exits: 31827,
            lines: 44284,
            digest: w(0x595e_402b_5576_5d65, 0x78b3_84fc_bc1f_d0d7),
            bytes: w(1_279_335, 1_279_325),
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
            digest: w(0x2438_48b2_612f_ff91, 0x6942_6448_9163_f45b),
            bytes: w(143_106, 143_096),
        },
        Pin {
            name: "TaskNotify",
            start: tasknotify::start,
            run_ticks: PIN_TICKS,
            ticks: 2001,
            yields: 227,
            exits: 2845,
            lines: 3518,
            digest: w(0x54c5_765b_0638_11ef, 0xf4e4_7f6d_899f_8ae1),
            bytes: w(105_221, 105_211),
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
            digest: w(0x8951_8b1d_73e2_6404, 0x855f_e2a2_3e28_b962),
            bytes: w(707_007, 706_997),
        },
        Pin {
            name: "MessageBufferAMP",
            start: mbamp::start,
            run_ticks: PIN_TICKS,
            ticks: 2000,
            yields: 75,
            exits: 2090,
            lines: 2445,
            digest: w(0x0f72_591f_32d8_4bec, 0x7721_e91a_1629_b4ee),
            bytes: w(71_529, 71_519),
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
            digest: w(0xf50b_bbbd_22ec_16d1, 0x7578_7588_bb26_7733),
            bytes: w(68_195, 68_185),
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
            digest: w(0xec10_62cf_1c36_07ea, 0x97c5_5839_eb46_3b68),
            bytes: w(129_014, 129_004),
        },
    ]
}
