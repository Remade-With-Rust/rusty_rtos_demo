//! Which of `still_running`'s three conditions fails `AbortDelay`'s hour,
//! and what the queue looks like when it does.
//!
//! `AbortDelay` is the single scenario that fails K3's one-hour clause, and
//! it fails on **both** emulators — 24 of 25 on RV32 and on Cortex-M3, the
//! same one. It reaches the full requested length every time with
//! `runaway=false`, so it neither hangs nor loops.
//!
//! Three questions, asked in order, each one the last probe could not answer:
//!
//! 1. **Which condition?** `still_running` returns false for three reasons —
//!    controlling cycles stalled, blocking cycles stalled, or `error` set —
//!    and the firmware only ever reported the answer.
//! 2. **Which direction?** `outside_margin` fails on `blocked < expected`
//!    *or* `blocked > expected + ALLOWABLE_MARGIN`, and the source says the
//!    short one is interesting: it is what an abort firing early looks like.
//! 3. **Why?** The first failure is a `queue_send` that did not block, which
//!    means the one-deep queue was not full. This asks how full it was.
//!
//! # Why the queue depth is read HERE and not in the scenario
//!
//! `queue_messages_waiting` is a **charged** kernel call under sim contract
//! v2: calling it inside a scenario body adds a critical-section exit and
//! moves every later trace line. The scenario would stop being
//! byte-identical to the C, which is the one thing it exists to be.
//!
//! So the scenario records only what it already knows — the margin failure,
//! three words of plain data — and the depth is read from **outside**, by
//! this test, after the run has finished. A probe that changes what it
//! measures is not a probe.
//!
//! Run it:
//! ```sh
//! cargo test -p rusty_rtos_demo-core --test abortdelay_hour -- --ignored --nocapture
//! ```

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{pins, Digest};
use rusty_rtos_demo_core::runner::{Shared, State};
use rusty_rtos_demo_core::{step_limit_for, Runner};

/// Either side of the bisected boundary, and one far past it.
const LENGTHS: [u64; 3] = [220_000, 221_000, 3_600_000];

/// Run `AbortDelay` for `ticks` and report what the scenario knows
/// afterwards. Returns `(pass, controlling, blocking, error, first_failure,
/// queued_at_end)`.
#[allow(clippy::type_complexity)]
fn run_for(ticks: u64) -> (bool, i32, i32, bool, Option<(u64, u64, u8)>, Option<usize>) {
    let pin = pins::<Digest>()
        .into_iter()
        .find(|p| p.name == "AbortDelay")
        .expect("AbortDelay is in the corpus");

    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    let shared = RefCell::new(Shared::default());
    let verdict = {
        let mut runner = Runner::new(&kernel, &shared);
        (pin.start)(&mut runner, ticks).expect("the scenario starts");
        runner.run(step_limit_for(ticks))
    };

    let borrowed = shared.borrow();
    let State::AbortDelay(state) = &borrowed.state else {
        panic!("the scenario's state is not AbortDelay's");
    };

    // AFTER the run: this call is charged, and charging it here costs
    // nothing because nothing is being compared to the C any more.
    let queued = match kernel.borrow_mut().queue_messages_waiting(state.queue) {
        Ok(n) => Some(n),
        Err(e) => {
            println!("            queue_messages_waiting refused: {e:?}");
            None
        }
    };

    (
        verdict.pass,
        state.controlling_cycles,
        state.blocking_cycles,
        state.error,
        state.first_margin_failure,
        queued,
    )
}

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn which_condition_fails_abort_delay() {
    println!();
    println!("=== AbortDelay: which condition fires, and what the queue holds ===");
    println!();
    println!(
        "{:>10}  {:>6}  {:>12}  {:>12}  {:>6}  {:>7}",
        "ticks", "pass", "controlling", "blocking", "error", "queued"
    );

    for ticks in LENGTHS {
        let (pass, controlling, blocking, error, first, queued) = run_for(ticks);
        println!(
            "{:>10}  {:>6}  {:>12}  {:>12}  {:>6}  {:>7}",
            ticks,
            pass,
            controlling,
            blocking,
            error,
            queued.map_or_else(|| "?".to_owned(), |q| q.to_string())
        );
        if let Some((expected, blocked, pc)) = first {
            let direction = if blocked < expected {
                "TOO SHORT -- what an abort firing early looks like"
            } else {
                "too long -- an overrun past the margin"
            };
            println!("            first margin failure: expected {expected}, blocked {blocked}, pc {pc} -- {direction}");
        }
    }

    // ------------------------------------------------- the exact tick ----
    // The bracket was 220,000-221,000 from the firmware. Narrow it here to
    // the tick, so the queue depth below is read as close to the failure as
    // a whole run allows.
    println!();
    println!("narrowing to the exact first-failure length...");
    let (mut lo, mut hi) = (220_000u64, 221_000u64);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if run_for(mid).4.is_some() {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    let (_, controlling, _, _, first, queued) = run_for(hi);
    println!("  first failure appears at {hi} ticks (passes at {lo})");
    if let Some((expected, blocked, pc)) = first {
        println!("  expected {expected}, blocked {blocked}, at pc {pc}");
    }
    println!(
        "  the one-deep queue holds {} message(s) at that length",
        queued.map_or_else(|| "?".to_owned(), |q| q.to_string())
    );
    println!("  the blocking task has completed {controlling} controlling cycles");
    println!();
    println!("Reading it: pc 72 is prvTestAbortingQueueSend's FIRST step, a");
    println!("queue_send on a one-deep queue that must block for 100 ticks.");
    println!("`blocked 0` means no time passed -- but the match arm there is");
    println!("`_ =>`, which catches Err(..) as well as a completed send, so a");
    println!("REFUSED send is indistinguishable from an instant one here.");
    println!("The handle is refused by end-of-run (Gone, then InvalidHandle),");
    println!("so which of the two happens at the failing call is STILL OPEN.");
}
