//! # ⚠ This is a REGRESSION GATE, not an optimisation instrument
//!
//! Censused 2026-09-19: of its 119,462,960 instructions, **46.4% is
//! `LineTrace<Digest>::event`** — formatting a trace line and hashing it —
//! 5.2% is `str::from_utf8` re-validating task names, 2.6% is `memcpy`, and
//! **about 6.9% is the kernel**.
//!
//! So a 10% kernel improvement moves this number by 0.7%, which is inside the
//! noise a rebuild produces. Read it to prove a change did NOT move the
//! corpus. For kernel speed work read `khot-ir`, `ksched-ir`, `kipc-ir` or
//! `kobj-ir`, which use `CountTrace` and whose harness is 2.66% of the total.
//!
//! Instruction counts for the Kairos kernel, over the conformance corpus.
//!
//! This runs exactly what `every_scenario_reproduces_the_c_kernels_trace_and_counters`
//! runs -- every pinned scenario, at its own tick floor, through the same sim
//! runner -- so a count taken here is a count of the work the GATED path does.
//! The scheduler, the ready and delayed lists, queues, semaphores, timers and
//! the tick are all under the counter together.
//!
//! A deterministic counter, not a clock. The tick total and the pass count are
//! the work parity anchors: the corpus is pinned, so either moving means the
//! kernel changed behaviour and the differential would have caught it.

use core::cell::RefCell;

use rusty_rtos_demo_core::pins::{Digest, pins};
use rusty_rtos_demo_core::runner::Shared;
use rusty_rtos_demo_core::{Runner, step_limit_for};

fn main() {
    let mut ticks = 0u64;
    let mut passes = 0u64;
    let mut scenarios = 0u64;
    let mut runaway = 0u64;

    for pin in &pins::<Digest>() {
        let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds the scenario");
        let shared = RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            (pin.start)(&mut runner, pin.run_ticks).expect("the scenario starts");
            runner.run(step_limit_for(pin.run_ticks))
        };

        scenarios = scenarios.wrapping_add(1);
        ticks = ticks.wrapping_add(u64::from(verdict.ticks));
        if verdict.pass {
            passes = passes.wrapping_add(1);
        }
        if verdict.runaway {
            runaway = runaway.wrapping_add(1);
        }
    }

    println!("checksum {ticks}");
    println!("scenarios {scenarios} ticks {ticks} passes {passes} runaway {runaway}");
}
