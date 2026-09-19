# rusty_rtos_demo

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![crates.io](https://img.shields.io/crates/v/rusty_rtos_demo.svg)](https://crates.io/crates/rusty_rtos_demo)
[![docs.rs](https://docs.rs/rusty_rtos_demo/badge.svg)](https://docs.rs/rusty_rtos_demo)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

The conformance corpus for Kairos. FreeRTOS's own demo task files remade as
Rust state machines, run beside the C kernel compiled from pinned sources, both
sides emitting a trace line per kernel event. Identical means identical.

- **The corpus**: 19 scenarios, each byte-identical to the C kernel for
  100,000 ticks, on four architectures — host, ARMv7-M, RV32 and Xtensa LX7,
  the last on silicon.
- **The method**: each scenario is a state machine because a task here owns no
  stack, which is what lets the same corpus run on a part with no context-switch
  port at all. The sim contract (`ORACLES.md`) fixes the tick, the exits and the
  yields on both sides so a single early tick moves every later line.

**Known gaps.** `IntQueue` is out of scope for a signal-driven host port.
`AbortDelay` is 1,945 of 2,549 lines and the next line is a contract question
rather than a kernel one — the C harness keys a queue's trace ordinal on its
malloc address.

- This package's plan: [docs/plans/rusty_rtos_demo.md](https://github.com/Remade-With-Rust/rusty_rtos_demo/blob/main/docs/plans/rusty_rtos_demo.md)
- Every number: [docs/LEDGER.md](https://github.com/Remade-With-Rust/rusty_rtos_demo/blob/main/docs/LEDGER.md)
- The family plan: Kairos [`docs/plans/rtos-mission.md`](https://github.com/Remade-With-Rust/kairos/blob/main/docs/plans/rtos-mission.md)

**Claims discipline:** this README makes no performance or capability claim that
is not backed by a test, a benchmark ledger entry, or a kill test recorded in
the plan. "Scaffold" means scaffold. "Sim only" means the sim port; "builds, not
flashed" means no chip has run it.

## Conformance

| | |
|---|---|
| scenarios byte-identical to the C kernel | **19** |
| ticks per scenario | 100,000 |
| soak, both emulators | RV32 18/18 in 58 min · M3 18/18 in 75 min |
| on silicon (XIAO ESP32-S3) | **18/18** |

```sh
kairos conform --all --ticks 100000      # from the Kairos umbrella
```

Offline, all of it is pinned by counters, line count, byte count and an
FNV-1a/64 digest of the C kernel's own trace file, so a regression is caught
without the oracle present.

## Using it

```sh
kairos conform --all --ticks 100000   # the whole corpus against the C kernel
kairos conform AbortDelay             # one scenario, on its own
cargo test -p rusty_rtos_demo-core    # the offline pins, no oracle needed
```

## Performance

No rows of its own: this package measures agreement, not speed. The timing
rows belong to [`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel)
and [`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port).

## Portability

| target | corpus |
|---|---|
| host (x86-64 Windows, Linux) | ✅ 19/19 |
| `thumbv7m-none-eabi` | ✅ 18/18, QEMU `mps2-an385` |
| `riscv32imac-unknown-none-elf` | ✅ 18/18, QEMU `virt` |
| `xtensa-esp32s3-none-elf` | ✅ 18/18, **on silicon** |

## Layout

```text
crates/rusty_rtos_demo          facade: re-exports + prelude; the crate you depend on
crates/rusty_rtos_demo-core     no_std (+ alloc); forbid(unsafe); types, traits, algorithms
firmware/                per-chip example projects, excluded from the workspace
docs/plans/              this package's plan and its hardening audit
docs/LEDGER.md           every number, with its method line
```

## Build

```sh
cargo test --workspace                                   # host: the tests
cargo check -p rusty_rtos_demo-core --no-default-features \
  --target thumbv7em-none-eabihf                         # Cortex-M4F class, no alloc
cargo check -p rusty_rtos_demo-core --no-default-features --features alloc \
  --target riscv32imac-unknown-none-elf                  # ESP32-C6 class, with alloc
```

CI holds the core to `thumbv7em-none-eabihf`, `thumbv8m.main-none-eabihf`,
`riscv32imac-unknown-none-elf` and `riscv32imafc-unknown-none-elf`, with and
without `alloc`, plus `cargo deny check`. Firmware examples (Xtensa needs the
esp toolchain; Cortex-M and RISC-V work on stable) are built from their own
directories under `firmware/`.

## Part of Remade With Rust

This crate is part of **[Kairos](https://github.com/Remade-With-Rust/kairos)** —
FreeRTOS remade in memory-safe Rust, as independent packages that expose the API
a FreeRTOS developer already knows and prove every scheduling decision against
the C kernel's own trace. `rusty_rtos_demo` is the evidence the rest of the family rests on.

**Where this sits for Mata.** Kairos is the real-time layer on the device
itself, and [`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) is the way out of it.
Paired with the **MATA distributed cloud**, robotics and sensor data has two
routes — read it on the machine, or reach it through the cloud — with the same
memory-safe crates at both ends.

The family:
[`rusty_rtos_core`](https://crates.io/crates/rusty_rtos_core) (the shared vocabulary),
[`rusty_rtos_kernel`](https://crates.io/crates/rusty_rtos_kernel) (the scheduler),
[`rusty_rtos_port`](https://crates.io/crates/rusty_rtos_port) (the architecture seam),
[`rusty_rtos_heap`](https://crates.io/crates/rusty_rtos_heap) (the allocators),
[`rusty_rtos_json`](https://github.com/Remade-With-Rust/rusty_rtos_json) (coreJSON),
[`rusty_rtos_sntp`](https://github.com/Remade-With-Rust/rusty_rtos_sntp) (coreSNTP),
[`rusty_rtos_mqtt`](https://github.com/Remade-With-Rust/rusty_rtos_mqtt) (coreMQTT),
[`rusty_rtos_backoff`](https://github.com/Remade-With-Rust/rusty_rtos_backoff) (backoffAlgorithm),
[`rusty_rtos-capi`](https://github.com/Remade-With-Rust/rusty_rtos-capi) (the C ABI) and
[`rusty_rtos_demo`](https://github.com/Remade-With-Rust/rusty_rtos_demo) (the conformance corpus).
The last six are on GitHub and not yet on crates.io. Also check out
the rest of **[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

**[Mata Network](https://www.mata.network/)** builds sovereign, self-hostable
privacy infrastructure — *"stop sacrificing your privacy for convenience"*:
wallet & identity, a password manager, a contact manager, and a browser
extension that stops your information leaking as you browse.

**Remade With Rust** is our open-source home for the permissively-licensed
building blocks that work depends on — including
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs) (the
FFmpeg alternative) and [FFAI](https://github.com/Remade-With-Rust/FFAI) (the
AI media toolkit).

→ **[www.mata.network](https://www.mata.network/)**

<!-- /ORG BOILERPLATE -->

## License

MIT OR Apache-2.0, at your option. FreeRTOS is MIT-licensed by Amazon.com,
Inc. or its affiliates; this crate remakes its API and behaviour from the
published sources and links no FreeRTOS code.

---

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-09-16 (v0.1.0 release pass) · **v1.0.0 gates** 10/17 · [Full checklist](https://github.com/Remade-With-Rust/rusty_rtos_demo/blob/main/docs/plans/use-protection-please.md)

`████████████░░░░░░░░` **62%** &nbsp;·&nbsp; 15 Completed · 0 Scheduled · 9 Incomplete · 31 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 0 | 0 | 1 | 1 |
| 1 — Toolchain | 2 | 0 | 2 | 0 |
| 2 — Supply chain | 7 | 0 | 0 | 1 |
| 3 — Code level | 3 | 0 | 1 | 3 |
| 4 — Static analysis | 0 | 0 | 0 | 1 |
| 5 — Dynamic analysis | 1 | 0 | 0 | 2 |
| 6 — Fuzzing and properties | 1 | 0 | 0 | 3 |
| 7 — Formal verification | 0 | 0 | 0 | 1 |
| 8 — Build and binary | 0 | 0 | 1 | 1 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 1 | 0 | 4 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **15** | **0** | **9** | **31** |

Gates waived for 0.x are listed with their reasons in the plan's "v0.1.0 release decision" section — an Incomplete gate not listed there is an omission, not a decision.

**Architect** — [Tim Almond](https://github.com/Ttimmahlax) — accountable for this unit's security design; rendered
<!-- HARDENING-TABLE:END -->
