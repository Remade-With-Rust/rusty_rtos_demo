# mps2-an385-qemu-corpus — the conformance corpus on a Cortex-M3

Six conformance scenarios running on an ARMv7-M and producing a trace
**byte-identical to the C FreeRTOS kernel's**.

```sh
cargo run --release                        # qemu-system-arm, no hardware
kairos check rusty_rtos_demo --qemu        # or as a gate
```

```text
dynamic    ok    ticks=2000 yields=3589 exits=21346 lines=24402 bytes=757383
PollQ      ok    ticks=2001 yields=43 exits=2116 lines=2362 bytes=68195
BlockQ     ok    ticks=2002 yields=3913 exits=25681 lines=26948 bytes=762294
semtest    ok    ticks=2000 yields=1296 exits=23281 lines=30099 bytes=684801
GenQTest   ok    ticks=2000 yields=3013 exits=26017 lines=25126 bytes=683187
TimerDemo  ok    ticks=2005 yields=111 exits=2672 lines=3091 bytes=90192

RESULT: PASS -- 6 scenarios byte-identical to the C kernel
        on a Cortex-M3, at 2000 ticks each.        QEMU exit code: 0
```

## Why this works without a context-switching port

The family has no `rusty_rtos_port-cortex-m` yet, and this needs none. A
demo scenario is a **state machine** driven by `Runner` — one `step` per C
statement, with a `pc` — so a task needs no stack of its own and the
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

Six of the seventeen pinned scenarios, chosen to cover different kernel
paths rather than to be quick: `dynamic` is the scheduler itself, `PollQ`
the non-blocking queue surface, `BlockQ` blocking sends and receives,
`semtest` semaphores, `GenQTest` peeks and mutexes, `TimerDemo` the
software timers and their daemon. The remaining eleven are host-side only
because they are pinned there, not because anything stops them running
here.
