# rusty_rtos_demo — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

This package's numbers are conformance counts. It has no performance
number and will not have one until a chip runs the corpus (K3).

## The corpus against the C kernel (2026-09-09, K1)

| scenario | lines identical at 100,000 ticks | ticks | yields | exits |
|---|---|---|---|---|
| `dynamic` | 1,219,231 | 100000 | 179588 | 1066689 |
| `PollQ` | 117,417 | 100051 | 2056 | 105608 |
| `BlockQ` | 1,344,460 | 100011 | 195466 | 1282272 |
| `semtest` | 1,503,634 | 100000 | 64837 | 1163649 |
| `countsem` | 966,326 | 100000 | 21001 | 1280002 |
| `recmutex` | 1,385,862 | 100000 | 40923 | 1068657 |
| `blocktim` | 131,384 | 100062 | 4505 | 114313 |
| `QPeek` | 486,871 | 100000 | 88964 | 414030 |
| `GenQTest` | 1,253,579 | 100000 | 150435 | 1299809 |
| **all nine** | **8,408,764** | equal on both sides | equal | equal |

| gate | result | method |
|---|---|---|
| scenarios covered | **9 of 9** | the K1 corpus complete: 34 demo tasks as resumable state machines |
| the gate | `kairos conform --all --ticks 100000` from the umbrella | the C arm is FreeRTOS-Kernel V11.3.1 @ `3a22924e` on its Posix port under the sim-contract-v1 patch, built by `kairos oracle build` |
| counters | ticks, yields, exits and line counts equal on both sides, every scenario | the `KAIROS_RESULT` line both harnesses print |
| each scenario's own check | pass, all nine | every `xAre...StillRunning()` remade with the C's own counters and error latches, so a scenario that traced correctly but did the wrong work still fails |
| the same nine at 2000 ticks | identical, and pinned | `tests/conformance.rs` asserts the counters, the line count, the byte count and an FNV-1a/64 digest **of the C kernel's own trace file** for all nine. It needs no C toolchain and so runs in CI. Poisoning one pinned number fails the test, so the gate is not vacuous |
| determinism | two runs of the sim agree on exits, lines and the trace digest, all nine | `tests/conformance.rs::every_scenario_is_deterministic` |

## The corpus against the C kernel (2026-09-09, K2)

Six scenarios more, and with them the first *interrupt* halves, the
software timer daemon and event groups.

| scenario | lines identical at 100,000 ticks | ticks | yields | exits |
|---|---|---|---|---|
| `QueueOverwrite` | 1,300,381 | 100000 | 1001 | 1600001 |
| `QueueSetPolling` | 1,373,605 | 100000 | 34334 | 1066673 |
| `IntSemTest` | 132,892 | 100001 | 5114 | 119569 |
| `StreamBufferInterrupt` | 113,299 | 100009 | 1331 | 104709 |
| `TimerDemo` | 156,491 | 100000 | 7128 | 133355 |
| `EventGroupsDemo` | 1,202,786 | 100003 | 248478 | 825505 |
| `MessageBufferAMP` | 120,513 | 100032 | 3469 | 103217 |
| **all sixteen** | **12,808,722** | equal on both sides | equal | equal |

| gate | result | method |
|---|---|---|
| scenarios covered | **16**, of which 14 are on K2's list of eighteen | the other two are K1's; `IntQueue` is out of scope for a signal-driven host port, and the four that remain are blocked above the kernel (umbrella `docs/LEDGER.md`) |
| the gate | `kairos conform --all --ticks 100000` from the umbrella | as K1's |
| offline regression | all sixteen pinned by counters, line count, byte count and an FNV-1a/64 digest of the C kernel's own 2000-tick trace file | `tests/conformance.rs::every_scenario_reproduces_the_c_kernels_trace_and_counters` |
| Miri | green over all sixteen | `cargo +nightly miri test --workspace`, 891.8 s: every scenario twice at 20 ticks (`cfg!(miri)` shortens them so the interpreter can finish), which is what puts the arenas, the lists, the byte allocator and the timer ring through an interpreter that checks them |

## The build fact (2026-09-09)

| gate | result | method |
|---|---|---|
| `cargo test --workspace` | 2 tests pass, covering all nine scenarios | the two above |
| `cargo check -p rusty_rtos_demo-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | all 8 rungs pass | `kairos check rusty_rtos_demo --fmt --clippy --test --deny`, exit 0. The corpus is allocator-free by construction, which is what lets K3 run it on a chip |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | same run |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok | same run |
| Miri | green over the whole kernel through this corpus | `cargo +nightly miri test --workspace`, 779 s, miri 0.1.0 of 2026-09-08 — all nine scenarios, twice each, at 20 ticks (`cfg!(miri)` shortens them so the interpreter can finish), which exercises the scheduler, the lists, the arenas, the queues, the semaphores and the mutexes |
