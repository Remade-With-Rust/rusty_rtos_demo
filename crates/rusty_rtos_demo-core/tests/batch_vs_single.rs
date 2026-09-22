//! Create-one-delete-one reclaims. Does create-many-delete-many?
//!
//! The observational probe (`capacity_over_time.rs`) accelerated
//! `AbortDelay`'s exhaustion twelve-fold — 11 slots gone in 9 passes where
//! the unperturbed run takes 108. The only thing it added was its own
//! sampling, and that sampling differs from every earlier probe in one way:
//!
//! - `which_resource_leaks.rs` creates **one** object and deletes it, forty
//!   times. Capacity holds.
//! - the sampler creates queues **until the kernel refuses**, then gives all
//!   of them back. Capacity falls.
//!
//! So the ninth hypothesis is not about which object or which surface. It is
//! that reclamation depends on **how many are outstanding**, or on the order
//! they are returned in — and the eight refuted hypotheses all missed it
//! because every one of them held at most one object at a time.
//!
//! Three shapes, same total number of creates:
//!
//! 1. **single**: create one, delete it. Repeat.
//! 2. **batch, same order**: create N, delete them oldest-first.
//! 3. **batch, reverse order**: create N, delete them newest-first.
//!
//! If (1) holds and (2) or (3) falls, the defect is named and it is a kernel
//! one.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test batch_vs_single -- --ignored --nocapture
//! ```

use rusty_rtos_demo_core::pins::Digest;
use rusty_rtos_demo_core::runner::SimKernel;
use rusty_rtos_demo_core::Runner;

/// How many batches, or equivalent singles.
const ROUNDS: usize = 10;

fn fresh() -> core::cell::RefCell<SimKernel<Digest>> {
    Runner::kernel_for(Digest::new()).expect("the sim geometry holds")
}


/// Capacity of an untouched kernel, and of one that has seen `ROUNDS` of
/// the shape.
///
/// Two FRESH kernels, and each is measured by creating until refusal and
/// NOT giving anything back. The first version of this measured its
/// baseline with a create-until-refusal-then-delete-all -- which is the
/// batch operation under test, so the baseline could have been leaking and
/// the comparison would have been meaningless.
fn run(shape: impl Fn(&mut SimKernel<Digest>)) -> (usize, usize) {
    fn drain(k: &mut SimKernel<Digest>) -> usize {
        let mut n = 0;
        while n < 64 && k.queue_create(1).is_ok() {
            n += 1;
        }
        n
    }

    let baseline = fresh();
    let before = drain(&mut baseline.borrow_mut());

    let worked = fresh();
    let after = {
        let mut k = worked.borrow_mut();
        for _ in 0..ROUNDS {
            shape(&mut k);
        }
        drain(&mut k)
    };

    (before, after)
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn does_holding_several_at_once_lose_a_slot() {
    println!();
    println!("=== {ROUNDS} rounds of each shape ===");
    println!();
    println!("{:<34}  {:>7}  {:>7}  {:>6}", "shape", "before", "after", "lost");

    // 1. one at a time, ten times the batch size so the totals are fair.
    let (b1, a1) = run(|k| {
        for _ in 0..10 {
            if let Ok(q) = k.queue_create(1) {
                let _ = k.queue_delete(q);
            }
        }
    });
    println!("{:<34}  {b1:>7}  {a1:>7}  {:>6}", "single: create 1, delete 1", b1.saturating_sub(a1));

    // 2. fill, then drain oldest-first.
    let (b2, a2) = run(|k| {
        let mut held = Vec::new();
        while held.len() < 64 {
            match k.queue_create(1) {
                Ok(q) => held.push(q),
                Err(_) => break,
            }
        }
        for q in held {
            let _ = k.queue_delete(q);
        }
    });
    println!("{:<34}  {b2:>7}  {a2:>7}  {:>6}", "batch: fill, drain oldest-first", b2.saturating_sub(a2));

    // 3. fill, then drain newest-first.
    let (b3, a3) = run(|k| {
        let mut held = Vec::new();
        while held.len() < 64 {
            match k.queue_create(1) {
                Ok(q) => held.push(q),
                Err(_) => break,
            }
        }
        while let Some(q) = held.pop() {
            let _ = k.queue_delete(q);
        }
    });
    println!("{:<34}  {b3:>7}  {a3:>7}  {:>6}", "batch: fill, drain newest-first", b3.saturating_sub(a3));

    // 4. the fragmenting shape: create three, delete the MIDDLE one.
    //
    // The end-of-arena-only fix cannot reclaim this -- the freed extent is
    // not the last allocation -- so it is what distinguishes a partial fix
    // from a complete one.
    let (b4, a4) = run(|k| {
        let mut held = Vec::new();
        for _ in 0..3 {
            if let Ok(q) = k.queue_create(1) {
                held.push(q);
            }
        }
        if held.len() == 3 {
            let _ = k.queue_delete(held[1]);
            let _ = k.queue_delete(held[0]);
            let _ = k.queue_delete(held[2]);
        }
    });
    println!("{:<34}  {b4:>7}  {a4:>7}  {:>6}", "fragmenting: delete middle first", b4.saturating_sub(a4));

    println!();
    let single = b1.saturating_sub(a1);
    let oldest = b2.saturating_sub(a2);
    let newest = b3.saturating_sub(a3);

    let frag = b4.saturating_sub(a4);
    if frag > 0 {
        println!("STRANDED: the fragmenting shape lost {frag} of {ROUNDS} rounds.");
        println!("An extent freed out of order is not the last allocation, so an");
        println!("end-of-arena-only fix cannot take it back. That is the known");
        println!("limit, and it is measured rather than assumed.");
    } else if single == 0 && oldest == 0 && newest == 0 {
        println!("ALL FOUR hold, including the fragmenting shape -- so an extent");
        println!("freed out of order is reclaimed too, and the allocator is");
        println!("complete rather than only covering create-then-delete.");
    } else if single == 0 && (oldest > 0 || newest > 0) {
        println!("NAMED: one at a time reclaims; holding several at once does not.");
        println!("oldest-first lost {oldest}, newest-first lost {newest}, over {ROUNDS} rounds.");
        println!();
        println!("That is a kernel defect and it is nothing to do with which");
        println!("object or which surface -- which is why eight hypotheses that");
        println!("each held at most ONE object missed it.");
    } else if single == 0 && oldest == 0 && newest == 0 {
        println!("All three hold. The acceleration the sampler caused is not");
        println!("explained by batching, and the ninth hypothesis dies too.");
    } else {
        println!("single lost {single}, which the earlier probe said it does not.");
        println!("Settle that contradiction before reading the batch rows.");
    }
}
