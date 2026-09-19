//! `semtest` — two binary semaphores, four tasks, one shared variable each.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/semtest.c`. Two tasks
//! poll a semaphore with no block time, two block on another with a 100
//! tick timeout; whoever holds one clears a shared variable and counts it
//! back up to the value the next holder expects to find. It is the scenario
//! that proves mutual exclusion actually excludes: a missed context switch
//! inside the counting loop shows up as a wrong value, and the count is
//! deliberately long enough (0xff or 0xfff) to guarantee switches.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::QueueHandle;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step};

/// `semtstBLOCKING_EXPECTED_VALUE`.
pub const BLOCKING_EXPECTED: u32 = 0xfff;
/// `semtstNON_BLOCKING_EXPECTED_VALUE`.
pub const NON_BLOCKING_EXPECTED: u32 = 0xff;
/// `semtstNUM_TASKS`.
pub const NUM_TASKS: usize = 4;
/// `semtstDELAY_FACTOR`.
pub const DELAY_FACTOR: u64 = 10;
/// The block time the two blocking tasks use.
pub const BLOCK_TIME: u64 = 100;
/// The priority `oracle/harness/main.c` starts this scenario at.
pub const PRIORITY: u8 = 1;

/// `semtest.c`'s file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `sCheckVariables`.
    pub check: [i16; NUM_TASKS],
    /// `sLastCheckVariables`, a static inside the check function.
    pub last_check: [i16; NUM_TASKS],
    /// `sNextCheckVariable`.
    pub next_check: i16,
    /// The two `pulSharedVariable`s, one per semaphore.
    pub shared: [u32; 2],
}

impl State {
    /// `xAreSemaphoreTasksStillRunning`: every one of the four counters
    /// must have moved.
    pub fn still_running(&mut self) -> bool {
        let mut running = true;
        for i in 0..NUM_TASKS {
            if let (Some(now), Some(last)) =
                (self.check.get(i).copied(), self.last_check.get_mut(i))
            {
                if now == *last {
                    running = false;
                }
                *last = now;
            }
        }
        running
    }
}

/// The scenario's one task function, run by four tasks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Body {
    pc: u8,
    /// The semaphore this task contends for.
    semaphore: QueueHandle,
    /// Which of the two shared variables goes with it.
    shared: usize,
    /// `xBlockTime`.
    block_time: u64,
    /// `ulExpectedValue`, from the block time.
    expected: u32,
    /// `sCheckVariableToUse`, claimed under a critical section at startup.
    check_slot: usize,
    /// `sError`.
    error: bool,
    /// `ulCounter`, the inner loop.
    counter: u32,
}

impl Body {
    #[inline(never)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let runner::State::SemTest(s) = &mut s.state else {
            return Step::Finish(false);
        };
        // for( ulCounter = 0; ulCounter <= ulExpectedValue; ulCounter++ )
        //     { *pulSharedVariable = ulCounter; if( ... != ulCounter ) sError = pdTRUE; }
        //
        // Pure computation: no kernel call, so no critical section, so no
        // tick — on either side. The loop is long on purpose, and the
        // switches that do happen come from the *other* tasks.
        //
        // It is also very nearly every step this body takes, so it is tested
        // for rather than jumped to: a nine-way table charges all nine states
        // the same six instructions, where three compares reach these three
        // in one, two and three.
        match self.pc {
            3 => {
                if let Some(cell) = s.shared.get_mut(self.shared) {
                    *cell = self.counter;
                }
                self.pc = 4;
            }
            4 => {
                if s.shared.get(self.shared).copied() != Some(self.counter) {
                    self.error = true;
                }
                self.pc = 5;
            }
            5 => {
                if self.counter >= self.expected {
                    self.pc = 6;
                } else {
                    self.counter = self.counter.wrapping_add(1);
                    self.pc = 3;
                }
            }
            _ => self.step_cycle(k, s),
        }
        Step::Continue
    }

    /// The six states either side of the counting loop, which run once per
    /// semaphore cycle rather than once per count.
    ///
    /// Out of line so the loop above neither jumps through their table nor
    /// carries their frame.
    #[inline(never)]
    fn step_cycle<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) {
        match self.pc {
            // portENTER_CRITICAL(); sCheckVariableToUse = sNextCheckVariable;
            // sNextCheckVariable++; portEXIT_CRITICAL();
            0 => self.claim_slot(k, s),
            // if( xSemaphoreTake( xSemaphore, xBlockTime ) == pdPASS )
            1 => self.take(k),
            // if( *pulSharedVariable != ulExpectedValue ) { sError = pdTRUE; }
            2 => {
                if s.shared.get(self.shared).copied() != Some(self.expected) {
                    self.error = true;
                }
                self.counter = 0;
                self.pc = 3;
            }
            // if( xSemaphoreGive( xSemaphore ) == pdFALSE ) { sError = pdTRUE; }
            6 => self.give(k),
            // if( sError == pdFALSE ) { sCheckVariables[ sCheckVariableToUse ]++; }
            7 => {
                if !self.error && self.check_slot < NUM_TASKS {
                    if let Some(cell) = s.check.get_mut(self.check_slot) {
                        *cell = cell.wrapping_add(1);
                    }
                }
                self.pc = 8;
            }
            // if( xBlockTime != 0 ) { vTaskDelay( xBlockTime * semtstDELAY_FACTOR ); }
            _ => self.wait_out(k),
        }
    }

    /// `portENTER_CRITICAL(); sCheckVariableToUse = sNextCheckVariable;
    /// sNextCheckVariable++; portEXIT_CRITICAL();`
    ///
    /// Out of line, with its three siblings, so that the counting loop this
    /// body spends nearly all its steps in does not carry a frame sized for
    /// a kernel call it never makes.
    #[inline(never)]
    fn claim_slot<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut State) {
        k.enter_critical();
        self.check_slot = usize::try_from(s.next_check).unwrap_or(0);
        s.next_check = s.next_check.wrapping_add(1);
        k.exit_critical();
        self.pc = 1;
    }

    /// `if( xSemaphoreTake( xSemaphore, xBlockTime ) == pdPASS )`
    #[inline(never)]
    fn take<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) {
        match k.semaphore_take(self.semaphore, self.block_time) {
            Ok(Wait::Ready(())) => self.pc = 2,
            Ok(Wait::Blocked) => {}
            // The take timed out. A polling task yields; a blocking one
            // simply tries again.
            Err(_) => {
                if self.block_time == 0 {
                    k.task_yield();
                }
            }
        }
    }

    /// `if( xSemaphoreGive( xSemaphore ) == pdFALSE ) { sError = pdTRUE; }`
    #[inline(never)]
    fn give<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) {
        if !matches!(k.semaphore_give(self.semaphore), Ok(Wait::Ready(()))) {
            self.error = true;
        }
        self.pc = 7;
    }

    /// `if( xBlockTime != 0 ) { vTaskDelay( xBlockTime * semtstDELAY_FACTOR ); }`
    #[inline(never)]
    fn wait_out<W: fmt::Write>(&mut self, k: &mut SimKernel<W>) {
        if self.block_time != 0 {
            let _ = k.delay(self.block_time.saturating_mul(DELAY_FACTOR));
        }
        self.pc = 1;
    }
}

/// `vStartSemaphoreTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let mut made: [Option<(rusty_rtos_core::handle::TaskHandle, Body)>; 4] =
        [None, None, None, None];
    {
        let mut k = runner.kernel_mut();

        // The first semaphore: created empty, then given once so the first
        // taker succeeds. Its two tasks poll with no block time.
        let sem1 = k.semaphore_create_binary()?;
        let _ = k.semaphore_give(sem1)?;
        let pol1 = k.create_task("PolSEM1", 0)?;
        let pol2 = k.create_task("PolSEM2", 0)?;
        let polling = Body {
            semaphore: sem1,
            shared: 0,
            block_time: 0,
            expected: NON_BLOCKING_EXPECTED,
            ..Body::default()
        };
        made[0] = Some((pol1, polling));
        made[1] = Some((pol2, polling));

        // The second: its two tasks block, and run at the harness's
        // priority so they starve the polling pair — which is the point.
        let sem2 = k.semaphore_create_binary()?;
        let _ = k.semaphore_give(sem2)?;
        let blk1 = k.create_task("BlkSEM1", PRIORITY)?;
        let blk2 = k.create_task("BlkSEM2", PRIORITY)?;
        let blocking = Body {
            semaphore: sem2,
            shared: 1,
            block_time: BLOCK_TIME,
            expected: BLOCKING_EXPECTED,
            ..Body::default()
        };
        made[2] = Some((blk1, blocking));
        made[3] = Some((blk2, blocking));
    }
    // The C seeds each shared variable with the value the first taker
    // expects to find (semtest.c lines 109 and 141); zeroing them would
    // latch `sError` on the first pass and the scenario would never pass
    // its own check, however perfect the kernel.
    runner.shared_mut().state = runner::State::SemTest(State {
        shared: [NON_BLOCKING_EXPECTED, BLOCKING_EXPECTED],
        ..State::default()
    });
    runner.start_common(max_ticks)?;
    for (task, body) in made.into_iter().flatten() {
        runner.attach(task, runner::Body::SemTest(body));
    }
    Ok(())
}
