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
//! # Where the future lives
//!
//! An `async fn`'s type has no name, so the runner cannot hold one as a
//! field. It does not have to: [`Body::Async`] holds a
//! `Pin<&mut dyn Future>`, the caller pins the future as a local and lends
//! it, and the runner polls it like any other body. No allocator, no
//! `unsafe`, no unstable feature — `core::pin::pin!` and an unsized coercion.
//!
//! The one thing it forces is that the **kernel is the caller's, not the
//! runner's**: a future borrows the kernel to make its calls, and a future
//! that borrowed a field of the struct polling it would be
//! self-referential. So both borrow a `RefCell` that outlives them. The
//! borrow can never conflict — the runner polls exactly one body at a time,
//! and it takes care to hold no borrow of its own while it does.
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
use core::fmt;
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::Poll;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::pollq::{
    CONSUMER_DELAY, NO_DELAY, PRIORITY, PRODUCER_DELAY, QUEUE_SIZE, State, VALUES_TO_PRODUCE,
};
use crate::runner::{self, Body, Runner, Shared, SimKernel};

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
    shared: &'a RefCell<Shared>,
    producer: bool,
) -> impl Future<Output = ()> + 'a {
    step_call(kernel, move |k| {
        k.enter_critical();
        if let runner::State::PollQ(s) = &mut shared.borrow_mut().state {
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
    shared: &RefCell<Shared>,
    queue: QueueHandle,
) {
    let mut value: u16 = 0;
    let mut error = false;
    loop {
        for _ in 0..VALUES_TO_PRODUCE {
            if send(kernel, queue, value).await {
                if !error {
                    count(kernel, shared, true).await;
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
    shared: &RefCell<Shared>,
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
                        count(kernel, shared, false).await;
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

/// The two futures, for a caller that will pin them.
///
/// An `async fn`'s type has no name, so the runner cannot hold one as a
/// field and the caller has to own it. This is the whole of the storage
/// question K2.2 left open, and the answer is one line at the call site:
/// pin them, then lend them.
///
/// ```ignore
/// let kernel = Runner::kernel_for(sink)?;
/// let (producer, consumer) = pollq_async::tasks(&kernel, &shared)?;
/// let mut producer = core::pin::pin!(producer);
/// let mut consumer = core::pin::pin!(consumer);
/// let mut runner = Runner::new(&kernel, &shared);
/// pollq_async::start(&mut runner, max_ticks, producer.as_mut(), consumer.as_mut())?;
/// ```
///
/// The futures borrow the kernel, which is why the kernel is the caller's
/// and not the runner's: a future that borrowed a field of the struct
/// polling it would be self-referential and cannot be written.
///
/// # Errors
/// As the kernel's create calls.
pub fn tasks<W: fmt::Write>(
    kernel: &RefCell<SimKernel<W>>,
    shared: &RefCell<Shared>,
) -> Result<(impl Future<Output = ()>, impl Future<Output = ()>)> {
    // `vStartPolledQueueTasks`, in the C's order.
    let queue = kernel.borrow_mut().queue_create(QUEUE_SIZE)?;
    shared.borrow_mut().state = runner::State::PollQ(State {
        queue,
        ..State::default()
    });
    Ok((
        producer(kernel, shared, queue),
        consumer(kernel, shared, queue),
    ))
}

/// `vStartPolledQueueTasks`, with the bodies already built.
///
/// The tasks are created in the C's order — the consumer first — because
/// the order is in the trace.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<'a, W: fmt::Write>(
    runner: &mut Runner<'a, W>,
    max_ticks: u64,
    producer: Pin<&'a mut dyn Future<Output = ()>>,
    consumer: Pin<&'a mut dyn Future<Output = ()>>,
) -> Result<()> {
    let (consumer_task, producer_task) = {
        let mut k = runner.kernel_mut();
        let consumer_task = k.create_task("QConsNB", PRIORITY)?;
        let producer_task = k.create_task("QProdNB", PRIORITY)?;
        (consumer_task, producer_task)
    };
    runner.start_common(max_ticks)?;
    runner.attach(consumer_task, Body::Async(consumer));
    runner.attach(producer_task, Body::Async(producer));
    Ok(())
}
