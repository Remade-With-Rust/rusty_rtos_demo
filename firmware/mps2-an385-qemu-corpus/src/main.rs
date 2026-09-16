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
//! Every scenario is run with the same FNV-1a/64 digest sink the host's
//! `tests/conformance.rs` uses, and checked against **the same pinned
//! numbers** — ticks, yields, exits, lines, the digest and the byte count.
//! Those pins are the C kernel's: the host test diffs them against
//! `oracle/traces/*`, so matching them here means this Cortex-M build
//! produces a trace **byte-identical to the C FreeRTOS kernel's**, not
//! merely a self-consistent one.
//!
//! Not a chosen subset, and not a second copy of the table: the whole
//! corpus runs, and all three consumers — this cell, the RV32 cell and
//! the host test — read `rusty_rtos_demo_core::pins`. A cell cannot
//! silently disagree with the host about what the C kernel said.
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

use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

use rusty_rtos_demo_core::pins::{Digest, pins};

/// How long each scenario runs, and therefore what is checked.
///
/// The default is the pinned 2000 ticks, where the C kernel's trace exists
/// and every counter and the digest are compared against it. `--features
/// soak` runs an hour of simulated time instead — 3,600,000 ticks at the
/// oracle's own `TICK_RATE_HZ` of 1000 — where no C pin exists, so the
/// check becomes the one the C demo itself makes: is every scenario's own
/// check task still reporting that it is running?
#[cfg(not(feature = "soak"))]
const RUN_TICKS: u64 = rusty_rtos_demo_core::pins::PIN_TICKS;
#[cfg(feature = "soak")]
const RUN_TICKS: u64 = soak_ticks();

/// Override the soak length, set at compile time:
///
/// ```sh
/// KAIROS_SOAK_TICKS=200000 KAIROS_SOAK_ONLY=semtest cargo run --release --features soak
/// ```
///
/// The soak asks one question -- is this scenario still running after an
/// hour -- and when the answer turns out to be "no" that question gives no
/// purchase on WHY. A length that can be moved turns a failure into a
/// bisection: the tick count where a scenario stops surviving is a fact
/// about the scenario, and a scenario that fails at every length is a
/// different fault from one that fails past some threshold.
///
/// `option_env!` resolves at COMPILE time. Unset leaves the hour alone.
#[cfg(feature = "soak")]
const SOAK_TICKS: Option<&str> = option_env!("KAIROS_SOAK_TICKS");

/// The soak length actually used: the override if one parsed, else an hour.
#[cfg(feature = "soak")]
const fn soak_ticks() -> u64 {
    match SOAK_TICKS {
        // `parse` is not const, so this walks the bytes. A malformed value
        // is a typo in a command line, and falling back to the full hour
        // would hide it -- so it saturates to 0 and the run reports a
        // scenario that did not reach its length, which is visible.
        Some(s) => {
            let b = s.as_bytes();
            let mut i = 0;
            let mut n: u64 = 0;
            while i < b.len() {
                if b[i] < b'0' || b[i] > b'9' {
                    return 0;
                }
                n = n * 10 + (b[i] - b'0') as u64;
                i += 1;
            }
            n
        }
        None => 3_600_000,
    }
}


/// Run only ONE scenario of the soak, named at compile time:
///
/// ```sh
/// KAIROS_SOAK_ONLY=death cargo run --release --features soak
/// ```
///
/// The full soak is all 18 scenarios at 3,600,000 ticks, which is hours on
/// an emulator. That is the right thing to run and the wrong thing to need
/// when only one scenario's hour is missing -- as happened when `death`
/// joined the corpus after the emulator hour had already been measured for
/// the other 17. Without this the only way to close that gap is to re-run
/// the 17 that already passed.
///
/// `option_env!` resolves at COMPILE time, so the filter cannot be changed
/// without a rebuild, and an unset variable leaves the full soak exactly as
/// it was. Unknown names are reported rather than silently running nothing.
#[cfg(feature = "soak")]
const SOAK_ONLY: Option<&str> = option_env!("KAIROS_SOAK_ONLY");

use rusty_rtos_demo_core::runner::{Runner, Shared};
use rusty_rtos_demo_core::step_limit_for;

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos conformance corpus on Cortex-M3 (mps2-an385, QEMU) ===");
    hprintln!("target  thumbv7m-none-eabi, no_std, NO alloc, no per-task stack");
    hprintln!("check   every counter and the FNV-1a/64 trace digest against the");
    hprintln!("        host's pins, which are pinned against the C kernel's trace");
    hprintln!();

    let mut failed = 0u32;
    #[cfg(feature = "soak")]
    let mut matched = 0u32;
    let table = pins::<Digest>();

    for pin in &table {
        // Skip whatever `KAIROS_SOAK_ONLY` did not name, if it named
        // anything. `matched` below turns a typo into a FAIL rather than a
        // vacuous pass.
        #[cfg(feature = "soak")]
        if let Some(only) = SOAK_ONLY {
            if pin.name != only {
                continue;
            }
            matched += 1;
        }

        let kernel = match Runner::kernel_for(Digest::new()) {
            Ok(k) => k,
            Err(_) => {
                hprintln!("{:<22} FAIL  the sim geometry refused the scenario", pin.name);
                failed += 1;
                continue;
            }
        };
        let shared = core::cell::RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            // A scenario with its own floor gets it; the soak length is
            // longer than any floor, so it wins on its own.
            if (pin.start)(&mut runner, RUN_TICKS.max(pin.run_ticks)).is_err() {
                hprintln!("{:<22} FAIL  the scenario would not start", pin.name);
                failed += 1;
                continue;
            }
            runner.run(step_limit_for(RUN_TICKS.max(pin.run_ticks)))
        };
        let d = Runner::into_writer(kernel);

        // At the pinned length every counter and the digest are compared
        // against the C kernel's. At an hour there is no C pin to compare
        // against, so the check is liveness — plus `ticks >= RUN_TICKS`,
        // without which a scenario that ended at tick 5 would report `pass`
        // perfectly happily, having never been asked to survive anything.
        #[cfg(not(feature = "soak"))]
        let ok = verdict.pass
            && !verdict.runaway
            && verdict.ticks == pin.ticks
            && verdict.yields == pin.yields
            && verdict.exits == pin.exits
            && verdict.lines == pin.lines
            && d.hash() == pin.digest
            && d.bytes() == pin.bytes;
        #[cfg(feature = "soak")]
        let ok = verdict.pass && !verdict.runaway && verdict.ticks >= RUN_TICKS.max(pin.run_ticks);

        if ok {
            hprintln!(
                "{:<22} ok    ticks={} yields={} exits={} lines={} bytes={}",
                pin.name, verdict.ticks, verdict.yields, verdict.exits, verdict.lines, d.bytes()
            );
        } else {
            failed += 1;
            hprintln!("{:<22} FAIL", pin.name);
            #[cfg(feature = "soak")]
            hprintln!(
                "           pass={} runaway={} ticks {} of {}",
                verdict.pass,
                verdict.runaway,
                verdict.ticks,
                RUN_TICKS.max(pin.run_ticks)
            );
            #[cfg(not(feature = "soak"))]
            hprintln!("           ticks  {:>8} want {:>8}", verdict.ticks, pin.ticks);
            #[cfg(not(feature = "soak"))]
            hprintln!("           yields {:>8} want {:>8}", verdict.yields, pin.yields);
            #[cfg(not(feature = "soak"))]
            hprintln!("           exits  {:>8} want {:>8}   <- sim time itself", verdict.exits, pin.exits);
            #[cfg(not(feature = "soak"))]
            hprintln!("           lines  {:>8} want {:>8}", verdict.lines, pin.lines);
            #[cfg(not(feature = "soak"))]
            hprintln!("           bytes  {:>8} want {:>8}", d.bytes(), pin.bytes);
            #[cfg(not(feature = "soak"))]
            hprintln!("           digest {:#018x}", d.hash());
            #[cfg(not(feature = "soak"))]
            hprintln!("           want   {:#018x}", pin.digest);
        }
    }

    hprintln!();
    if failed == 0 {
        #[cfg(not(feature = "soak"))]
        {
            hprintln!("RESULT: PASS -- {} scenarios byte-identical to the C kernel", table.len());
            // NOT "each": `death` has a floor of its own, because at
            // the default length it deletes nothing at all.
            hprintln!("        on a Cortex-M3, at {} ticks or each pin's own floor.", RUN_TICKS);
        }
        #[cfg(feature = "soak")]
        {
            // A filter that matched nothing must not read as a pass: an
            // empty run trivially has zero failures, which is exactly the
            // shape of a gate that tests nothing.
            if SOAK_ONLY.is_some() && matched == 0 {
                hprintln!("RESULT: FAIL -- KAIROS_SOAK_ONLY named no scenario in the table");
                debug::exit(debug::EXIT_FAILURE);
            }
            let ran = if SOAK_ONLY.is_some() {
                matched as usize
            } else {
                table.len()
            };
            hprintln!("RESULT: PASS -- {} scenario(s) still running after an hour", ran);
            hprintln!("        of simulated time ({} ticks) on a Cortex-M3.", RUN_TICKS);
            hprintln!("        Liveness, not conformance: no C pin exists at this length.");
        }
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} of {} scenarios diverged", failed, table.len());
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}
