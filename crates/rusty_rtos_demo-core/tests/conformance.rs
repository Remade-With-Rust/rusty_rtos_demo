//! The K1 conformance regression, without a C toolchain.
//!
//! `kairos conform` is the real gate: it runs a scenario on this kernel and
//! on the instrumented C kernel and compares the two traces line for line.
//! That needs the oracle checked out, patched and built, which CI does not
//! have — so this test pins what the oracle *said*, on the day it said it,
//! and fails if the Rust kernel ever decides something else.
//!
//! The numbers below are not chosen, invented or rounded. Each row is the
//! `KAIROS_RESULT` line the C kernel printed for that scenario at 2000
//! ticks on 2026-09-09 (umbrella `docs/LEDGER.md`), and each digest is of
//! the C kernel's own trace file, not of ours. Changing any of them means
//! either the C oracle was re-pinned or the sim contract changed — both of
//! which are decision-log rows, not test edits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use core::cell::RefCell;
use core::pin::pin;

use rusty_rtos_demo_core::pins::{Digest, PIN_TICKS, pins};
use rusty_rtos_demo_core::runner::Shared;
use rusty_rtos_demo_core::{Runner, pollq_async, step_limit_for};

#[test]
#[cfg_attr(
    miri,
    ignore = "2000 ticks x the whole corpus is hours under Miri; \
                     `every_scenario_is_deterministic` covers the same code there"
)]
fn every_scenario_reproduces_the_c_kernels_trace_and_counters() {
    for pin in &pins::<Digest>() {
        let kernel =
            Runner::kernel_for(Digest::new()).expect("the sim geometry holds the scenario");
        let shared = RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            (pin.start)(&mut runner, PIN_TICKS).expect("the scenario starts");
            runner.run(step_limit_for(PIN_TICKS))
        };

        assert!(
            !verdict.runaway,
            "{}: the scenario did not finish",
            pin.name
        );
        assert!(verdict.pass, "{}: failed its own check", pin.name);
        assert_eq!(verdict.ticks, pin.ticks, "{}: tick count", pin.name);
        assert_eq!(
            verdict.yields, pin.yields,
            "{}: portYIELD() calls",
            pin.name
        );
        assert_eq!(
            verdict.exits, pin.exits,
            "{}: outermost critical-section exits, which is sim time itself",
            pin.name
        );
        assert_eq!(verdict.lines, pin.lines, "{}: trace lines", pin.name);

        let digest = Runner::into_writer(kernel);
        assert_eq!(
            digest.bytes(),
            pin.bytes,
            "{}: trace size in bytes",
            pin.name
        );
        assert_eq!(
            digest.hash(),
            pin.digest,
            "{name}: the trace differs from the one the C kernel produced; run \
             `kairos conform {name} --exits` to see the first line that moved",
            name = pin.name
        );
    }
}

/// The `async` arm reproduces `PollQ`'s trace exactly (mission plan, K2.2).
///
/// It cannot go in [`pins`] because its bodies are futures and the caller
/// has to pin them, which is one line more than the table's `start` shape
/// allows. Everything else is the same: the same [`Runner`], the same
/// `step_once`, the same verdict — and the same numbers as `PollQ`, which
/// is the claim. If an `.await` ever landed somewhere a `pc` arm did not,
/// this is what would say so.
///
/// It pins with [`core::pin::pin!`] rather than `Box::pin`, which is the
/// point of the mechanism: a future of unnameable type goes in a task slot
/// with no allocator, no `unsafe` and nothing unstable.
#[test]
fn the_async_arm_reproduces_pollqs_trace_exactly() {
    let table = pins::<Digest>();
    let pollq = table
        .iter()
        .find(|p| p.name == "PollQ")
        .expect("PollQ is pinned");

    let kernel = Runner::kernel_for(Digest::new()).expect("the sim geometry holds");
    let shared = RefCell::new(Shared::default());
    let verdict = {
        let (producer, consumer) =
            pollq_async::tasks(&kernel, &shared).expect("the async arm starts");
        let mut producer = pin!(producer);
        let mut consumer = pin!(consumer);
        let mut runner = Runner::new(&kernel, &shared);
        pollq_async::start(&mut runner, PIN_TICKS, producer.as_mut(), consumer.as_mut())
            .expect("the async arm starts");
        runner.run(step_limit_for(PIN_TICKS).saturating_mul(4))
    };
    let digest = Runner::into_writer(kernel);

    assert!(verdict.pass, "the async arm failed its own check");
    assert!(!verdict.runaway, "the async arm did not finish");
    assert_eq!(verdict.ticks, pollq.ticks, "ticks");
    assert_eq!(verdict.yields, pollq.yields, "portYIELD() calls");
    assert_eq!(
        verdict.exits, pollq.exits,
        "outermost critical-section exits, which is sim time itself"
    );
    assert_eq!(verdict.lines, pollq.lines, "trace lines");
    assert_eq!(digest.bytes(), pollq.bytes, "trace size in bytes");
    assert_eq!(
        digest.hash(),
        pollq.digest,
        "the async arm's trace differs from PollQ's; an await point has \
         moved off a `pc` arm boundary"
    );
}

/// Two runs must agree — and, under Miri, this is also the only test that
/// puts the kernel's arenas and lists through an interpreter that checks
/// them. Twenty ticks is short enough to finish there and long enough for
/// every scenario to have created its objects, started its tasks and gone
/// round its loop.
#[test]
fn every_scenario_is_deterministic() {
    let ticks: u64 = if cfg!(miri) { 20 } else { 500 };
    for pin in &pins::<Digest>() {
        let run = || {
            let kernel = Runner::kernel_for(Digest::new()).unwrap();
            let shared = RefCell::new(Shared::default());
            let verdict = {
                let mut runner = Runner::new(&kernel, &shared);
                (pin.start)(&mut runner, ticks).unwrap();
                runner.run(step_limit_for(ticks))
            };
            (
                verdict.exits,
                verdict.lines,
                Runner::into_writer(kernel).hash(),
            )
        };
        let (exits, lines, hash) = run();
        assert!(lines > 0, "{}: the scenario traced nothing", pin.name);
        assert_eq!(
            (exits, lines, hash),
            run(),
            "{}: two runs of the sim must agree",
            pin.name
        );
    }
}
