# firmware/

Per-chip example projects for `rusty_rtos_demo`. Each directory here is a **separate
cargo project**, excluded from the workspace, because every chip needs its own
target triple, linker script and (for Xtensa parts) its own toolchain. n0's
iroh-on-ESP32 work and the Janus family both reached the same conclusion: keep
the firmware projects out of the library workspace so architecture-specific
patches never leak into it.

Naming: `<board>-<demo>/`, for example `lm3s6965-qemu-flash/` or
`esp32c6-devkitc-blink/`.

| Chip class | Runtime | Target |
|---|---|---|
| Cortex-M3 (QEMU `lm3s6965evb`) | `cortex-m-rt` + `rusty_rtos_port-cortex-m` | `thumbv7m-none-eabi` |
| Cortex-M4F / M7 | same | `thumbv7em-none-eabihf` |
| Cortex-M33 | same | `thumbv8m.main-none-eabihf` |
| RISC-V RV32 (QEMU `virt`) | `riscv-rt` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` |
| ESP32-C6 / P4 | `esp-hal` + `rusty_rtos_port-riscv` | `riscv32imac-unknown-none-elf` / `riscv32imafc-unknown-none-elf` |
| ESP32 / ESP32-S3 | `esp-hal` (esp toolchain) + `rusty_rtos_port-xtensa` | `xtensa-esp32-none-elf` / `xtensa-esp32s3-none-elf` |

Rules:

- Depend on this repo's crates by **path** (`../../crates/rusty_rtos_demo`) inside a
  firmware example; depend on siblings by git URL as usual.
- Release profile for a chip: `opt-level = "s"` (or `"z"`), `lto = true`,
  `codegen-units = 1`, `panic = "abort"`, `overflow-checks = true`.
- A firmware example is not a test. The library's tests run on the host and
  on the sim port.

## The cells

| cell | what it proves | needs |
|---|---|---|
| [`mps2-an385-qemu-corpus`](mps2-an385-qemu-corpus) | the corpus on **ARMv7-M**; the hour is **24 of 25** (`AbortDelay` fails) | nothing — `qemu-system-arm` |
| [`riscv32-qemu-corpus`](riscv32-qemu-corpus) | the same on **RV32**; the hour is **24 of 25**, the same one failing | nothing — `qemu-system-riscv32` |
| [`xiao-s3-corpus`](xiao-s3-corpus) | the same 18, on **Xtensa LX7 — SILICON, not an emulator** | a XIAO ESP32-S3 on a serial port, and the `esp` toolchain. Never started by a gate: its runner is `espflash` |
| [`esp32c6-corpus`](esp32c6-corpus) | the same on **RV32 SILICON** — K3's third hour. **BUILDS, NEVER RUN**: no C6 has been on this bench, so it carries no numbers. Builds on STABLE, with and without `--features soak` | an ESP32-C6 on a serial port. Never started by a gate: its runner is `espflash` |

Both are gates: each ends by calling `debug::exit`, so the guest's verdict
becomes QEMU's exit code, and `kairos check rusty_rtos_demo --qemu`
discovers them off the filesystem rather than from a list here — a cell
nobody runs is not a gate, and a hand-written list is how one gets
forgotten.

Both read the **same** pin table, `rusty_rtos_demo_core::pins`, which the
host's `tests/conformance.rs` reads too. Three copies of seventeen rows of
hex would drift, and a cell that silently disagrees with the host is worse
than no cell.
