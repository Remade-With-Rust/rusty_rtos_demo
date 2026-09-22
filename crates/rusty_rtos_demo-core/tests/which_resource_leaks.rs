//! Which of the four create/delete pairs does not return its slot.
//!
//! `AbortDelay` exhausts queue capacity over ~870 cycles
//! (`docs/LEDGER.md`, 2026-09-21) while deleting everything it creates. It
//! creates **four** kinds of object each cycle — a binary semaphore, an
//! event group, a queue and a stream buffer — and in this family, as in
//! FreeRTOS, a semaphore IS a queue, so all four draw on the capacity the
//! leak probe measures.
//!
//! Inferring the culprit from a scenario that exercises all four at once is
//! how a plausible answer gets recorded as a fact. This asks each pair on
//! its own, on a fresh kernel, with nothing else running:
//!
//! > create and delete the SAME object N times, then count how many queues
//! > the kernel will still hand out.
//!
//! A pair that reclaims leaves capacity where it started, whatever N is. A
//! pair that leaks drops it by one per iteration.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test which_resource_leaks -- --ignored --nocapture
//! ```


use rusty_rtos_demo_core::pins::Digest;
use rusty_rtos_demo_core::Runner;

/// How many create/delete rounds per pair. Larger than the capacity, so a
/// leak of even one slot per round is unmissable.
const ROUNDS: usize = 40;

/// How many queues the kernel will still hand out. Bounded so a kernel that
/// never refuses fails rather than hangs.
fn free_capacity(k: &mut rusty_rtos_demo_core::runner::SimKernel<Digest>) -> usize {
    let mut granted = 0usize;
    while granted < 10_000 {
        if k.queue_create(1).is_err() {
            break;
        }
        granted += 1;
    }
    granted
}

/// Baseline capacity, and capacity after `rounds` of the given pair.
fn capacity_after(rounds: usize, mut pair: impl FnMut(&mut rusty_rtos_demo_core::runner::SimKernel<Digest>)) -> (usize, usize) {
    let baseline = {
        let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
        let mut k = kernel.borrow_mut();
        free_capacity(&mut k)
    };

    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    let after = {
        let mut k = kernel.borrow_mut();
        for _ in 0..rounds {
            pair(&mut k);
        }
        free_capacity(&mut k)
    };

    (baseline, after)
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn which_create_delete_pair_does_not_reclaim() {
    println!();
    println!("=== capacity after {ROUNDS} create/delete rounds, one pair at a time ===");
    println!();
    println!("{:<26}  {:>8}  {:>8}  {:>7}", "pair", "before", "after", "lost");

    let mut leakers = Vec::new();

    let cases: [(&str, fn(&mut rusty_rtos_demo_core::runner::SimKernel<Digest>)); 4] = [
        ("queue / queue_delete", |k| {
            if let Ok(q) = k.queue_create(1) {
                let _ = k.queue_delete(q);
            }
        }),
        ("binary sem / queue_delete", |k| {
            if let Ok(s) = k.semaphore_create_binary() {
                let _ = k.queue_delete(s);
            }
        }),
        ("event group / delete", |k| {
            if let Ok(g) = k.event_group_create() {
                let _ = k.event_group_delete(g);
            }
        }),
        ("stream buffer / delete", |k| {
            if let Ok(b) = k.stream_buffer_create(1, 1) {
                let _ = k.stream_buffer_delete(b);
            }
        }),
    ];

    for (name, pair) in cases {
        let (before, after) = capacity_after(ROUNDS, pair);
        let lost = before.saturating_sub(after);
        println!("{name:<26}  {before:>8}  {after:>8}  {lost:>7}");
        if lost > 0 {
            leakers.push((name, lost));
        }
    }

    println!();
    if leakers.is_empty() {
        println!("None of the four loses capacity on its own over {ROUNDS} rounds.");
        println!("So the exhaustion AbortDelay hits is not a single pair failing");
        println!("to reclaim, and the next suspect is an interaction between");
        println!("them -- or a resource this probe does not count.");
    } else {
        for (name, lost) in &leakers {
            println!("LEAKS: {name} lost {lost} of {ROUNDS} rounds' worth.");
        }
        println!();
        println!("Read the ratio, not just the sign: one slot lost per round is a");
        println!("slot never returned, while a handful over {ROUNDS} rounds is");
        println!("something rarer and the arithmetic says which.");
    }
}
