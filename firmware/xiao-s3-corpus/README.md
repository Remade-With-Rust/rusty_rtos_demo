# `xiao-s3-corpus` — the conformance corpus on ESP32-S3 silicon

The third architecture cell for the Kairos corpus, and the **first on a
part rather than an emulator**.

```
=== the Kairos conformance corpus on ESP32-S3 (Xtensa, SILICON) ===
target  xtensa-esp32s3-none-elf, no_std, NO alloc, no per-task stack

dynamic                ok    ticks=2000 yields=3589 exits=21346 lines=24402 bytes=757383
...
death                  ok    ticks=4000 yields=55 exits=3890 lines=4369 bytes=129014

RESULT: PASS -- 18 scenarios byte-identical to the C kernel
        on ESP32-S3 SILICON, at 2000 ticks or each pin's own floor.
```

## What it claims

**Conformance.** Every scenario is checked against the *C kernel's*
numbers — ticks, yields, exits, line count, an FNV-1a/64 digest of the whole
trace and its byte count — read from `rusty_rtos_demo_core::pins`, the same
table the host test and the two QEMU cells read. Matching them means this
Xtensa build produces a trace **byte-identical to C FreeRTOS's**, on
hardware.

`exits` is the one to watch: it is sim time itself, the count of outermost
critical-section exits, so a target that changed *when* the scheduler ran
would move it long before it moved a digest.

## What it does NOT claim

**No timing.** There is no cycle count here on purpose. The numbers that
matter for K3 belong on a Kairos part, and a wall-clock figure taken through
a JTAG-serial `println` would be measuring the printing.

**No context switch.** See below — that is a feature of the result, not a
gap in it.

**No exit code.** A board has none to hand back, so the verdict is the
`RESULT:` line and a person reads it. That is the one thing this cell cannot
do that the QEMU pair can, which is why those two stay.

## Why this needs no Xtensa port at all

For the same reason the Cortex-M3 and RV32 cells needed none: a scenario is
a **state machine** driven by `Runner`, one `step` per C statement with a
`pc`, so a task keeps its locals in the TCB and needs no stack of its own.
`rusty_rtos_demo-core` is `no_std` with **no `alloc`** and builds for
`xtensa-esp32s3-none-elf` unchanged.

This is worth stating plainly because the K5a plan row assumed the opposite —
that Xtensa work had to begin with a context switch. It does not. A context
switch is what you need to run tasks that own stacks; it is not what you need
to prove this kernel schedules identically to C FreeRTOS on this part. For
the corpus, "the smallest port that supports a real workload" is **no port**.

## Poison-proving

The cell is a gate only if it can fail. Changing one pinned number —
`dynamic`'s `exits`, 21346 to 21347 — rebuilt and reflashed, reports:

```
dynamic                FAIL
           exits     21346 want    21347   <- sim time itself
RESULT: FAIL -- 1 scenario(s) disagreed
```

Same discipline as the two QEMU cells, which are poison-proven the same way.

## Running it

Needs the `esp` toolchain (Xtensa has no upstream rustc target) and a board
on a serial port, so it is **never started by a gate** — `kairos check
--qemu` discovers cells whose runner is a `qemu-system-*`, and this one's is
`espflash`.

```sh
cargo +esp run --release                 # flash and monitor
cargo +esp run --release --features soak # an hour of simulated time instead
```

Measured on a XIAO ESP32-S3 (esp32s3 rev v0.2, 8 MB flash), app size
172,704 bytes.
