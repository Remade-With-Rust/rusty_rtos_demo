# mps2-an385-qemu-corpus — the conformance corpus on a Cortex-M3

All **17** conformance scenarios running on an ARMv7-M and producing a
trace **byte-identical to the C FreeRTOS kernel's**.

```sh
cargo run --release                        # qemu-system-arm, no hardware
kairos check rusty_rtos_demo --qemu        # or as a gate
```

```text
dynamic                ok    ticks=2000 yields=3589 exits=21346 lines=24402 bytes=757383
PollQ                  ok    ticks=2001 yields=43 exits=2116 lines=2362 bytes=68195
BlockQ                 ok    ticks=2002 yields=3913 exits=25681 lines=26948 bytes=762294
semtest                ok    ticks=2000 yields=1296 exits=23281 lines=30099 bytes=684801
...
PollQ-typed            ok    ticks=2001 yields=43 exits=2116 lines=2362 bytes=68195

RESULT: PASS -- 17 scenarios byte-identical to the C kernel
        on a Cortex-M3, at 2000 ticks each.        QEMU exit code: 0
```

## Why this works without a context-switching port

`rusty_rtos_port-cortex-m` exists now, and this cell still needs none of
it. A demo scenario is a **state machine** driven by `Runner` — one `step`
per C statement, with a `pc` — so a task needs no stack of its own and the
kernel needs no context switch to run it. `rusty_rtos_demo-core` is
`no_std` with **no `alloc`**, and builds for `thumbv7m-none-eabi`
unchanged.

That is a consequence of the K2 design rather than a trick: the kernel
keeps a blocking call's locals in the TCB (`WaitFrame`) instead of on a C
stack, which is what lets a call return and be re-entered. The same
property that made the corpus provable is what makes it portable.

## Why the numbers are worth more than "it ran"

Each scenario is checked against the host's `tests/conformance.rs` pins —
ticks, yields, exits, lines, the FNV-1a/64 digest and the byte count — and
**those pins are the C kernel's**: the host test diffs them against
`oracle/traces/*`. Matching them here means this Cortex-M build agrees
with C FreeRTOS to the byte, not merely with itself.

`exits` is the one to watch. It is sim time itself — the count of
outermost critical-section exits — so anything that changed *when* the
scheduler ran would move it long before it moved a digest. The poison test
is exactly that: changing `dynamic`'s pinned `exits` from 21,346 to 21,347
gives

```text
dynamic    FAIL
           exits     21346 want    21347   <- sim time itself
```

with every other field still matching, and the cell exits non-zero.

## What it does not claim

**No timing.** QEMU is a translator, not a pipeline simulator. The sibling
`rusty_rtos_core/firmware/mps2-an385-qemu-region` cell measured six ways
that it can supply no cycle, latency or work counter at all — DWT
unimplemented, SysTick tracking host wall time and *shrinking* as work
grows, nothing under `-icount`. This cell therefore asserts only counts
and a hash, which are exact on any host.

## All seventeen, from one table

This cell ran six of the seventeen for a while — chosen to cover different
kernel paths — and the other eleven were host-side only because they were
pinned there, not because anything stopped them running here. Nothing did:
the whole corpus takes **5.6 seconds** under QEMU, so it all runs.

The pins live in `rusty_rtos_demo_core::pins` and this cell, the
[RV32 cell](../riscv32-qemu-corpus) and the host's `tests/conformance.rs`
all read them. They used to be three hand-written copies of seventeen rows
of hex, which is a drift waiting to happen: a cell that silently disagrees
with the host is worse than no cell, because it reports PASS against
numbers nobody is comparing. A pin that moves, moves once.
