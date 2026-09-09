//! `countsem` — counting semaphores, driven to both ends of their range.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/countsem.c`: two tasks,
//! one semaphore each, one created full and one created empty. Each task
//! walks its semaphore all the way down and all the way up, checking the
//! count at every step and checking that the give past the maximum and the
//! take past zero both fail. It is the scenario that proves a counting
//! semaphore counts.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.
//! The `configASSERT( uxSemaphoreGetCount(...) )` calls are kept, because
//! `uxSemaphoreGetCount` takes a critical section and on the sim that is
//! where time passes.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `countMAX_COUNT_VALUE`.
pub const MAX_COUNT: usize = 200;
/// `countDONT_BLOCK`.
pub const DONT_BLOCK: u64 = 0;

/// `countsem.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xErrorDetected`.
    pub error: bool,
    /// `xParameters[ n ].uxLoopCounter`.
    pub loops: [u32; 2],
    /// `uxLastCount0` / `uxLastCount1`, statics inside the check function.
    pub last: [u32; 2],
}

impl State {
    /// `xAreCountingSemaphoreTasksStillRunning`.
    pub fn still_running(&mut self) -> bool {
        let mut running = !self.error;
        for i in 0..2 {
            if let (Some(now), Some(last)) = (self.loops.get(i).copied(), self.last.get_mut(i)) {
                if now == *last {
                    running = false;
                } else {
                    *last = now;
                }
            }
        }
        running
    }
}

/// `prvCountingSemaphoreTask`, run by two tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// Where the two subroutines return to.
    ret: u8,
    semaphore: QueueHandle,
    /// Which `uxLoopCounter` this task bumps.
    slot: usize,
    /// `uxExpectedStartCount == countSTART_AT_MAX_COUNT`.
    starts_full: bool,
    /// `ux`, the subroutine loop counter.
    ux: usize,
}

impl Body {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::CountSem(s) = &mut s.state else {
            return Step::Finish(false);
        };
        match self.pc {
            // if( uxExpectedStartCount == countSTART_AT_MAX_COUNT )
            //     { prvDecrementSemaphoreCount( ... ); }
            0 => {
                if self.starts_full {
                    self.ret = 1;
                    self.pc = 20;
                } else {
                    self.pc = 1;
                }
            }
            // if( xSemaphoreTake( xSemaphore, 0 ) == pdPASS ) { xErrorDetected = pdTRUE; }
            1 => {
                if matches!(
                    k.semaphore_take(self.semaphore, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = 2;
            }
            // for( ;; ) { prvIncrementSemaphoreCount(); prvDecrementSemaphoreCount(); }
            2 => {
                self.ret = 3;
                self.pc = 10;
            }
            3 => {
                self.ret = 2;
                self.pc = 20;
            }

            // ---------------------------- prvIncrementSemaphoreCount ----
            // if( xSemaphoreTake( xSemaphore, countDONT_BLOCK ) == pdPASS )
            //     { xErrorDetected = pdTRUE; }
            10 => {
                if matches!(
                    k.semaphore_take(self.semaphore, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.ux = 0;
                self.pc = 11;
            }
            // configASSERT( uxSemaphoreGetCount( xSemaphore ) == ux );
            11 => {
                if k.semaphore_count(self.semaphore) != Ok(self.ux) {
                    s.error = true;
                }
                self.pc = 12;
            }
            // if( xSemaphoreGive( xSemaphore ) != pdPASS ) { xErrorDetected = pdTRUE; }
            // ( *puxLoopCounter )++;
            12 => {
                if !matches!(k.semaphore_give(self.semaphore), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                if let Some(cell) = s.loops.get_mut(self.slot) {
                    *cell = cell.wrapping_add(1);
                }
                self.pc = 13;
            }
            13 => {
                self.ux = self.ux.saturating_add(1);
                self.pc = if self.ux < MAX_COUNT { 11 } else { 14 };
            }
            // if( xSemaphoreGive( xSemaphore ) == pdPASS ) { xErrorDetected = pdTRUE; }
            14 => {
                if matches!(k.semaphore_give(self.semaphore), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.pc = self.ret;
            }

            // ---------------------------- prvDecrementSemaphoreCount ----
            // if( xSemaphoreGive( xSemaphore ) == pdPASS ) { xErrorDetected = pdTRUE; }
            20 => {
                if matches!(k.semaphore_give(self.semaphore), Ok(Wait::Ready(()))) {
                    s.error = true;
                }
                self.ux = 0;
                self.pc = 21;
            }
            // configASSERT( uxSemaphoreGetCount( ... ) == ( countMAX_COUNT_VALUE - ux ) );
            21 => {
                if k.semaphore_count(self.semaphore) != Ok(MAX_COUNT.saturating_sub(self.ux)) {
                    s.error = true;
                }
                self.pc = 22;
            }
            // if( xSemaphoreTake( xSemaphore, countDONT_BLOCK ) != pdPASS )
            //     { xErrorDetected = pdTRUE; }
            // ( *puxLoopCounter )++;
            22 => {
                if !matches!(
                    k.semaphore_take(self.semaphore, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                if let Some(cell) = s.loops.get_mut(self.slot) {
                    *cell = cell.wrapping_add(1);
                }
                self.pc = 23;
            }
            23 => {
                self.ux = self.ux.saturating_add(1);
                self.pc = if self.ux < MAX_COUNT { 21 } else { 24 };
            }
            // configASSERT( uxSemaphoreGetCount( xSemaphore ) == 0 );
            24 => {
                if k.semaphore_count(self.semaphore) != Ok(0) {
                    s.error = true;
                }
                self.pc = 25;
            }
            // if( xSemaphoreTake( xSemaphore, countDONT_BLOCK ) == pdPASS )
            //     { xErrorDetected = pdTRUE; }
            _ => {
                if matches!(
                    k.semaphore_take(self.semaphore, DONT_BLOCK),
                    Ok(Wait::Ready(()))
                ) {
                    s.error = true;
                }
                self.pc = self.ret;
            }
        }
        Step::Continue
    }
}

/// `vStartCountingSemaphoreTasks`: both semaphores first, then both tasks —
/// which is what puts them at ordinals `q1`, `q2` in the trace.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<W>, max_ticks: u64) -> Result<()> {
    let (cnt1, cnt2, sem1, sem2) = {
        let k = runner.kernel_mut();
        let sem1 = k.semaphore_create_counting(MAX_COUNT, MAX_COUNT)?;
        let sem2 = k.semaphore_create_counting(MAX_COUNT, 0)?;
        let cnt1 = k.create_task("CNT1", 0)?;
        let cnt2 = k.create_task("CNT2", 0)?;
        (cnt1, cnt2, sem1, sem2)
    };
    runner.shared_mut().state = runner::State::CountSem(State::default());
    runner.start_common(max_ticks)?;
    runner.attach(
        cnt1,
        runner::Body::CountSem(Body {
            semaphore: sem1,
            slot: 0,
            starts_full: true,
            ..Body::default()
        }),
    );
    runner.attach(
        cnt2,
        runner::Body::CountSem(Body {
            semaphore: sem2,
            slot: 1,
            starts_full: false,
            ..Body::default()
        }),
    );
    Ok(())
}
