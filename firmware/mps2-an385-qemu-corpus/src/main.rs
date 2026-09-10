//! The conformance corpus on a Cortex-M3, checked against the host's pins.
//!
//! This is K3's main clause — "the full corpus check task passes on
//! M3-qemu" — and the reason it can be answered before the family has a
//! context-switching port is the demo's task model: a scenario is a state
//! machine driven by [`Runner`], so it needs **no per-task stack and no
//! heap**. `rusty_rtos_demo-core` is `no_std` with no `alloc`, and builds
//! for `thumbv7m-none-eabi` unchanged.
//!
//! # What makes this more than "it ran"
//!
//! Each scenario is run with the same FNV-1a/64 digest sink the host's
//! `tests/conformance.rs` uses, and checked against **the same pinned
//! numbers** — ticks, yields, exits, lines, the digest and the byte count.
//! Those pins are the C kernel's: the host test diffs them against
//! `oracle/traces/*`, so matching them here means this Cortex-M build
//! produces a trace **byte-identical to the C FreeRTOS kernel's**, not
//! merely a self-consistent one.
//!
//! `exits` is the one to watch. It is sim time itself — the count of
//! outermost critical-section exits — so a port or a target that changed
//! when the scheduler ran would move it long before it moved a digest.
//!
//! # What it does not claim
//!
//! No timing. QEMU is a translator, not a pipeline simulator, and the
//! sibling `mps2-an385-qemu-region` cell measured six ways that it can
//! supply no cycle, latency or work counter at all. This cell asserts
//! only counts and a hash, which are exact on any host.

#![no_std]
#![no_main]

use core::fmt;

use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_demo_core::runner::{Runner, Shared};
use rusty_rtos_demo_core::{blockq, dynamic, genqtest, pollq, semtest, timerdemo};
use rusty_rtos_demo_core::prelude::Result;

/// The host's sink, reproduced: FNV-1a/64 over every byte of the trace, so
/// the whole thing never has to be held in RAM. Same constants as
/// `tests/conformance.rs`, which is what makes the digests comparable.
#[derive(Default)]
struct Digest {
    hash: u64,
    bytes: usize,
}

impl Digest {
    const fn new() -> Self {
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

/// One pinned scenario. The numbers are copied from the host's `PINS`
/// table, which is itself pinned against the C kernel's trace.
struct Pin {
    name: &'static str,
    start: fn(&mut Runner<'_, Digest>, u64) -> Result<()>,
    ticks: u64,
    yields: u64,
    exits: u64,
    lines: u64,
    digest: u64,
    bytes: usize,
}

/// Six of the seventeen, chosen to cover different kernel paths rather
/// than to be quick: `dynamic` is the scheduler itself, `PollQ` the
/// non-blocking queue surface, `BlockQ` blocking sends and receives,
/// `semtest` semaphores, `GenQTest` peeks and mutexes, `TimerDemo` the
/// software timers and their daemon.
const PINS: [Pin; 6] = [
    Pin { name: "dynamic",   start: dynamic::start,   ticks: 2000, yields: 3589, exits: 21346, lines: 24402, digest: 0x6bee_a9f4_66e5_1e2d, bytes: 757_383 },
    Pin { name: "PollQ",     start: pollq::start,     ticks: 2001, yields: 43,   exits: 2116,  lines: 2362,  digest: 0xf50b_bbbd_22ec_16d1, bytes: 68_195 },
    Pin { name: "BlockQ",    start: blockq::start,    ticks: 2002, yields: 3913, exits: 25681, lines: 26948, digest: 0x8832_7800_3be7_cba9, bytes: 762_294 },
    Pin { name: "semtest",   start: semtest::start,   ticks: 2000, yields: 1296, exits: 23281, lines: 30099, digest: 0x20b3_c3c7_6b70_9ce8, bytes: 684_801 },
    Pin { name: "GenQTest",  start: genqtest::start,  ticks: 2000, yields: 3013, exits: 26017, lines: 25126, digest: 0x99a0_03aa_8c2a_19b4, bytes: 683_187 },
    Pin { name: "TimerDemo", start: timerdemo::start, ticks: 2005, yields: 111,  exits: 2672,  lines: 3091,  digest: 0x3d09_b339_924c_3819, bytes: 90_192 },
];

const PIN_TICKS: u64 = 2000;

/// The host's `step_limit_for`, so the two runs stop the same way.
const fn step_limit_for(ticks: u64) -> u64 {
    ticks.saturating_mul(4096).saturating_add(1 << 20)
}

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos conformance corpus on Cortex-M3 (mps2-an385, QEMU) ===");
    hprintln!("target  thumbv7m-none-eabi, no_std, NO alloc, no per-task stack");
    hprintln!("check   every counter and the FNV-1a/64 trace digest against the");
    hprintln!("        host's pins, which are pinned against the C kernel's trace");
    hprintln!();

    let mut failed = 0u32;

    for pin in &PINS {
        let kernel = match Runner::kernel_for(Digest::new()) {
            Ok(k) => k,
            Err(_) => {
                hprintln!("{:<10} FAIL  the sim geometry refused the scenario", pin.name);
                failed += 1;
                continue;
            }
        };
        let shared = core::cell::RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            if (pin.start)(&mut runner, PIN_TICKS).is_err() {
                hprintln!("{:<10} FAIL  the scenario would not start", pin.name);
                failed += 1;
                continue;
            }
            runner.run(step_limit_for(PIN_TICKS))
        };
        let d = Runner::into_writer(kernel);

        let ok = verdict.pass
            && !verdict.runaway
            && verdict.ticks == pin.ticks
            && verdict.yields == pin.yields
            && verdict.exits == pin.exits
            && verdict.lines == pin.lines
            && d.hash == pin.digest
            && d.bytes == pin.bytes;

        if ok {
            hprintln!(
                "{:<10} ok    ticks={} yields={} exits={} lines={} bytes={}",
                pin.name, verdict.ticks, verdict.yields, verdict.exits, verdict.lines, d.bytes
            );
        } else {
            failed += 1;
            hprintln!("{:<10} FAIL", pin.name);
            hprintln!("           ticks  {:>8} want {:>8}", verdict.ticks, pin.ticks);
            hprintln!("           yields {:>8} want {:>8}", verdict.yields, pin.yields);
            hprintln!("           exits  {:>8} want {:>8}   <- sim time itself", verdict.exits, pin.exits);
            hprintln!("           lines  {:>8} want {:>8}", verdict.lines, pin.lines);
            hprintln!("           bytes  {:>8} want {:>8}", d.bytes, pin.bytes);
            hprintln!("           digest {:#018x}", d.hash);
            hprintln!("           want   {:#018x}", pin.digest);
        }
    }

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- {} scenarios byte-identical to the C kernel", PINS.len());
        hprintln!("        on a Cortex-M3, at 2000 ticks each.");
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} of {} scenarios diverged", failed, PINS.len());
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}
