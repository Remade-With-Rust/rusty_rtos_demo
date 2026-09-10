//! The conformance corpus on **RV32**, checked against the host's pins.
//!
//! The Cortex-M3 twin of this cell answered K3's "the corpus passes on
//! M3-qemu"; this one answers "and on RV32-qemu". The two are deliberately
//! the same program over the same table — `rusty_rtos_demo_core::pins` —
//! so what they compare is the *architecture* and nothing else.
//!
//! # Why this can run before the family has a RISC-V port
//!
//! For the same reason the M3 cell could: a scenario is a **state machine**
//! driven by [`Runner`], one `step` per C statement with a `pc`, so a task
//! needs no stack of its own and the kernel needs no context switch to run
//! it. `rusty_rtos_demo-core` is `no_std` with **no `alloc`** and builds
//! for `riscv32imac-unknown-none-elf` unchanged. That is a consequence of
//! the K2 design — a blocking call's locals live in the TCB rather than on
//! a C stack — rather than a trick, and this cell is the second time that
//! property has paid for itself on a new architecture.
//!
//! # What makes this more than "it ran"
//!
//! Every scenario is checked against the **C kernel's** numbers: ticks,
//! yields, exits, lines, an FNV-1a/64 digest of the whole trace and its
//! byte count. Those pins are diffed against `oracle/traces/*` by the
//! host's `tests/conformance.rs`, so matching them here means this RISC-V
//! build produces a trace byte-identical to C FreeRTOS's.
//!
//! `exits` is the one to watch. It is sim time itself — the count of
//! outermost critical-section exits — so a target that changed *when* the
//! scheduler ran would move it long before it moved a digest.
//!
//! # What it does not claim
//!
//! No timing. QEMU is a translator, not a pipeline simulator, and
//! `rusty_rtos_core/firmware/mps2-an385-qemu-region` measured six ways
//! that it can supply no cycle, latency or work counter at all. This cell
//! asserts only counts and a hash, which are exact on any host.

#![no_std]
#![no_main]

use riscv_rt::entry;
use riscv_semihosting::{debug, hprintln};
use panic_halt as _;

// Linked for its side effect only: the `critical-section-single-hart`
// feature's `_critical_section_1_0_acquire`/`_release` symbols, which
// `riscv-semihosting` needs and bare metal does not otherwise supply.
// This machine is single-hart, so disabling interrupts IS the critical
// section — the same shape as the Cortex-M port's PRIMASK, arriving from
// the other architecture.
use riscv as _;

use rusty_rtos_demo_core::pins::{Digest, PIN_TICKS, pins};
use rusty_rtos_demo_core::runner::{Runner, Shared};
use rusty_rtos_demo_core::step_limit_for;

#[entry]
fn main() -> ! {
    hprintln!();
    hprintln!("=== the Kairos conformance corpus on RV32 (QEMU virt) ===");
    hprintln!("target  riscv32imac-unknown-none-elf, no_std, NO alloc, no per-task stack");
    hprintln!("check   every counter and the FNV-1a/64 trace digest against the");
    hprintln!("        host's pins, which are pinned against the C kernel's trace");
    hprintln!();

    let mut failed = 0u32;
    let table = pins::<Digest>();

    for pin in &table {
        let kernel = match Runner::kernel_for(Digest::new()) {
            Ok(k) => k,
            Err(_) => {
                hprintln!("{:<22} FAIL  the sim geometry refused the scenario", pin.name);
                failed += 1;
                continue;
            }
        };
        let shared = core::cell::RefCell::new(Shared::default());
        let verdict = {
            let mut runner = Runner::new(&kernel, &shared);
            if (pin.start)(&mut runner, PIN_TICKS).is_err() {
                hprintln!("{:<22} FAIL  the scenario would not start", pin.name);
                failed += 1;
                continue;
            }
            runner.run(step_limit_for(PIN_TICKS))
        };
        let d = Runner::into_writer(kernel);

        let ok = verdict.pass
            && !verdict.runaway
            && verdict.ticks == pin.ticks
            && verdict.yields == pin.yields
            && verdict.exits == pin.exits
            && verdict.lines == pin.lines
            && d.hash() == pin.digest
            && d.bytes() == pin.bytes;

        if ok {
            hprintln!(
                "{:<22} ok    ticks={} yields={} exits={} lines={} bytes={}",
                pin.name, verdict.ticks, verdict.yields, verdict.exits, verdict.lines, d.bytes()
            );
        } else {
            failed += 1;
            hprintln!("{:<22} FAIL", pin.name);
            hprintln!("           ticks  {:>8} want {:>8}", verdict.ticks, pin.ticks);
            hprintln!("           yields {:>8} want {:>8}", verdict.yields, pin.yields);
            hprintln!("           exits  {:>8} want {:>8}   <- sim time itself", verdict.exits, pin.exits);
            hprintln!("           lines  {:>8} want {:>8}", verdict.lines, pin.lines);
            hprintln!("           bytes  {:>8} want {:>8}", d.bytes(), pin.bytes);
            hprintln!("           digest {:#018x}", d.hash());
            hprintln!("           want   {:#018x}", pin.digest);
        }
    }

    hprintln!();
    if failed == 0 {
        hprintln!("RESULT: PASS -- {} scenarios byte-identical to the C kernel", table.len());
        hprintln!("        on RV32, at {} ticks each.", PIN_TICKS);
        debug::exit(debug::EXIT_SUCCESS);
    } else {
        hprintln!("RESULT: FAIL -- {} of {} scenarios diverged", failed, table.len());
        debug::exit(debug::EXIT_FAILURE);
    }
    loop {
        core::hint::spin_loop();
    }
}
