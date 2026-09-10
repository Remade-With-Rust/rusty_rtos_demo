//! `PollQ` a third time, with the task bodies as `async fn` (K2.2).
//!
//! K2 wrote six continuations by hand — `WaitFrame`, `stream_resume`,
//! `owed_exits`, `OwedTrace`, the AMP handler's `stage`, and every body's
//! `pc` — because a kernel with no stacks cannot park a call in the middle
//! and come back to it. Rust has a compiler that writes continuations, and
//! this arm exists to answer one question with a trace instead of an
//! opinion:
//!
//! > **do the await points land exactly where the `Wait::Blocked` points
//! > are today?**
//!
//! **No — and the counterfactual was run rather than reasoned about.** Once
//! it is fixed the traces are identical, exits included. What follows is
//! the whole of it, because it is the finding K2.2 was opened to get.
//!
//! # The trap: a `pc` arm is not a blocking call
//!
//! The obvious mapping is "await where the kernel blocks": a future that
//! answers `Poll::Pending` exactly when a kernel call answers
//! `Wait::Blocked`. That was the first thing tried here, and it does not
//! merely produce a different trace. **It hangs.**
//!
//! `PollQ` is the scenario whose whole subject is *not* blocking — a send
//! and a receive with `pollqNO_DELAY`, and a `uxQueueMessagesWaiting` to
//! decide between them. Not one of its calls can answer `Blocked`, so not
//! one `.await` ever suspends, so `poll` never returns. The task runs on
//! past `vTaskDelay` — past the call that took it off the ready list — and
//! keeps sending as a task the scheduler believes is asleep. Measured:
//! `kairos-sim PollQ-async 200` does not terminate, and the step limit
//! never fires because the runaway is *inside a single poll*.
//!
//! The reason is the one K2 paid for six times. A `pc` arm ends after
//! **every** kernel call, not only the blocking ones, because a tick can
//! land at any critical-section exit and switch the task away; whatever the
//! C would have run next belongs to a frame the scheduler has abandoned. An
//! `.await` has to be worth exactly one of those arms, so every call future
//! here yields once *after* it succeeds: the call is made on one poll and
//! its answer taken on the next.
//!
//! That is the whole of K2.2's answer. `async` writes the continuations,
//! and it writes them correctly — but *where the suspension points go* is
//! the scheduler's business, not the compiler's, and getting it from the
//! kernel's blocking behaviour is precisely wrong.
//!
//! # What that buys, and what it costs
//!
//! It buys the thing K2 paid for six times: the locals live across the
//! suspension because the compiler put them in the future, so there is no
//! `WaitFrame` to write and no `pc` to keep in step with the code. Compare
//! [`crate::pollq`]'s producer — four arms, an explicit `loops` counter and
//! a `pc` threaded through all of it — with the `for` loop below.
//!
//! It costs one extra poll per kernel call, which is invisible: a poll is
//! not a kernel call and the trace only records kernel calls. The step
//! counter roughly doubles and nothing else moves.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::pollq::{
    CONSUMER_DELAY, NO_DELAY, PRIORITY, PRODUCER_DELAY, QUEUE_SIZE, State, VALUES_TO_PRODUCE,
};
use crate::runner::{self, Body, Check, Idle, Shared, SimKernel, Step, Timer, Verdict};

// The kernel is reachable from a task body that also has to survive an
// `.await`. A future cannot hold `&mut SimKernel` across a suspension and
// still let the driver hand the kernel to another task, so the tasks share
// it by `&RefCell` and borrow it for the length of one call. The borrow can
// never conflict — the driver polls exactly one task at a time — and
// `RefCell` is the price of saying so without `unsafe`.

/// One kernel call, and the step boundary after it.
///
/// `call` is polled until it stops answering `Poll::Pending`, which is the
/// kernel's `Wait::Blocked`; then the answer is held back for one more poll
/// so that the `.await` is worth exactly one `pc` arm. See the module note.
fn step_call<'a, W, T>(
    kernel: &'a RefCell<SimKernel<W>>,
    mut call: impl FnMut(&mut SimKernel<W>) -> Poll<T> + 'a,
) -> impl Future<Output = T> + 'a
where
    W: fmt::Write,
    T: 'a,
{
    let mut answer: Option<T> = None;
    poll_fn(move |_cx| {
        if let Some(value) = answer.take() {
            return Poll::Ready(value);
        }
        match call(&mut kernel.borrow_mut()) {
            // The kernel said `Blocked`: this is a real suspension, and the
            // driver will bring us back when the task runs again.
            Poll::Pending => Poll::Pending,
            // The call happened. Yield the step before using the answer.
            Poll::Ready(value) => {
                answer = Some(value);
                Poll::Pending
            }
        }
    })
}

/// `xQueueSend( ..., pollqNO_DELAY )`.
fn send<'a, W: fmt::Write>(
    kernel: &'a RefCell<SimKernel<W>>,
    queue: QueueHandle,
    value: u16,
) -> impl Future<Output = bool> + 'a {
    step_call(kernel, move |k| {
        Poll::Ready(matches!(
            k.queue_send(queue, u64::from(value), NO_DELAY),
            Ok(Wait::Ready(()))
        ))
    })
}

/// `xQueueReceive( ..., pollqNO_DELAY )`.
fn receive<'a, W: fmt::Write>(
    kernel: &'a RefCell<SimKernel<W>>,
    queue: QueueHandle,
) -> impl Future<Output = Option<u16>> + 'a {
    step_call(kernel, move |k| {
        Poll::Ready(match k.queue_receive(queue, NO_DELAY) {
            Ok(Wait::Ready(value)) => Some(u16::try_from(value).unwrap_or(u16::MAX)),
            Ok(Wait::Blocked) | Err(_) => None,
        })
    })
}

/// `uxQueueMessagesWaiting`.
fn waiting<'a, W: fmt::Write>(
    kernel: &'a RefCell<SimKernel<W>>,
    queue: QueueHandle,
) -> impl Future<Output = usize> + 'a {
    step_call(kernel, move |k| {
        Poll::Ready(k.queue_messages_waiting(queue).unwrap_or(0))
    })
}

/// `vTaskDelay`.
fn delay<'a, W: fmt::Write>(
    kernel: &'a RefCell<SimKernel<W>>,
    ticks: u64,
) -> impl Future<Output = ()> + 'a {
    step_call(kernel, move |k| {
        let _ = k.delay(ticks);
        Poll::Ready(())
    })
}

/// `taskENTER_CRITICAL(); count++; taskEXIT_CRITICAL();`
///
/// One arm in the `pc` version, so one `.await` here: the exit can release
/// a tick, and nothing may run after it in this frame.
fn count<'a, W: fmt::Write>(
    kernel: &'a RefCell<SimKernel<W>>,
    state: &'a RefCell<State>,
    producer: bool,
) -> impl Future<Output = ()> + 'a {
    step_call(kernel, move |k| {
        k.enter_critical();
        {
            let mut s = state.borrow_mut();
            if producer {
                s.producer_count = s.producer_count.wrapping_add(1);
            } else {
                s.consumer_count = s.consumer_count.wrapping_add(1);
            }
        }
        k.exit_critical();
        Poll::Ready(())
    })
}

/// `vPolledQueueProducer`, as the C reads.
///
/// Set this beside [`crate::pollq::Producer::step`]: the loop is a loop,
/// `usValue` and `xError` are locals, and there is no `pc`. The compiler
/// keeps them across the suspensions, which is the whole point.
async fn producer<W: fmt::Write>(
    kernel: &RefCell<SimKernel<W>>,
    state: &RefCell<State>,
    queue: QueueHandle,
) {
    let mut value: u16 = 0;
    let mut error = false;
    loop {
        for _ in 0..VALUES_TO_PRODUCE {
            if send(kernel, queue, value).await {
                if !error {
                    count(kernel, state, true).await;
                }
                value = value.wrapping_add(1);
            } else {
                error = true;
            }
        }
        delay(kernel, PRODUCER_DELAY).await;
    }
}

/// `vPolledQueueConsumer`.
async fn consumer<W: fmt::Write>(
    kernel: &RefCell<SimKernel<W>>,
    state: &RefCell<State>,
    queue: QueueHandle,
) {
    let mut expected: u16 = 0;
    let mut error = false;
    loop {
        while waiting(kernel, queue).await > 0 {
            // A zero block time cannot park the task, so `None` means the
            // queue emptied under us and the C `if` falls through to the
            // `while` test.
            if let Some(data) = receive(kernel, queue).await {
                if data == expected {
                    if !error {
                        count(kernel, state, false).await;
                    }
                } else {
                    error = true;
                    expected = data;
                }
                expected = expected.wrapping_add(1);
            }
        }
        delay(kernel, CONSUMER_DELAY).await;
    }
}

/// Run the scenario, and answer the same [`Verdict`] the runner would.
///
/// This is [`crate::runner::Runner::run`]'s loop with one change: when the
/// current task is one of the two async ones it is *polled* rather than
/// stepped. Everything else — the harness's `CHECK`, the idle task and the
/// timer daemon — is the runner's own body, unchanged, so that any
/// difference in the trace is the async bodies' and nothing else's.
///
/// # Errors
/// As the kernel's create calls.
#[allow(
    clippy::too_many_lines,
    reason = "the driver is one loop and its setup"
)]
pub fn run<W: fmt::Write>(
    sink: W,
    max_ticks: u64,
    step_limit: u64,
    exit_column: bool,
) -> Result<(Verdict, W)> {
    let mut kernel = SimKernel::new(
        rusty_rtos_port::sim::SimPort::default(),
        crate::trace::LineTrace::new(sink).with_exit_column(exit_column),
    )?;

    // `vStartPolledQueueTasks`, in the C's order.
    let queue = kernel.queue_create(QUEUE_SIZE)?;
    let consumer_task = kernel.create_task("QConsNB", PRIORITY)?;
    let producer_task = kernel.create_task("QProdNB", PRIORITY)?;

    // `start_common`: the harness's check task, then the scheduler.
    let check_task = kernel.create_task("CHECK", Check::PRIORITY)?;
    let started = kernel.start_scheduler()?;

    let mut shared = Shared {
        max_ticks,
        timer_queue: started.timer_queue,
        state: runner::State::PollQ(State {
            queue,
            ..State::default()
        }),
    };
    let mut bodies: [Body; crate::runner::TASKS] = [Body::Empty; crate::runner::TASKS];
    if let Some(slot) = bodies.get_mut(usize::from(check_task.index())) {
        *slot = Body::Check(Check::default());
    }
    if let Some(slot) = bodies.get_mut(usize::from(started.idle.index())) {
        *slot = Body::Idle(Idle::default());
    }
    if let Some(slot) = bodies.get_mut(usize::from(started.timer.index())) {
        *slot = Body::Timer(Timer::default());
    }

    let state = RefCell::new(State {
        queue,
        ..State::default()
    });
    let kernel = RefCell::new(kernel);

    // The futures borrow the kernel, so the verdict has to be taken while
    // they are still alive and the block has to end before the kernel can
    // be moved out for its writer.
    let verdict = {
        let mut producer_future = pin!(producer(&kernel, &state, queue));
        let mut consumer_future = pin!(consumer(&kernel, &state, queue));
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let mut steps: u64 = 0;
        let mut pass = false;
        let mut runaway = false;
        loop {
            if steps >= step_limit {
                runaway = true;
                break;
            }
            steps = steps.wrapping_add(1);

            // `Runner::step_once`, up to the point where it picks a body.
            let current = {
                let mut k = kernel.borrow_mut();
                if k.resume_pending() {
                    continue;
                }
                k.current()
            };

            if current == producer_task {
                let _ = producer_future.as_mut().poll(&mut cx);
            } else if current == consumer_task {
                let _ = consumer_future.as_mut().poll(&mut cx);
            } else {
                // The harness's own tasks, through the runner's bodies. They
                // read the scenario's statics from `Shared`, so keep the two
                // copies of the counters in step for the check task.
                if let runner::State::PollQ(s) = &mut shared.state {
                    *s = *state.borrow();
                }
                let step = match bodies.get_mut(usize::from(current.index())) {
                    Some(body) => body.step(&mut kernel.borrow_mut(), &mut shared),
                    None => Step::Finish(false),
                };
                if let runner::State::PollQ(s) = &shared.state {
                    *state.borrow_mut() = *s;
                }
                if let Step::Finish(verdict) = step {
                    pass = verdict;
                    break;
                }
            }
        }

        let k = kernel.borrow();
        Verdict {
            pass: pass && !runaway && !k.trace().failed(),
            ticks: k.tick_count(),
            yields: k.port().yields(),
            exits: k.port().exits(),
            lines: k.trace().lines(),
            steps,
            runaway,
        }
    };

    // The verdict *line* is the caller's, as it is for the runner: the
    // offline pin digests the trace without it.
    let k = kernel.into_inner();
    Ok((verdict, k.into_trace().into_writer()))
}
