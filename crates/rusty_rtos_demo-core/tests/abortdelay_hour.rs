//! Which of `still_running`'s three conditions fails `AbortDelay`'s hour.
//!
//! `AbortDelay` is the single scenario that fails K3's one-hour clause, and
//! it fails it on **both** emulators — 24 of 25 on RV32 and on Cortex-M3,
//! the same one. It is bisected to a 1,000-tick bracket: it passes at
//! 220,000 ticks and fails at 221,000, reaching the full requested length
//! every time with `runaway=false`. So it neither hangs nor loops.
//!
//! That is as far as bisection goes. `State::still_running` returns false
//! for three different reasons and the cell only reports the answer:
//!
//! 1. `controlling_cycles` has not moved since the previous check,
//! 2. `blocking_cycles` has not moved since the previous check,
//! 3. `error` is set — the scenario itself detected a fault.
//!
//! **Three and the other two are different findings.** `error` means a block
//! came back outside its allowable margin, which is a kernel or scenario
//! defect. A stalled cycle counter means a task stopped being scheduled,
//! which is a liveness one. Guessing between them would be the third
//! unverified mechanism in a day, so this asks instead.
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

#[test]
#[ignore = "a diagnostic, not a gate: run it with --ignored --nocapture"]
fn which_condition_fails_abort_delay() {
    let pin = pins::<Digest>()
        .into_iter()
        .find(|p| p.name == "AbortDelay")
        .expect("AbortDelay is in the corpus");

    println!();
    println!("=== AbortDelay: which of still_running's three conditions fires ===");
    println!("the bracket: passes at 220,000 ticks, fails at 221,000 (RV32, reproducible)");
    println!();
    println!(
        "{:>10}  {:>6}  {:>12}  {:>12}  {:>6}",
        "ticks", "pass", "controlling", "blocking", "error"
    );

    for length in LENGTHS {
        let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
        let shared = RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            (pin.start)(&mut runner, length).expect("the scenario starts");
            runner.run(step_limit_for(length))
        };

        let borrowed = shared.borrow();
        let State::AbortDelay(state) = &borrowed.state else {
            panic!("the scenario's state is not AbortDelay's");
        };

        println!(
            "{:>10}  {:>6}  {:>12}  {:>12}  {:>6}",
            length,
            verdict.pass,
            state.controlling_cycles,
            state.blocking_cycles,
            state.error
        );
    }

    println!();
    println!("Reading it: `error` true means the scenario caught a block outside");
    println!("its margin -- a kernel or scenario defect. `error` false with a cycle");
    println!("count equal to the previous check means a task stopped being");
    println!("scheduled -- a liveness one. A cycle count that keeps climbing with");
    println!("pass=false means the check ran between two of that task's cycles,");
    println!("which is a HARNESS artefact and not a kernel finding at all.");
}
