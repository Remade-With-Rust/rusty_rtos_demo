//! WHEN does `AbortDelay` lose each slot?
//!
//! Eight hypotheses about the mechanism have been proposed and refuted
//! (`docs/LEDGER.md`, 2026-09-21). Every one guessed at *what* the scenario
//! does wrong; every isolated reproduction reclaimed perfectly. So this
//! stops reproducing and observes instead.
//!
//! The scenario is driven a step at a time and free capacity is sampled as
//! it runs, which turns "capacity falls between 100,000 and 219,000 ticks"
//! into a list of the exact moments a slot goes missing. A loss that happens
//! at one point in the scenario's cycle looks completely different from one
//! spread evenly, and neither is visible from end-of-run totals.
//!
//! # The sample is non-destructive
//!
//! Capacity is measured by asking for queues until the kernel refuses — and
//! then **giving them all back**. That is only sound because `queue_delete`
//! has been shown to reclaim in isolation, which is one of the eight
//! refutations being useful rather than merely negative.
//!
//! The sampling does perturb the run: those are charged kernel calls and
//! they move sim time. For a diagnostic that is the right trade, and it is
//! stated rather than hidden — the tick numbers here will not line up with
//! the unperturbed run's 220,387.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test capacity_over_time -- --ignored --nocapture
//! ```

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{pins, Digest};
use rusty_rtos_demo_core::runner::{Shared, SimKernel, State, Step};
use rusty_rtos_demo_core::Runner;

/// Sample this often, in ticks.
const EVERY: u64 = 2_000;
/// Stop here. Past the point capacity reaches zero in the unperturbed run.
const UNTIL: u64 = 240_000;

/// Free capacity, restored afterwards. Returns what was free.
fn sample(k: &mut SimKernel<Digest>) -> usize {
    // A test runs with std, so a Vec is fine here even though the crate
    // under test is no_std.
    let mut held = Vec::new();
    while held.len() < 64 {
        match k.queue_create(1) {
            Ok(q) => held.push(q),
            Err(_) => break,
        }
    }
    let n = held.len();
    for q in held {
        let _ = k.queue_delete(q);
    }
    n
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn when_is_each_slot_lost() {
    let pin = pins::<Digest>()
        .into_iter()
        .find(|p| p.name == "AbortDelay")
        .expect("AbortDelay is in the corpus");

    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    let shared = RefCell::new(Shared::default());
    let mut runner = Runner::new(&kernel, &shared);
    (pin.start)(&mut runner, UNTIL).expect("the scenario starts");

    println!();
    println!("=== free capacity as AbortDelay runs (sampled every {EVERY} ticks) ===");
    println!();
    println!("{:>9}  {:>9}  {:>6}  {:>8}", "tick", "capacity", "drop", "passes");

    let mut last = usize::MAX;
    let mut next_sample = 0u64;
    let mut steps = 0u64;

    loop {
        steps += 1;
        if steps > 80_000_000 {
            println!("step limit reached");
            break;
        }
        if matches!(runner.step_once(), Step::Finish(_)) {
            println!("the scenario finished");
            break;
        }

        let tick = kernel.borrow().tick_count();
        if tick < next_sample {
            continue;
        }
        next_sample = tick + EVERY;

        let free = {
            let mut k = kernel.borrow_mut();
            sample(&mut k)
        };
        let passes = {
            let b = shared.borrow();
            match &b.state {
                State::AbortDelay(s) => s.queue_creates,
                _ => 0,
            }
        };

        if free != last {
            let drop = last.saturating_sub(free);
            println!("{tick:>9}  {free:>9}  {drop:>6}  {passes:>8}");
            last = free;
        }

        if free == 0 || tick >= UNTIL {
            println!();
            println!("stopped at tick {tick}, capacity {free}, after {passes} passes");
            break;
        }
    }

    println!();
    println!("Read the `drop` column: a slot lost one at a time, evenly, is a");
    println!("leak per pass. Several at once is something structural happening");
    println!("at one point in the cycle. Nothing until late is a third thing");
    println!("again -- and none of the three is visible from end-of-run totals,");
    println!("which is all the eight refuted hypotheses ever had to work with.");
}
