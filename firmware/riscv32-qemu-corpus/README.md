# riscv32-qemu-corpus — the conformance corpus on RV32

**Verified cell.** No hardware: `qemu-system-riscv32`, and it sets QEMU's
exit code, so it gates.

```
kairos check rusty_rtos_demo --qemu     # or: cargo run --release
```

## What it proves

All **17** pinned scenarios of the Kairos conformance corpus produce a
trace **byte-identical to the C FreeRTOS kernel's**, on
`riscv32imac-unknown-none-elf`.

```text
=== the Kairos conformance corpus on RV32 (QEMU virt) ===
dynamic                ok    ticks=2000 yields=3589 exits=21346 lines=24402 bytes=757383
PollQ                  ok    ticks=2001 yields=43 exits=2116 lines=2362 bytes=68195
...
PollQ-typed            ok    ticks=2001 yields=43 exits=2116 lines=2362 bytes=68195

RESULT: PASS -- 17 scenarios byte-identical to the C kernel
        on RV32, at 2000 ticks each.
```

Each scenario is checked on six quantities — ticks, yields, **exits**,
lines, an FNV-1a/64 digest of the entire trace, and its byte count. The
pinned values are the **C kernel's**: the host's `tests/conformance.rs`
diffs them against `oracle/traces/*`. So this is agreement with C
FreeRTOS on RISC-V, not self-consistency.

`exits` is the one to watch — it is sim time itself, the count of
outermost critical-section exits, so anything that changed *when* the
scheduler ran moves it long before it moves a digest.

## It can fail

Moving `dynamic`'s pinned `exits` by one:

```text
dynamic                FAIL
           exits     21346 want    21347   <- sim time itself
```

— every other field matching, and **QEMU exit code 1**. A cell whose test
has never been made to fail is not a gate.

## Why this runs with no RISC-V port

For the same reason the Cortex-M3 twin did: a scenario is a **state
machine** driven by `Runner`, one step per C statement with a `pc`, so a
task needs no stack of its own and the kernel needs no context switch to
run it. `rusty_rtos_demo-core` is `no_std` with **no `alloc`** and builds
for RV32 unchanged.

That is a consequence of the K2 design rather than a trick: the kernel
keeps a blocking call's locals in the **TCB**, not on a C stack, which is
what lets a call return and be re-entered. The property that made the
corpus provable is the one that makes it portable — and this is the second
architecture it has paid for.

## One table, three readers

The pins live in `rusty_rtos_demo_core::pins` and this cell, the Cortex-M3
cell and the host test all read them. They used to be three hand-written
copies of seventeen rows of hex, which is a drift waiting to happen: a cell
that silently disagrees with the host is worse than no cell, because it
reports PASS against numbers nobody is comparing. A pin that moves, moves
once — and the poison above proves all three see it.

## Two things the machine needed

- **`-bios none`.** The ELF *is* the firmware; there is no SBI here and
  nothing to chain to. QEMU loads it into DRAM at `0x8000_0000` and starts
  at its entry point, which is why `memory.x` gives text and data the same
  region.
- **A `critical-section` implementation, linked for its side effect.**
  `riscv-semihosting` takes a critical section around each host call and
  bare metal supplies none, so the build fails with two undefined symbols
  (`_critical_section_1_0_acquire` / `_release`). `riscv`'s
  `critical-section-single-hart` feature provides them — this machine is
  single-hart, so disabling interrupts *is* the critical section, the same
  shape as the Cortex-M port's PRIMASK arriving from the other
  architecture. It needs `use riscv as _;` as well as the dependency:
  nothing calls the crate, so without that the linker drops the rlib and
  the symbols come back undefined anyway.

## What it does not claim

**No timing.** QEMU is a translator, not a pipeline simulator, and
`rusty_rtos_core/firmware/mps2-an385-qemu-region` measured six ways that it
supplies no cycle, latency or work counter at all. This cell asserts only
counts and a hash, which are exact on any host. Cycle rows come from
silicon.

## The one-hour soak

```sh
cargo run --release --features soak     # ~3 hours under QEMU
```

K3 asks that the corpus survive **one hour**. The sim runs
`PosixDemoConfig`, whose `TICK_RATE_HZ` is 1000 — the same value as the
oracle's `FreeRTOSConfig.h` — so one tick is one millisecond and an hour is
**3,600,000 ticks**, not a round number chosen to be quick.

It is a **feature and not a second binary** because `kairos check --qemu`
runs a plain `cargo run --release` here, and a second bin target would make
that ambiguous. The gate's invocation is untouched.

What it checks changes with the length, and says so: at 2000 ticks every
counter and the trace digest are compared against the C kernel's, because
that is where the C trace exists. At an hour there is no C pin — getting
one means an hour-long instrumented C run per scenario — so the check
becomes the one the C demo itself makes: is every scenario's check task
still reporting that it is running? Plus `ticks >= 3,600,000`, without
which a scenario that stopped at tick 5 would report PASS having never been
asked to survive anything. **Liveness, not conformance**, and it prints
that word.

The host runs the same hour in ~2 minutes:
`kairos check rusty_rtos_demo --soak`.
