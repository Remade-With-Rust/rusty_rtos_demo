# `esp32c6-corpus` — the conformance corpus, and K3's hour, on a C6

> **STATUS: BUILDS, NEVER RUN.** No ESP32-C6 has been on this bench. This
> cell compiles for `riscv32imac-unknown-none-elf`, with and without the
> `soak` feature, and has never been flashed. It carries **no numbers**.

K3 asks for the full corpus check task to pass **an hour each on M3-qemu,
RV32-qemu and a C6**. The first two are measured — 24 of 25 on each,
`AbortDelay` the only failure — and this is the cell for the third.

```sh
cargo run --release                     # the pinned 2,000-tick conformance run
cargo run --release --features soak     # the hour: 3,600,000 ticks
KAIROS_SOAK_ONLY=semtest cargo run --release --features soak   # one scenario
```

## What it is

The same corpus as every other cell, byte for byte: `rusty_rtos_demo-core`
with no default features, `no_std`, **no allocator**, and no per-task stack.
It checks every counter and the FNV-1a/64 trace digest against the host's
pins, which are themselves pinned against the C kernel's own trace.

It is the S3 corpus cell retargeted, because a scenario is a state machine
and a task owns no stack — which is exactly why the corpus ports to a new
chip by changing a target triple and three feature strings rather than by
writing a port.

## Why a C6 specifically

The S3 already runs the corpus on silicon, so this is not the first silicon
run. It is the **second architecture of record on a real part**: Xtensa LX7
and RV32 agreeing on every counter says the corpus is the kernel's behaviour
rather than one chip's.

It also builds on **stable** — no esp toolchain, no `build-std` — unlike
every Xtensa cell here.

## One flag that differs from the Xtensa cells

`-nostartfiles` is **absent** from `.cargo/config.toml`. The Xtensa cells
pass it; `rust-lld` rejects it outright on RISC-V, where `riscv-rt` supplies
the startup. Copying an Xtensa cell's config verbatim fails at link with
`unknown argument '-nostartfiles'`.

`-Tlinkall.x` is still required: esp-hal supplies the whole link map, and
without naming it the image builds and flashes nowhere — `.flash.appdesc` is
never placed and espflash refuses the image.

## When it first runs

Expect `AbortDelay` to fail the hour, as it does on both emulators — it
reaches the full 3,600,000 ticks and its check task declines to pass, and it
is bisected to a boundary between 220,000 and 221,000 ticks on RV32
(`docs/LEDGER.md`). A third architecture agreeing would be useful evidence
that the cause is the scenario rather than the target.

Put the numbers in `docs/LEDGER.md` with a date, and replace the status
banner above.
