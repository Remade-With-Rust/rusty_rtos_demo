//! Does the scenario delete every queue it creates?
//!
//! Six mechanisms for `AbortDelay`'s capacity exhaustion have been proposed
//! and refuted. The refutations share a shape: every one assumed the KERNEL
//! failed to return a slot, and every isolated probe showed the kernel
//! returning it.
//!
//! The assumption underneath all six was that the scenario deletes what it
//! creates — stated in this ledger repeatedly, including by me, on the
//! strength of reading two lines of source. It has never been counted.
//!
//! A scenario whose control flow sometimes skips its delete leaks with no
//! kernel defect at all, and would produce exactly the NON-UNIFORM rate the
//! capacity probe found: flat for hundreds of cycles, then away.
//!
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test creates_vs_deletes -- --ignored --nocapture
//! ```

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{pins, Digest};
use rusty_rtos_demo_core::runner::{Shared, State};
use rusty_rtos_demo_core::{step_limit_for, Runner};

const LENGTHS: [u64; 6] = [1_000, 100_000, 200_000, 210_000, 219_000, 221_000];

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn every_created_queue_is_deleted() {
    println!();
    println!("=== AbortDelay: queues created against queues deleted ===");
    println!();
    println!("{:>10}  {:>9}  {:>9}  {:>8}", "ticks", "created", "deleted", "unclosed");

    let mut worst = 0u32;
    for ticks in LENGTHS {
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

        let borrowed = shared.borrow();
        let State::AbortDelay(s) = &borrowed.state else {
            panic!("the scenario's state is not AbortDelay's");
        };
        let unclosed = s.queue_creates.saturating_sub(s.queue_deletes);
        worst = worst.max(unclosed);
        println!(
            "{:>10}  {:>9}  {:>9}  {:>8}",
            ticks, s.queue_creates, s.queue_deletes, unclosed
        );
    }

    println!();
    if worst <= 1 {
        println!("Creates and deletes track to within one -- the one in flight.");
        println!("The scenario DOES delete what it creates, so the exhaustion is");
        println!("not a skipped delete and the kernel is back under suspicion.");
    } else {
        println!("{worst} queues created and never deleted.");
        println!();
        println!("That is a SCENARIO defect, not a kernel one: the control flow");
        println!("leaves its delete unreached on some path. It also explains the");
        println!("non-uniform rate -- a delete skipped occasionally, not a slot");
        println!("lost every cycle.");
    }
}
