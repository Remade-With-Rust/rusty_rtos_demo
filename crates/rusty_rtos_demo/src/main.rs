//! `kairos-sim` — run one conformance scenario on the Rust kernel.
//!
//! The command-line contract is the C harness's, so the two are
//! interchangeable in a diff:
//!
//! ```text
//! kairos-sim <scenario> [max_ticks]
//! ```
//!
//! The trace goes to stderr, one line per kernel event, and the last line
//! is the verdict:
//!
//! ```text
//! KAIROS_RESULT <scenario> pass|fail ticks=<n> yields=<n> exits=<n> lines=<n>
//! ```
//!
//! Exit code 0 on pass, 1 on fail, 2 on a usage error. `kairos conform`
//! runs this against `kairos oracle cat <scenario>` and reports the first
//! line that differs.

use std::cell::RefCell;
use std::env;
use std::fmt;
use std::io::{self, BufWriter, Write as _};
use std::pin::pin;
use std::process::ExitCode;

use rusty_rtos_demo_core::runner::{Runner, Shared};
use rusty_rtos_demo_core::{
    Scenario, Step, Verdict, abortdelay, blockq, blocktim, countsem, death, dynamic, eventgroups,
    genqtest, intsem, mbamp, pollq, pollq_async, pollq_typed, qoverwrite, qpeek, qsetpoll,
    recmutex, sbint, semtest, step_limit_for, timerdemo,
};

/// The C harness's default run length.
const DEFAULT_MAX_TICKS: u64 = 2000;

/// A [`fmt::Write`] over a buffered stderr.
///
/// The kernel writes through `fmt::Write` so the same sink serves a UART on
/// a chip; buffering matters because a scenario emits tens of thousands of
/// short lines and an unbuffered write per line is the run's whole cost.
struct Stderr {
    out: BufWriter<io::Stderr>,
    failed: bool,
}

impl Stderr {
    fn new() -> Self {
        Self {
            out: BufWriter::with_capacity(1 << 20, io::stderr()),
            failed: false,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self.out.write_all(s.as_bytes()) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.failed = true;
                Err(fmt::Error)
            }
        }
    }
}

/// The conformance session's loop: one step at a time, with the kernel's
/// counters printed to **stdout** after every step that emitted a trace line.
///
/// stdout, not stderr, so the trace on stderr stays byte-comparable with the
/// oracle's while this runs. Each row is `<line> <exits> <nesting>
/// <suspended> <pended>`, which lines up with the C harness's
/// `KAIROS_TRACE_EXITS` column.
fn run_stepping(runner: &mut Runner<Stderr>, step_limit: u64) -> Verdict {
    let mut steps: u64 = 0;
    let mut pass = false;
    let mut runaway = false;
    let mut last_lines = 0;
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(1 << 20, stdout.lock());
    loop {
        if steps >= step_limit {
            runaway = true;
            break;
        }
        steps = steps.wrapping_add(1);
        let step = runner.step_once();
        let k = runner.kernel();
        let lines = k.trace().lines();
        if lines != last_lines {
            last_lines = lines;
            let _ = writeln!(
                out,
                "{lines} {} {} {} {}",
                k.port().exits(),
                k.port().nesting(),
                k.scheduler_suspended(),
                k.pended_ticks()
            );
        }
        if let Step::Finish(verdict) = step {
            pass = verdict;
            break;
        }
    }
    let _ = out.flush();
    runner.verdict(steps, pass, runaway)
}

fn usage() -> ExitCode {
    let mut err = io::stderr();
    let _ = writeln!(err, "usage: kairos-sim <scenario> [max_ticks]");
    let names: Vec<&str> = Scenario::all().iter().map(|s| s.name()).collect();
    let _ = writeln!(err, "scenarios: {}", names.join(", "));
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(name) = args.first() else {
        return usage();
    };
    let Some(scenario) = Scenario::from_name(name) else {
        let mut err = io::stderr();
        let _ = writeln!(err, "unknown scenario {name}");
        return usage();
    };
    let max_ticks = args
        .get(1)
        .and_then(|t| t.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_TICKS);

    let debug_exits = env::var_os("KAIROS_TRACE_EXITS").is_some();
    let sink = rusty_rtos_demo_core::LineTrace::new(Stderr::new()).with_exit_column(debug_exits);
    let kernel = match Runner::kernel_with_sink(sink) {
        Ok(k) => k,
        Err(e) => {
            let mut err = io::stderr();
            let _ = writeln!(err, "kernel refused the geometry: {e:?}");
            return ExitCode::from(3);
        }
    };
    let shared = RefCell::new(Shared::default());
    let limit = step_limit_for(max_ticks);

    // The runner borrows the kernel and the statics, and an `async` body's
    // future borrows them too — so everything with a lifetime lives in this
    // block and the writer is taken once it has ended.
    // The runner borrows the kernel and the statics, and an `async` body's
    // future borrows them too — so everything with a lifetime lives inside
    // this block, and the writer is taken once it has ended.
    let verdict = if scenario == Scenario::PollQAsync {
        // A future has no nameable type, so it cannot be a field: the
        // caller pins it and lends it. `pin!`, not `Box::pin` — the whole
        // point is that a task slot needs no allocator.
        let (producer, consumer) = match pollq_async::tasks(&kernel, &shared) {
            Ok(pair) => pair,
            Err(e) => {
                let mut err = io::stderr();
                let _ = writeln!(err, "scenario {name} could not start: {e:?}");
                return ExitCode::from(3);
            }
        };
        let mut producer = pin!(producer);
        let mut consumer = pin!(consumer);
        let mut runner = Runner::new(&kernel, &shared);
        if let Err(e) =
            pollq_async::start(&mut runner, max_ticks, producer.as_mut(), consumer.as_mut())
        {
            let mut err = io::stderr();
            let _ = writeln!(err, "scenario {name} could not start: {e:?}");
            return ExitCode::from(3);
        }
        let verdict = if env::var_os("KAIROS_SIM_EXITS").is_some() {
            run_stepping(&mut runner, limit)
        } else {
            runner.run(limit)
        };
        let _ = runner.finish(scenario.name(), &verdict);
        verdict
    } else {
        let mut runner = Runner::new(&kernel, &shared);
        let started = match scenario {
            Scenario::Dynamic => dynamic::start(&mut runner, max_ticks),
            Scenario::PollQ => pollq::start(&mut runner, max_ticks),
            Scenario::BlockQ => blockq::start(&mut runner, max_ticks),
            Scenario::SemTest => semtest::start(&mut runner, max_ticks),
            Scenario::CountSem => countsem::start(&mut runner, max_ticks),
            Scenario::RecMutex => recmutex::start(&mut runner, max_ticks),
            Scenario::BlockTim => blocktim::start(&mut runner, max_ticks),
            Scenario::AbortDelay => abortdelay::start(&mut runner, max_ticks),
            Scenario::Death => death::start(&mut runner, max_ticks),
            Scenario::QPeek => qpeek::start(&mut runner, max_ticks),
            Scenario::GenQTest => genqtest::start(&mut runner, max_ticks),
            Scenario::QOverwrite => qoverwrite::start(&mut runner, max_ticks),
            Scenario::QSetPoll => qsetpoll::start(&mut runner, max_ticks),
            Scenario::IntSem => intsem::start(&mut runner, max_ticks),
            Scenario::SbInt => sbint::start(&mut runner, max_ticks),
            Scenario::TimerDemo => timerdemo::start(&mut runner, max_ticks),
            Scenario::EventGroups => eventgroups::start(&mut runner, max_ticks),
            Scenario::MbAmp => mbamp::start(&mut runner, max_ticks),
            Scenario::PollQTyped => pollq_typed::start(&mut runner, max_ticks),
            // Handled above: its bodies are futures the caller pins.
            Scenario::PollQAsync => Ok(()),
        };
        if let Err(e) = started {
            let mut err = io::stderr();
            let _ = writeln!(err, "scenario {name} could not start: {e:?}");
            return ExitCode::from(3);
        }
        let verdict = if env::var_os("KAIROS_SIM_EXITS").is_some() {
            run_stepping(&mut runner, limit)
        } else {
            runner.run(limit)
        };
        let _ = runner.finish(scenario.name(), &verdict);
        verdict
    };

    let mut sink = Runner::<Stderr>::into_writer(kernel);
    let _ = sink.flush();

    if verdict.runaway {
        let mut err = io::stderr();
        let _ = writeln!(err, "the scenario did not finish within {limit} steps");
    }
    if verdict.pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
