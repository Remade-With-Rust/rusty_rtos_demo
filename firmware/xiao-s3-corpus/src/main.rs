#![no_std]
#![no_main]
//! The conformance corpus on **ESP32-S3 Xtensa — real silicon**.
//!
//! The Cortex-M3 and RV32 cells answered "the corpus passes on an
//! emulator". This one answers a different question, and the difference is
//! the whole point: **those two run under QEMU; this runs on a part.** Same
//! program, same table (`rusty_rtos_demo_core::pins`), so what it compares
//! is the architecture and the silicon, and nothing else.
//!
//! # Why this needs no Xtensa context switch
//!
//! For the same reason the other two cells needed no port at all: a
//! scenario is a **state machine** driven by `Runner`, one `step` per C
//! statement with a `pc`, so a task keeps its locals in the TCB and needs
//! no stack of its own. `rusty_rtos_demo-core` is `no_std` with **no
//! `alloc`** and builds for `xtensa-esp32s3-none-elf` unchanged.
//!
//! That is worth stating plainly because the K5a plan row assumed the
//! opposite — that Xtensa work had to start with a context switch. It does
//! not. A context switch is needed to run tasks that have their own stacks;
//! it is not needed to prove this kernel schedules identically to C
//! FreeRTOS on this part. The smallest port that supports a real workload
//! is, for the corpus, no port at all.
//!
//! # What this does and does not claim
//!
//! It claims **conformance**: every scenario checked against the C kernel's
//! ticks, yields, exits, line count, FNV-1a/64 trace digest and byte count.
//! Matching them means this Xtensa build produces a trace byte-identical to
//! C FreeRTOS's, on hardware.
//!
//! It claims **no timing**. There is no cycle count here on purpose. The
//! ESP32-S3 has no `mcycle`-shaped counter this cell reads, the numbers
//! that matter for K3 are a Kairos part's, and a wall-clock figure taken
//! through a JTAG-serial `println` would measure the printing. `exits` is
//! the only clock here, and it is *simulated* time.
//!
//! # `exits` is the one to watch
//!
//! It is sim time itself — the count of outermost critical-section exits —
//! so a target that changed *when* the scheduler ran would move it long
//! before it moved a digest.

use esp_backtrace as _;
use esp_println::println;

use rusty_rtos_demo_core::pins::{pins, Digest};
use rusty_rtos_demo_core::runner::{Runner, Shared};
use rusty_rtos_demo_core::step_limit_for;

// esp-hal 1.2 images must carry an ESP-IDF app descriptor or the
// second-stage bootloader refuses them.
esp_bootloader_esp_idf::esp_app_desc!();

/// How long each scenario runs, and therefore what is checked.
///
/// The default is the pinned length, where the C kernel's trace exists and
/// every counter and the digest are compared against it. `--features soak`
/// runs an hour of simulated time instead — 3,600,000 ticks at the oracle's
/// own `TICK_RATE_HZ` of 1000 — where no C pin exists, so the check becomes
/// the one the C demo itself makes: is every scenario's own check task
/// still reporting that it is running?
#[cfg(not(feature = "soak"))]
const RUN_TICKS: u64 = rusty_rtos_demo_core::pins::PIN_TICKS;
#[cfg(feature = "soak")]
const RUN_TICKS: u64 = soak_ticks();

/// Run only ONE scenario of the soak, named at compile time:
///
/// ```sh
/// KAIROS_SOAK_ONLY=death cargo build --release --features soak
/// ```
///
/// The full soak is all 18 scenarios at 3,600,000 ticks. On silicon that is
/// hours, and `semtest` alone is 78% of it -- it runs 345 state-machine
/// steps per unit of sim time against a corpus average of 6.3. When only
/// one scenario's hour is missing, re-running the other seventeen to reach
/// it is waste.
///
/// `option_env!` resolves at COMPILE time, so an unset variable leaves the
/// full soak exactly as it was. Unknown names FAIL rather than silently
/// running nothing.
#[cfg(feature = "soak")]
const SOAK_ONLY: Option<&str> = option_env!("KAIROS_SOAK_ONLY");

/// Override the soak length, set at compile time. A length that can be
/// moved turns "it did not survive" into a bisection.
#[cfg(feature = "soak")]
const SOAK_TICKS: Option<&str> = option_env!("KAIROS_SOAK_TICKS");

#[cfg(feature = "soak")]
const fn soak_ticks() -> u64 {
    match SOAK_TICKS {
        // `parse` is not const. A malformed value saturates to 0 so the run
        // reports a scenario that never reached its length, rather than
        // silently falling back to the hour and hiding the typo.
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


#[esp_hal::main]
fn main() -> ! {
    let _p = esp_hal::init(esp_hal::Config::default());

    println!();
    println!("=== the Kairos conformance corpus on ESP32-S3 (Xtensa, SILICON) ===");
    println!("target  xtensa-esp32s3-none-elf, no_std, NO alloc, no per-task stack");
    println!("check   every counter and the FNV-1a/64 trace digest against the");
    println!("        host's pins, which are pinned against the C kernel's trace");
    println!();

    let mut failed = 0u32;
    #[cfg(feature = "soak")]
    let mut matched = 0u32;
    let table = pins::<Digest>();

    for pin in &table {
        // Skip whatever `KAIROS_SOAK_ONLY` did not name, if it named
        // anything. `matched` turns a typo into a FAIL below.
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
                println!(
                    "{:<22} FAIL  the sim geometry refused the scenario",
                    pin.name
                );
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
                println!("{:<22} FAIL  the scenario would not start", pin.name);
                failed += 1;
                continue;
            }
            runner.run(step_limit_for(RUN_TICKS.max(pin.run_ticks)))
        };
        let d = Runner::into_writer(kernel);

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
            println!(
                "{:<22} ok    ticks={} yields={} exits={} lines={} bytes={}",
                pin.name,
                verdict.ticks,
                verdict.yields,
                verdict.exits,
                verdict.lines,
                d.bytes()
            );
        } else {
            failed += 1;
            println!("{:<22} FAIL", pin.name);
            #[cfg(feature = "soak")]
            println!(
                "           pass={} runaway={} ticks {} of {}",
                verdict.pass,
                verdict.runaway,
                verdict.ticks,
                RUN_TICKS.max(pin.run_ticks)
            );
            #[cfg(not(feature = "soak"))]
            println!(
                "           ticks  {:>8} want {:>8}",
                verdict.ticks, pin.ticks
            );
            #[cfg(not(feature = "soak"))]
            println!(
                "           yields {:>8} want {:>8}",
                verdict.yields, pin.yields
            );
            #[cfg(not(feature = "soak"))]
            println!(
                "           exits  {:>8} want {:>8}   <- sim time itself",
                verdict.exits, pin.exits
            );
            #[cfg(not(feature = "soak"))]
            println!(
                "           lines  {:>8} want {:>8}",
                verdict.lines, pin.lines
            );
            #[cfg(not(feature = "soak"))]
            println!("           digest {:#018x}", d.hash());
            #[cfg(not(feature = "soak"))]
            println!("           want   {:#018x}", pin.digest);
        }
    }

    println!();
    if failed == 0 {
        #[cfg(not(feature = "soak"))]
        {
            println!(
                "RESULT: PASS -- {} scenarios byte-identical to the C kernel",
                table.len()
            );
            println!(
                "        on ESP32-S3 SILICON, at {} ticks or each pin's own floor.",
                RUN_TICKS
            );
        }
        #[cfg(feature = "soak")]
        {
            // A filter that matched nothing must not read as a pass: an
            // empty run trivially has zero failures, which is exactly the
            // shape of a gate that tests nothing.
            if SOAK_ONLY.is_some() && matched == 0 {
                println!("RESULT: FAIL -- KAIROS_SOAK_ONLY named no scenario in the table");
                loop {
                    core::hint::spin_loop();
                }
            }
            let ran = if SOAK_ONLY.is_some() {
                matched as usize
            } else {
                table.len()
            };
            println!(
                "RESULT: PASS -- {} scenario(s) still running after an hour",
                ran
            );
            println!(
                "        of simulated time ({} ticks) on ESP32-S3 silicon.",
                RUN_TICKS
            );
        }
    } else {
        println!("RESULT: FAIL -- {failed} scenario(s) disagreed");
    }

    // A board has no exit code to hand back, so the verdict is the line
    // above and a person (or `kairos`) reads it. That is the one thing this
    // cell cannot do that the QEMU pair can, and it is why those two stay.
    loop {
        core::hint::spin_loop();
    }
}
