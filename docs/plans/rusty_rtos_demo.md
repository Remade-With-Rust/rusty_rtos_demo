# rusty_rtos_demo — package plan

**One sentence:** The FreeRTOS standard demo tasks (Demo/Common/Minimal) remade in Rust as the Kairos conformance corpus: every scenario self-checks like the C original and, on the sim port, diffs its trace against the C kernel's.

Family plan: Kairos `docs/plans/rtos-mission.md` (umbrella repo) — its §2.1
names what this package remakes, wraps and never touches; its §6 carries the
phase this package's kill test belongs to. This file obeys that one.

Written 2026-09-09. Status: **K1 — 9 scenarios of 9, passed.** `dynamic`,
`PollQ`, `BlockQ`, `semtest`, `countsem`, `recmutex`, `blocktim`, `QPeek`
and `GenQTest` are remade — 34 tasks — and every one traces identically to
the C kernel for 100,000 ticks, 8,408,764 lines in all, counters included.
All nine are pinned offline in `tests/conformance.rs` so drift fails in CI,
which has no C toolchain.

---

## 1. What it is, what it is not

**Is:** the corpus every FreeRTOS port has been judged against for twenty
years, remade. Each C file in `FreeRTOS/Demo/Common/Minimal` starts a
handful of tasks that torture one part of the kernel and answers one
question, `xAre...StillRunning()`. This package remakes them, plus the
runner that drives them on the sim port and the sink that writes the trace
in the contract's format.

**Is not:** a benchmark, a sample application, or a test of this package.
It is a test of `rusty_rtos_kernel`, and its verdict is a diff against the
C kernel.

## 2. The laws this package encodes

1. **A scenario is a state machine, one `step` per C statement.** The
   kernel cannot switch stacks (`forbid(unsafe)`), so a task cannot block
   inside a call; it returns, and the runner steps whoever the kernel says
   is current. Both kernels then make the same calls in the same order,
   which is all the trace records.
2. **The C statements are all of them, including the assertions.**
   `configASSERT` is defined in the harness, so `uxTaskPriorityGet` and
   `eTaskGetState` really are called — and each takes a critical section,
   which on the sim is where time passes. Dropping an assertion would move
   every tick after it. Each `pc` arm carries the C line it stands for.
3. **The scenario's own check is kept.** A trace diff proves the kernels
   agree; `xAre...StillRunning()` proves they agree about something that
   works. `kairos conform` requires both.
4. **No allocator.** The bodies are an enum, the runner's table is an
   array, and the whole corpus fits in `.bss` — so the same code runs on a
   chip at K3.

## 3. The corpus

| scenario | C file | what it tortures | state |
|---|---|---|---|
| `dynamic` | `dynamic.c` | suspend, resume, priority set, suspend-all, a queue polled from a doubly-suspended scheduler | **conformant, 100,000 ticks** |
| `PollQ` | `PollQ.c` | a queue polled without blocking, both ends | to do |
| `semtest` | `semtest.c` | counting semaphores under contention | to do |
| `GenQTest` | `GenQTest.c` | the generic queue API, mutexes, priority inheritance | to do |
| `blocktim` | `blocktim.c` | block times, and that they are honoured exactly | to do |
| `countsem` | `countsem.c` | counting semaphores to their limit | to do |
| `recmutex` | `recmutex.c` | recursive mutexes | to do |
| `EventGroupsDemo` | `EventGroupsDemo.c` | event groups | to do (K2) |
| `TimerDemo` | `TimerDemo.c` | software timers | to do (K2) |

The five after `dynamic` need blocking queue operations and
`vTaskDelayUntil` from the kernel; the last two need K2's subsystems.

## 4. Roadmap

| Milestone | Adds | Driven by | Kill test |
|---|---|---|---|
| **K1a** (done 2026-09-09) | the runner, the trace sink, `dynamic`, the conformance regression test | K1 | `kairos conform dynamic --ticks 100000` |
| K1b | `PollQ`, `semtest`, `GenQTest`, `blocktim`, `countsem`, `recmutex` | K1 | `kairos conform --all --ticks 100000` |
| K2 | `EventGroupsDemo`, `TimerDemo`, and the K2 subsystems' scenarios | K2 | the same, with nine scenarios |
| K3 | a firmware that runs the corpus on a chip, reporting over a UART | K3 | the corpus passing an hour on QEMU and on a C6 |

## 5. Deliberately absent

- **The 200 board demos.** `ORACLES.md` sparse-checks them out of the
  oracle deliberately; they are vendor board bring-up, not conformance.
- **A `main` that flashes an LED.** This package is a test corpus.
- **Timing.** A scenario counts events; a clock arrives at K3 with a chip.

## 6. Risks

| Risk | Mitigation |
|---|---|
| A remade scenario quietly differs from its C original and the diff passes for the wrong reason | each `pc` arm names the C line it stands for, and the scenario's own `xAre...StillRunning()` has to pass as well as the trace matching |
| The state-machine rewrite is mistaken for the kernel being simpler than it is | the plan says it in §2.1 and the runner's docs say it again: the model is what the corpus costs, not what the kernel avoids |
| Eight scenarios is a long grind and the corpus stalls at one | the gate localises a divergence to a line in one run (`--exits`); the structural work was `dynamic`'s |

## 7. Decision log

| Date | Decision |
|---|---|
| 2026-09-09 | Stamped from the Kairos template; obeys the family plan. |
| 2026-09-09 | A scenario is a state machine driven by a runner, one step per C statement — the only shape a `forbid(unsafe)` kernel can drive, and the shape that keeps the kernel calls in the C's order. |
| 2026-09-09 | The C `configASSERT` calls are kept, because they call kernel functions that take critical sections, and on the sim a critical-section exit is the clock. |
| 2026-09-09 | The conformance regression (`tests/conformance.rs`) pins the counters and a digest of the trace rather than the trace itself: CI has no C toolchain, and a 757 KB fixture would rot. |
