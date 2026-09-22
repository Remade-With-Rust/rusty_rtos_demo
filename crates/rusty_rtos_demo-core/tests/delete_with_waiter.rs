//! Does deleting an object with a task blocked on it return its slot?
//!
//! Five mechanisms for `AbortDelay`'s capacity exhaustion have been proposed
//! and refuted (`docs/LEDGER.md`, 2026-09-21), the last being a per-pair
//! reclaim failure — every create/delete pair reclaims perfectly in
//! isolation, 12 of 12 over forty rounds.
//!
//! What the isolated probe never did is **queue a waiter**. `AbortDelay`
//! blocks a task on each object and then aborts that block; that is the
//! whole subject of the scenario. So the sixth hypothesis is that deletion —
//! or abort — with a waiter queued does not reclaim.
//!
//! This asks it directly, and it asks three shapes, because "with a waiter"
//! is not one thing:
//!
//! 1. block a task on a queue, then **delete** the queue;
//! 2. block a task, **abort** the block, then delete;
//! 3. block a task, **let it time out**, then delete — the control, which
//!    should behave exactly like the isolated probe.
//!
//! If (3) holds capacity and (1) or (2) loses it, the defect is named. If
//! all three hold, the hypothesis dies like the five before it.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test delete_with_waiter -- --ignored --nocapture
//! ```

use rusty_rtos_demo_core::pins::Digest;
use rusty_rtos_demo_core::runner::SimKernel;
use rusty_rtos_demo_core::Runner;

/// Rounds per shape. Three times the arena, so one slot lost per round is
/// unmissable and a rarer loss still shows.
const ROUNDS: usize = 40;

/// How many queues the kernel will still hand out.
fn free_capacity(k: &mut SimKernel<Digest>) -> usize {
    let mut granted = 0usize;
    while granted < 10_000 {
        if k.queue_create(1).is_err() {
            break;
        }
        granted += 1;
    }
    granted
}

/// Build a kernel with one task running, so there is something to block.
fn kernel_with_a_task() -> core::cell::RefCell<SimKernel<Digest>> {
    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    {
        let mut k = kernel.borrow_mut();
        k.create_task("waiter", 2).expect("the task arena holds one");
        k.start_scheduler().expect("the scheduler starts");
    }
    kernel
}

/// `rounds` of: create a queue, block the current task on it, do `after`,
/// then delete. Returns capacity before and after.
fn capacity_after(rounds: usize, mut after_block: impl FnMut(&mut SimKernel<Digest>)) -> (usize, usize) {
    let before = {
        let kernel = kernel_with_a_task();
        let mut k = kernel.borrow_mut();
        free_capacity(&mut k)
    };

    let kernel = kernel_with_a_task();
    let after = {
        let mut k = kernel.borrow_mut();
        for _ in 0..rounds {
            let Ok(q) = k.queue_create(1) else { break };
            // Empty queue, so a receive with a timeout parks the caller.
            let _ = k.queue_receive(q, 100);
            after_block(&mut k);
            let _ = k.queue_delete(q);
        }
        free_capacity(&mut k)
    };

    (before, after)
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn deleting_an_object_with_a_waiter_reclaims_or_does_not() {
    println!();
    println!("=== capacity after {ROUNDS} rounds, with a task blocked on the queue ===");
    println!();
    println!("{:<34}  {:>7}  {:>7}  {:>6}", "shape", "before", "after", "lost");

    // 1. block, then delete.
    let (b1, a1) = capacity_after(ROUNDS, |_| {});
    println!("{:<34}  {b1:>7}  {a1:>7}  {:>6}", "block -> delete", b1.saturating_sub(a1));

    // 2. block, abort the block, then delete.
    let (b2, a2) = capacity_after(ROUNDS, |k| {
        let current = k.current();
        let _ = k.abort_delay(current);
    });
    println!("{:<34}  {b2:>7}  {a2:>7}  {:>6}", "block -> abort -> delete", b2.saturating_sub(a2));

    // 3. the control: block, let the wait elapse, then delete.
    let (b3, a3) = capacity_after(ROUNDS, |k| {
        for _ in 0..101 {
            let _ = k.increment_tick();
        }
    });
    println!("{:<34}  {b3:>7}  {a3:>7}  {:>6}", "block -> time out -> delete", b3.saturating_sub(a3));

    println!();
    let lost = [
        ("block -> delete", b1.saturating_sub(a1)),
        ("block -> abort -> delete", b2.saturating_sub(a2)),
        ("block -> time out -> delete", b3.saturating_sub(a3)),
    ];
    let guilty: Vec<_> = lost.iter().filter(|(_, l)| *l > 0).collect();

    if guilty.is_empty() {
        println!("All three hold their capacity. A queued waiter does NOT stop a");
        println!("slot being returned, so the sixth hypothesis dies with the five");
        println!("before it and AbortDelay's exhaustion is still unexplained.");
    } else {
        for (name, l) in guilty {
            println!("LOSES CAPACITY: {name} -- {l} slot(s) over {ROUNDS} rounds.");
        }
        println!();
        println!("Compare against the control (block -> time out -> delete): if it");
        println!("held and the others did not, the waiter is the difference and");
        println!("the defect has a name.");
    }
}
