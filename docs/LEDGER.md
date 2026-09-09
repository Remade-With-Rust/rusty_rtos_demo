# rusty_rtos_demo — the ledger

Every number this package claims, with the run that produced it. A row
without a method is not a number. Counters before clocks; an external oracle
before a self-metric; the method line names the machine, the pinning, the arm
order and the null-arm floor for anything timed.

This package's numbers are conformance counts. It has no performance
number and will not have one until a chip runs the corpus (K3).

## The corpus against the C kernel (2026-09-09, K1)

| gate | result | method |
|---|---|---|
| `dynamic`, 100,000 ticks | **1,219,231 trace lines identical** to the C kernel's | `kairos conform dynamic --ticks 100000` from the umbrella; the C arm is FreeRTOS-Kernel V11.3.1 @ `3a22924e` on its Posix port under the sim-contract-v1 patch, built by `kairos oracle build` |
| counters at 100,000 ticks | ticks 100000, yields 179588, exits 1066689, lines 1219230 — equal on both sides | the `KAIROS_RESULT` line both harnesses print |
| `dynamic`, 2000 ticks | 24,403 lines identical; trace 757,383 bytes, FNV-1a/64 `0x6beea9f466e51e2d` | `kairos conform dynamic`; the same numbers are asserted by `tests/conformance.rs`, which needs no C toolchain and so runs in CI |
| the scenario's own check | pass | `xAreDynamicPriorityTasksStillRunning()` remade: the check variable moved, the expected value advanced, and neither queue error latched |
| determinism | two runs of the sim agree on exits, lines and the trace digest | `tests/conformance.rs::the_scenario_is_deterministic` |
| scenarios covered | 1 of 9 | the eight in the plan's §3 are to do |

## The build fact (2026-09-09)

| gate | result | method |
|---|---|---|
| `cargo test --workspace` | 2 tests pass | the two above |
| `cargo check -p rusty_rtos_demo-core --no-default-features` and `--features alloc` on `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`, `riscv32imac-unknown-none-elf`, `riscv32imafc-unknown-none-elf` | all 8 rungs pass | `kairos check rusty_rtos_demo --fmt --clippy --test --deny`, exit 0. The corpus is allocator-free by construction, which is what lets K3 run it on a chip |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean | same run |
| `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok | same run |
| Miri | green over the whole kernel through this corpus | `cargo +nightly miri test --test conformance the_scenario_is_deterministic`, 158 s, miri 0.1.0 of 2026-09-08 — a 500-tick scenario, which exercises the scheduler, the lists, the arenas and the queue under the interpreter |
