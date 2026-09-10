//! K3's one-hour clause: the whole corpus, for an hour of simulated time.
//!
//! The FreeRTOS standard demo is validated by its **check task**: each
//! scenario keeps a "still running" flag that its own checker reads, and
//! the demo is judged by whether that flag still says yes after a long
//! run. That is what an hour buys and a 2000-tick run does not — a
//! scenario can be byte-identical to C for 2000 ticks and still deadlock,
//! starve or leak a list entry at tick 300,000.
//!
//! # An hour is a real hour
//!
//! The sim runs `PosixDemoConfig`, whose `TICK_RATE_HZ` is 1000 — the same
//! value as the oracle's `FreeRTOSConfig.h`, which is why the corpus can be
//! byte-identical to C in the first place. So one tick is one millisecond
//! and **an hour is 3,600,000 ticks**, not a round number chosen to be
//! quick.
//!
//! # Why this checks something the pins cannot
//!
//! The pinned corpus (`tests/conformance.rs`) compares against the C
//! kernel's trace at 2000 ticks, because that is where the C oracle was
//! traced. There are no C pins at 3,600,000 ticks and getting them would
//! mean an hour-long instrumented C run per scenario. So this test does
//! what the C demo itself does — it asks each scenario's own checker — and
//! claims exactly that and no more. It is a **liveness** test, not a
//! conformance one.
//!
//! # Where the two minutes go, since the answer is not what it looks like
//!
//! `semtest` takes **92 of the ~125 seconds** and every other scenario
//! takes between 0.3 and 4.1 — despite semtest doing FEWER yields and
//! fewer critical-section exits than `GenQTest`, which finishes in 4.1.
//! A counter settles what a story would not: `Verdict::steps`.
//!
//! | scenario | steps per exit | ns per step |
//! |---|---:|---:|
//! | `semtest` | **345.6** | 6.3 |
//! | `GenQTest` | 1.0 | 68.4 |
//! | `countsem` | 1.5 | 35.2 |
//! | `dynamic` | 1.7 | 45.6 |
//!
//! semtest runs 345 state-machine steps per unit of sim time where the
//! others run one or two, its steps are individually *cheap*, and the
//! ratio is 345.6 at 200,000 ticks and 345.6 at 400,000 — constant, so
//! structural rather than a leak. That is `semtest.c`'s polling pair,
//! which takes its semaphore with a **zero block time**: the same shape
//! the ledger records for `dynamic`'s SUSP_RX, and the reason the sim
//! contract counts critical-section exits rather than yields in the first
//! place. It is faithful to C, so it is a cost and not a defect.
//!
//! `#[ignore]`d because it is ~2 minutes: run it with
//! `cargo test -p rusty_rtos_demo-core --test soak --release -- --ignored
//! --nocapture`, or through `kairos check rusty_rtos_demo --soak`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{Digest, pins};
use rusty_rtos_demo_core::runner::Shared;
use rusty_rtos_demo_core::{Runner, step_limit_for};

/// One hour at `PosixDemoConfig::TICK_RATE_HZ` (1000 Hz, as the oracle).
const ONE_HOUR_TICKS: u64 = 3_600_000;

#[test]
#[ignore = "two minutes: K3's one-hour soak, run it with --ignored"]
fn the_whole_corpus_survives_an_hour() {
    let mut failures = Vec::new();
    println!();
    println!("=== the corpus for one hour of simulated time ({ONE_HOUR_TICKS} ticks) ===");

    for pin in &pins::<Digest>() {
        let began = std::time::Instant::now();
        let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
        let shared = RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            (pin.start)(&mut runner, ONE_HOUR_TICKS).expect("the scenario starts");
            runner.run(step_limit_for(ONE_HOUR_TICKS))
        };

        // Three checks, and the third is the one that stops this being
        // theatre: a scenario that ended at tick 5 would report `pass`
        // perfectly happily, having never been asked to survive anything.
        let mut why = Vec::new();
        if !verdict.pass {
            why.push("the scenario's own check task says it is no longer running".to_owned());
        }
        if verdict.runaway {
            why.push(format!(
                "ran away: the step limit stopped it at tick {}",
                verdict.ticks
            ));
        }
        if verdict.ticks < ONE_HOUR_TICKS {
            why.push(format!(
                "stopped early at tick {} of {ONE_HOUR_TICKS}",
                verdict.ticks
            ));
        }

        if why.is_empty() {
            println!(
                "  {:<22} ok    ticks={} yields={} exits={} in {:.1}s",
                pin.name,
                verdict.ticks,
                verdict.yields,
                verdict.exits,
                began.elapsed().as_secs_f64()
            );
        } else {
            println!("  {:<22} FAIL  {}", pin.name, why.join("; "));
            failures.push(format!("{}: {}", pin.name, why.join("; ")));
        }
    }

    println!();
    assert!(
        failures.is_empty(),
        "{} of {} scenarios did not survive an hour:\n  {}",
        failures.len(),
        pins::<Digest>().len(),
        failures.join("\n  ")
    );
    println!("RESULT: PASS -- every scenario still running after an hour");
}
