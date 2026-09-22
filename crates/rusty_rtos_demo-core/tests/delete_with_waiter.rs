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
//! This asks it directly across SIX shapes and all four surfaces the
//! scenario uses. On the queue, because "with a waiter" is not one thing:
//!
//! 1. block a task on a queue, then **delete** the queue;
//! 2. block a task, **abort** the block, then delete;
//! 3. block a task, **let it time out**, then delete — the control, which
//!    should behave exactly like the isolated probe.
//!
//! and then the same block-abort-delete shape on the semaphore, the event
//! group and the stream buffer.
//!
//! RESULT: all six hold. The hypothesis died like the five before it, and
//! the three extra surfaces killed the follow-up that named them.
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

/// The same shape for a surface that is not a queue: create it, park the
/// current task on it, **abort** that block, then delete — and report
/// capacity before and after.
///
/// Abort rather than time out, because abort is the shape the scenario uses
/// and the one the queue rows found hardest to break.
fn surface_with_waiter<H: Copy>(
    _name: &str,
    mut create: impl FnMut(&mut SimKernel<Digest>) -> Option<H>,
    mut block: impl FnMut(&mut SimKernel<Digest>, H),
    mut delete: impl FnMut(&mut SimKernel<Digest>, H),
) -> (usize, usize) {
    let before = {
        let kernel = kernel_with_a_task();
        let mut k = kernel.borrow_mut();
        free_capacity(&mut k)
    };

    let kernel = kernel_with_a_task();
    let after = {
        let mut k = kernel.borrow_mut();
        for _ in 0..ROUNDS {
            let Some(h) = create(&mut k) else { break };
            block(&mut k, h);
            let current = k.current();
            let _ = k.abort_delay(current);
            delete(&mut k, h);
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

    // ---- the other three surfaces ----
    //
    // The queue rows above were the first version of this test. The
    // scenario also blocks and aborts on a semaphore, an event group and a
    // stream buffer, and those were only ever tested for plain
    // create/delete -- never with a waiter parked on them.
    let sem = surface_with_waiter(
        "semaphore",
        |k| k.semaphore_create_binary().ok(),
        |k, h| {
            let _ = k.semaphore_take(h, 100);
        },
        |k, h| {
            let _ = k.queue_delete(h);
        },
    );
    println!("{:<34}  {:>7}  {:>7}  {:>6}", "semaphore: block -> abort -> del", sem.0, sem.1, sem.0.saturating_sub(sem.1));

    let grp = surface_with_waiter(
        "event group",
        |k| k.event_group_create().ok(),
        |k, h| {
            let _ = k.event_group_wait_bits(h, 0x01, true, false, 100);
        },
        |k, h| {
            let _ = k.event_group_delete(h);
        },
    );
    println!("{:<34}  {:>7}  {:>7}  {:>6}", "event group: block -> abort -> del", grp.0, grp.1, grp.0.saturating_sub(grp.1));

    let stream = surface_with_waiter(
        "stream buffer",
        |k| k.stream_buffer_create(1, 1).ok(),
        |k, h| {
            let mut buf = [0u8; 1];
            let _ = k.stream_buffer_receive(h, &mut buf, 100);
        },
        |k, h| {
            let _ = k.stream_buffer_delete(h);
        },
    );
    println!("{:<34}  {:>7}  {:>7}  {:>6}", "stream buffer: block -> abort -> del", stream.0, stream.1, stream.0.saturating_sub(stream.1));

    println!();
    let lost = [
        ("semaphore", sem.0.saturating_sub(sem.1)),
        ("event group", grp.0.saturating_sub(grp.1)),
        ("stream buffer", stream.0.saturating_sub(stream.1)),
        ("block -> delete", b1.saturating_sub(a1)),
        ("block -> abort -> delete", b2.saturating_sub(a2)),
        ("block -> time out -> delete", b3.saturating_sub(a3)),
    ];
    let guilty: Vec<_> = lost.iter().filter(|(_, l)| *l > 0).collect();

    if guilty.is_empty() {
        println!("ALL SIX hold their capacity, across all four surfaces the");
        println!("scenario uses. A queued waiter does not stop a slot being");
        println!("returned, and neither does aborting it.");
        println!();
        println!("So every isolated shape reclaims while the scenario exhausts.");
        println!("That is now the finding: the difference is something these");
        println!("probes do not reproduce, and eight guesses at what have each");
        println!("been wrong. The next person should look for what the RUNNER");
        println!("does that a hand-driven kernel does not, rather than for");
        println!("another create/delete pair to blame.");
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
