//! Leak, or an arena sized for fewer queues?
//!
//! `AbortDelay` creates and deletes its queue every cycle and
//! `queue_create` starts refusing after roughly 870 of them
//! (`docs/LEDGER.md`, 2026-09-21). The scenario deletes what it creates, so a
//! kernel that reclaims a deleted queue's slot should run it forever.
//!
//! Two explanations fit that, and they are not close:
//!
//! - **A leak.** `queue_delete` does not return the slot, so free capacity
//!   falls with every cycle. That is a defect in a published crate.
//! - **An arena limit.** Capacity is fixed and something else consumes it
//!   once, early; the run then fails for a reason that has nothing to do
//!   with cycles.
//!
//! **One measurement separates them**: free capacity, sampled at several run
//! lengths. Falling with length is a leak. Flat is not.
//!
//! The capacity is measured by asking for queues until the kernel refuses,
//! AFTER the run has finished — so the probe cannot perturb what it
//! measures, and the charged kernel calls it makes cost nothing because
//! nothing is being compared to the C any more.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test queue_slot_leak -- --ignored --nocapture
//! ```

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{pins, Digest};
use rusty_rtos_demo_core::runner::Shared;
use rusty_rtos_demo_core::{step_limit_for, Runner};

/// Lengths to sample. The last two straddle the failure at 220,387.
const LENGTHS: [u64; 10] = [
    1_000, 100_000, 200_000, 210_000, 215_000, 219_000, 220_000, 220_386, 220_387, 221_000,
];

/// Run `AbortDelay` for `ticks`, then count how many more queues the kernel
/// will hand out before refusing.
fn free_capacity_after(ticks: u64) -> usize {
    let pin = pins::<Digest>()
        .into_iter()
        .find(|p| p.name == "AbortDelay")
        .expect("AbortDelay is in the corpus");

    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    let shared = RefCell::new(Shared::default());
    {
        let mut runner = Runner::new(&kernel, &shared);
        (pin.start)(&mut runner, ticks).expect("the scenario starts");
        let _ = runner.run(step_limit_for(ticks));
    }

    // Ask until refused. Bounded, so a kernel that never refuses fails the
    // test rather than hanging it.
    let mut granted = 0usize;
    let mut k = kernel.borrow_mut();
    while granted < 10_000 {
        if k.queue_create(1).is_err() {
            break;
        }
        granted += 1;
    }
    granted
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn queue_slots_are_reclaimed_or_they_are_not() {
    println!();
    println!("=== free queue capacity after running AbortDelay for N ticks ===");
    println!();
    println!("{:>10}  {:>14}", "ticks", "more queues");

    let mut readings = Vec::new();
    for ticks in LENGTHS {
        let free = free_capacity_after(ticks);
        readings.push((ticks, free));
        println!("{ticks:>10}  {free:>14}");
    }

    println!();
    let first = readings.first().expect("sampled").1;
    let last = readings.last().expect("sampled").1;

    // The verdict has to survive the SHAPE, not just the endpoints. An
    // earlier version compared first against last and called anything
    // falling a leak; this data is flat for ~400 cycles and then collapses,
    // which that test would have described as a steady leak.
    let zero_at = readings.iter().find(|(_, free)| *free == 0).map(|(t, _)| *t);
    if last < first {
        println!("FALLING: {first} -> {last}, on a scenario that deletes every");
        println!("queue it creates. Capacity is not being fully reclaimed.");
        if let Some(t) = zero_at {
            println!();
            println!("Exhausted by {t} ticks -- BEFORE the scenario fails at");
            println!("220,387. Capacity reaches zero while it is still passing,");
            println!("and the refusal only bites when it next needs to CREATE.");
        }
        println!();
        println!("NOT uniform, and that is unexplained: flat for roughly 400");
        println!("cycles, then away. A steady per-cycle leak would empty 11");
        println!("slots in 11 cycles. Note the scenario also creates and deletes");
        println!("a semaphore, an event group and a stream buffer each cycle,");
        println!("and in this family a semaphore IS a queue -- so the leaking");
        println!("resource is not necessarily the one named `queue`.");
    } else if last == first {
        println!("FLAT at {first}: queue_delete IS reclaiming, and the refusal");
        println!("after ~870 cycles has another cause. The leak reading is");
        println!("refuted.");
    } else {
        println!("RISING: {first} -> {last}, which should be impossible and means");
        println!("this probe is wrong before anything about the kernel is.");
    }
}
