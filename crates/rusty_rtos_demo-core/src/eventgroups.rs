//! `EventGroupsDemo` — four tasks around one event group, and an interrupt
//! around another.
//!
//! The C original is `FreeRTOS/Demo/Common/Minimal/EventGroupsDemo.c`. A
//! master task at the *lowest* priority drives three higher-priority tasks
//! through three tests, and the only reason it can drive them at all is
//! that each of the three blocks or suspends itself the moment it is let
//! go: the master runs exactly when nobody else can, so every
//! `eTaskGetState` it makes is a statement about what the kernel did with
//! the call before it.
//!
//! The three tests are:
//!
//! - **selective bits**: two tasks wait on overlapping sets with
//!   wait-for-any, and each of the eight bits is set in turn. A task must
//!   wake for a bit in its own set and stay blocked for one that is not.
//! - **bit combinations**: one task waits for all of three bits, and the
//!   master sets them in an order that leaves the waiter blocked until the
//!   last one arrives — while checking what the group reads back at each
//!   step, including that a waiter's clear-on-exit really cleared.
//! - **task sync**: all four tasks rendezvous through `xEventGroupSync`,
//!   which is `xEventGroupSetBits` and `xEventGroupWaitBits` in one so that
//!   no task can set its own bit and then be preempted before it waits.
//!
//! The interrupt half is separate on purpose: `xEventGroupSetBitsFromISR`
//! cannot walk the waiting list from interrupt context, so it hands the job
//! to the timer daemon, and the demo checks that the bits arrive.
//!
//! One `step` per C statement; each `pc` arm names the line it stands for.

use core::fmt;

use rusty_rtos_core::error::Result;
use rusty_rtos_core::handle::{EventGroupHandle, TaskHandle};
use rusty_rtos_kernel::kernel::TaskState;
use rusty_rtos_kernel::queue::Wait;

use crate::runner::{self, Runner, Shared, SimKernel, Step, TickIsr};

/// `ebSET_BIT_TASK_PRIORITY`: `tskIDLE_PRIORITY`. The master is the lowest
/// priority task in the system, which is what makes the test work.
pub const SET_BIT_TASK_PRIORITY: u8 = 0;
/// `ebWAIT_BIT_TASK_PRIORITY`.
pub const WAIT_BIT_TASK_PRIORITY: u8 = 1;

/// `ebBIT_1`.
const BIT_1: u32 = 0x02;
/// `ebCOMBINED_BITS`: `ebBIT_1 | ebBIT_5 | ebBIT_7`.
const COMBINED_BITS: u32 = 0x02 | 0x20 | 0x80;
/// `ebALL_BITS`: the eight the demo uses.
const ALL_BITS: u32 = 0xff;

/// `ebSET_BIT_TASK_SYNC_BIT`.
const SET_BIT_TASK_SYNC_BIT: u32 = 0x01;
/// `ebWAIT_BIT_TASK_SYNC_BIT`.
const WAIT_BIT_TASK_SYNC_BIT: u32 = 0x02;
/// `ebRENDEZVOUS_TASK_1_SYNC_BIT`.
const RENDEZVOUS_TASK_1_SYNC_BIT: u32 = 0x04;
/// `ebRENDEZVOUS_TASK_2_SYNC_BIT`.
const RENDEZVOUS_TASK_2_SYNC_BIT: u32 = 0x08;
/// `ebALL_SYNC_BITS`.
const ALL_SYNC_BITS: u32 = SET_BIT_TASK_SYNC_BIT
    | WAIT_BIT_TASK_SYNC_BIT
    | RENDEZVOUS_TASK_1_SYNC_BIT
    | RENDEZVOUS_TASK_2_SYNC_BIT;

/// `ebDONT_BLOCK`.
const DONT_BLOCK: u64 = 0;

/// `ebSELECTIVE_BITS_1`: what the first rendezvous task waits for.
const SELECTIVE_BITS_1: u32 = 0x03;
/// `ebSELECTIVE_BITS_2`: what the second waits for. The two overlap in
/// `ebBIT_0`, which is the point — one bit must wake both.
const SELECTIVE_BITS_2: u32 = 0x05;

/// `uxBitsToSet` in `vPeriodicEventGroupsProcessing`.
const ISR_BITS_TO_SET: u32 = 0x12;
/// `xSetBitCount`, `xGetBitsCount`, `xClearBitsCount`.
const ISR_SET_AT: u32 = 100;
const ISR_GET_AT: u32 = 200;
const ISR_CLEAR_AT: u32 = 300;

/// `EventGroupsDemo.c`'s task-half file-scope variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct State {
    /// `xEventGroup`, which the master deletes and remakes every cycle.
    pub group: EventGroupHandle,
    /// `xTestSlaveTaskHandle`, the master's parameter.
    pub slave: TaskHandle,
    /// `xSyncTask1`.
    pub sync1: TaskHandle,
    /// `xSyncTask2`.
    pub sync2: TaskHandle,
    /// `ulTestMasterCycles`.
    pub master_cycles: u32,
    /// `ulTestSlaveCycles`.
    pub slave_cycles: u32,
    /// `ulPreviousSetBitCycles`.
    previous_master: u32,
    /// `ulPreviousWaitBitCycles`.
    previous_slave: u32,
    /// `ulPreviousISRCycles`.
    previous_isr: u32,
}

impl State {
    /// `xAreEventGroupTasksStillRunning`: all three counters must have
    /// moved since the last time this was asked.
    pub fn still_running(&mut self, isr: Isr) -> bool {
        let mut status = true;
        if self.previous_master == self.master_cycles {
            status = false;
        }
        self.previous_master = self.master_cycles;
        if self.previous_slave == self.slave_cycles {
            status = false;
        }
        self.previous_slave = self.slave_cycles;
        if self.previous_isr == isr.cycles {
            status = false;
        }
        self.previous_isr = isr.cycles;
        status
    }
}

/// `vPeriodicEventGroupsProcessing`: the half that runs in the tick.
///
/// It owns its own event group, because the two calls it makes that touch
/// a waiting list — set and clear — go to the timer daemon rather than
/// happening here, and sharing a group with the tasks would make what the
/// tasks see depend on when the daemon got round to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Isr {
    /// `xISREventGroup`.
    pub group: EventGroupHandle,
    /// `xCallCount`.
    call_count: u32,
    /// `xISRTestError`.
    error: bool,
    /// `ulISRCycles`.
    pub cycles: u32,
}

impl Isr {
    pub(crate) fn tick<W: fmt::Write>(mut self, k: &mut SimKernel<W>) -> Self {
        self.call_count = self.call_count.wrapping_add(1);
        if self.call_count == ISR_SET_AT {
            // The C's `if( uxReturned != 0x00 ) ... else if( xMessagePosted
            // != pdPASS )`: two different reasons, the same latch.
            let empty = k.event_group_bits_from_isr(self.group) == Ok(0);
            let posted = empty
                && matches!(
                    k.event_group_set_bits_from_isr(self.group, ISR_BITS_TO_SET),
                    Ok((true, _))
                );
            if !posted {
                self.error = true;
            }
        } else if self.call_count == ISR_GET_AT {
            if k.event_group_bits_from_isr(self.group) != Ok(ISR_BITS_TO_SET) {
                self.error = true;
            }
        } else if self.call_count == ISR_CLEAR_AT {
            if !matches!(
                k.event_group_clear_bits_from_isr(self.group, ISR_BITS_TO_SET),
                Ok((true, _))
            ) {
                self.error = true;
            }
            self.call_count = 0;
            if !self.error {
                self.cycles = self.cycles.wrapping_add(1);
            }
        }
        self
    }
}

/// Read the scenario's statics, or give up on this step.
macro_rules! state {
    ($s:expr) => {
        match &mut $s.state {
            runner::State::EventGroups(s) => s,
            _ => return Step::Finish(false),
        }
    };
}

/// `prvTestMasterTask`, with its three test functions flattened into one
/// program counter. The `pc` ranges name the C function each arm is in.
#[derive(Debug, Clone, Copy, Default)]
pub struct Master {
    pc: u8,
    /// `xError`, which the C declares outside the loop and never resets.
    error: bool,
    /// `uxBit`, the selective test's loop variable.
    bit: u32,
    /// `uxBits`, the value the last call answered with.
    bits: u32,
}

impl Master {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let s = state!(s);
        match self.pc {
            // xEventGroup = xEventGroupCreate();
            0 => {
                s.group = k.event_group_create().unwrap_or(EventGroupHandle::NULL);
                self.pc = 1;
            }

            // ------------------ prvSelectiveBitsTestMasterFunction --
            1 => {
                self.expect(k, s.sync1, TaskState::Suspended);
                self.pc = 2;
            }
            2 => {
                self.expect(k, s.sync2, TaskState::Suspended);
                self.bit = 0x01;
                self.pc = 3;
            }
            // for( uxBit = 0x01; uxBit < 0x100; uxBit <<= 1 )
            3 => {
                let _ = k.resume(s.sync1);
                self.pc = 4;
            }
            4 => {
                let _ = k.resume(s.sync2);
                self.pc = 5;
            }
            5 => {
                self.expect(k, s.sync1, TaskState::Blocked);
                self.pc = 6;
            }
            6 => {
                self.expect(k, s.sync2, TaskState::Blocked);
                self.pc = 7;
            }
            7 => {
                let _ = k.event_group_set_bits(s.group, self.bit);
                self.pc = 8;
            }
            // A task waiting for any of its bits wakes for one that is in
            // its set and stays blocked for one that is not.
            8 => {
                let want = if self.bit & SELECTIVE_BITS_1 == 0 {
                    TaskState::Blocked
                } else {
                    TaskState::Suspended
                };
                self.expect(k, s.sync1, want);
                self.pc = 9;
            }
            9 => {
                let want = if self.bit & SELECTIVE_BITS_2 == 0 {
                    TaskState::Blocked
                } else {
                    TaskState::Suspended
                };
                self.expect(k, s.sync2, want);
                self.bit = self.bit.wrapping_shl(1);
                self.pc = if self.bit < 0x100 { 3 } else { 10 };
            }
            10 => {
                let _ = k.resume(s.sync1);
                self.pc = 11;
            }
            11 => {
                let _ = k.resume(s.sync2);
                self.pc = 12;
            }
            12 => {
                let _ = k.event_group_delete(s.group);
                self.pc = 13;
            }

            // ---------------------------------- the for(;;) loop top --
            13 => {
                s.group = k.event_group_create().unwrap_or(EventGroupHandle::NULL);
                self.pc = 20;
            }

            // ---------------- prvBitCombinationTestMasterFunction --
            20 => {
                let _ = k.resume(s.slave);
                self.pc = 21;
            }
            21 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 22;
            }
            22 => {
                let _ = k.event_group_set_bits(s.group, COMBINED_BITS);
                self.pc = 23;
            }
            // The slave took ebBIT_1 with clear-on-exit, so what is left is
            // the other two.
            23 => {
                self.wait_bits(k, s.group, ALL_BITS, false, false, DONT_BLOCK);
                if self.bits != COMBINED_BITS & !BIT_1 {
                    self.error = true;
                }
                self.pc = 24;
            }
            24 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 25;
            }
            25 => {
                let _ = k.event_group_set_bits(s.group, ALL_BITS & !BIT_1);
                self.pc = 26;
            }
            26 => {
                self.wait_bits(k, s.group, ALL_BITS, false, false, DONT_BLOCK);
                if self.bits != ALL_BITS & !BIT_1 {
                    self.error = true;
                }
                self.pc = 27;
            }
            27 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 28;
            }
            28 => {
                let _ = k.event_group_set_bits(s.group, BIT_1);
                self.pc = 29;
            }
            29 => {
                self.expect(k, s.slave, TaskState::Suspended);
                self.pc = 30;
            }
            // Setting nothing is how the C reads the bits back from a call
            // that also unblocks waiters.
            30 => {
                if k.event_group_set_bits(s.group, 0) != Ok(ALL_BITS) {
                    self.error = true;
                }
                self.pc = 31;
            }
            31 => {
                if k.event_group_clear_bits(s.group, BIT_1) != Ok(ALL_BITS) {
                    self.error = true;
                }
                self.pc = 32;
            }
            32 => {
                let _ = k.resume(s.slave);
                self.pc = 33;
            }
            33 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 34;
            }
            34 => {
                let _ = k.event_group_set_bits(s.group, BIT_1);
                self.pc = 35;
            }
            35 => {
                self.expect(k, s.slave, TaskState::Suspended);
                self.pc = 36;
            }
            36 => {
                self.wait_bits(k, s.group, ALL_BITS, false, false, DONT_BLOCK);
                if self.bits != ALL_BITS & !COMBINED_BITS {
                    self.error = true;
                }
                self.pc = 37;
            }
            37 => {
                if k.event_group_clear_bits(s.group, ALL_BITS) != Ok(ALL_BITS & !COMBINED_BITS) {
                    self.error = true;
                }
                self.pc = 38;
            }
            38 => {
                if k.event_group_bits(s.group) != Ok(0) {
                    self.error = true;
                }
                self.pc = 40;
            }

            // -------------------------- prvPerformTaskSyncTests --
            40 => {
                self.expect(k, s.slave, TaskState::Suspended);
                self.pc = 41;
            }
            41 => {
                self.expect(k, s.sync1, TaskState::Suspended);
                self.pc = 42;
            }
            42 => {
                self.expect(k, s.sync2, TaskState::Suspended);
                self.pc = 43;
            }
            43 => {
                let _ = k.event_group_set_bits(s.group, ALL_SYNC_BITS & !SET_BIT_TASK_SYNC_BIT);
                self.pc = 44;
            }
            // A sync that waits only for itself: the condition is met the
            // moment the bit goes in, so it never blocks.
            44 => {
                self.sync(
                    k,
                    s.group,
                    SET_BIT_TASK_SYNC_BIT,
                    SET_BIT_TASK_SYNC_BIT,
                    SimKernel::<W>::MAX_DELAY,
                );
                if self.bits & SET_BIT_TASK_SYNC_BIT != SET_BIT_TASK_SYNC_BIT {
                    self.error = true;
                }
                self.pc = 45;
            }
            45 => {
                if k.event_group_bits(s.group) != Ok(ALL_SYNC_BITS & !SET_BIT_TASK_SYNC_BIT) {
                    self.error = true;
                }
                self.pc = 46;
            }
            46 => {
                let _ = k.event_group_clear_bits(s.group, ALL_SYNC_BITS & !SET_BIT_TASK_SYNC_BIT);
                self.pc = 47;
            }
            47 => {
                if k.event_group_bits(s.group) != Ok(0) {
                    self.error = true;
                }
                self.pc = 48;
            }
            48 => {
                let _ = k.resume(s.slave);
                self.pc = 49;
            }
            49 => {
                let _ = k.resume(s.sync1);
                self.pc = 50;
            }
            50 => {
                let _ = k.resume(s.sync2);
                self.pc = 51;
            }
            51 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 52;
            }
            52 => {
                self.expect(k, s.sync1, TaskState::Blocked);
                self.pc = 53;
            }
            53 => {
                self.expect(k, s.sync2, TaskState::Blocked);
                self.pc = 54;
            }
            // The real rendezvous: the master is the last of the four to
            // arrive, so this call returns without blocking and the other
            // three wake inside it.
            54 => {
                self.sync(
                    k,
                    s.group,
                    SET_BIT_TASK_SYNC_BIT,
                    ALL_SYNC_BITS,
                    SimKernel::<W>::MAX_DELAY,
                );
                if self.bits & ALL_SYNC_BITS != ALL_SYNC_BITS {
                    self.error = true;
                }
                self.pc = 55;
            }
            55 => {
                if k.event_group_bits(s.group) != Ok(0) {
                    self.error = true;
                }
                self.pc = 56;
            }
            56 => {
                self.expect(k, s.slave, TaskState::Suspended);
                self.pc = 57;
            }
            57 => {
                self.expect(k, s.sync1, TaskState::Suspended);
                self.pc = 58;
            }
            58 => {
                self.expect(k, s.sync2, TaskState::Suspended);
                self.pc = 59;
            }
            59 => {
                let _ = k.resume(s.slave);
                self.pc = 60;
            }
            60 => {
                let _ = k.resume(s.sync1);
                self.pc = 61;
            }
            61 => {
                let _ = k.resume(s.sync2);
                self.pc = 62;
            }
            62 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 63;
            }
            63 => {
                self.expect(k, s.sync1, TaskState::Blocked);
                self.pc = 64;
            }
            64 => {
                self.expect(k, s.sync2, TaskState::Blocked);
                self.pc = 65;
            }
            // The same rendezvous from above the other three, so they are
            // left *ready* rather than running when the sync completes.
            65 => {
                let _ = k.set_priority(None, WAIT_BIT_TASK_PRIORITY.saturating_add(1));
                self.pc = 66;
            }
            66 => {
                self.sync(
                    k,
                    s.group,
                    SET_BIT_TASK_SYNC_BIT,
                    ALL_SYNC_BITS,
                    SimKernel::<W>::MAX_DELAY,
                );
                if self.bits & ALL_SYNC_BITS != ALL_SYNC_BITS {
                    self.error = true;
                }
                self.pc = 67;
            }
            67 => {
                if k.event_group_bits(s.group) != Ok(0) {
                    self.error = true;
                }
                self.pc = 68;
            }
            68 => {
                self.expect(k, s.slave, TaskState::Ready);
                self.pc = 69;
            }
            69 => {
                self.expect(k, s.sync1, TaskState::Ready);
                self.pc = 70;
            }
            70 => {
                self.expect(k, s.sync2, TaskState::Ready);
                self.pc = 71;
            }
            // Dropping back lets all three run to their next suspend.
            71 => {
                let _ = k.set_priority(None, SET_BIT_TASK_PRIORITY);
                self.pc = 72;
            }
            72 => {
                self.expect(k, s.slave, TaskState::Blocked);
                self.pc = 73;
            }
            73 => {
                self.expect(k, s.sync1, TaskState::Blocked);
                self.pc = 74;
            }
            74 => {
                self.expect(k, s.sync2, TaskState::Blocked);
                self.pc = 75;
            }

            // ------------------------- back in prvTestMasterTask --
            75 => {
                let _ = k.event_group_delete(s.group);
                self.pc = 76;
            }
            76 => {
                self.expect(k, s.slave, TaskState::Suspended);
                self.pc = 77;
            }
            77 => {
                self.expect(k, s.sync1, TaskState::Suspended);
                self.pc = 78;
            }
            78 => {
                self.expect(k, s.sync2, TaskState::Suspended);
                self.pc = 79;
            }
            _ => {
                if !self.error {
                    s.master_cycles = s.master_cycles.wrapping_add(1);
                }
                self.pc = 13;
            }
        }
        Step::Continue
    }

    /// `if( eTaskGetState( x ) != y ) { xError = pdTRUE; }`, which is one
    /// critical section in the C and therefore one here.
    fn expect<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, task: TaskHandle, want: TaskState) {
        if k.task_state_get(task) != Ok(want) {
            self.error = true;
        }
    }

    /// `xEventGroupWaitBits`, keeping the answer. Only ever called here
    /// with no block time, so it cannot come back `Blocked`.
    fn wait_bits<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        wait_for: u32,
        clear_on_exit: bool,
        wait_for_all: bool,
        ticks: u64,
    ) {
        match k.event_group_wait_bits(group, wait_for, clear_on_exit, wait_for_all, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => {}
            Err(_) => self.error = true,
        }
    }

    /// `xEventGroupSync`. This one can block, and a blocked call comes back
    /// to the same `pc` — which is the C returning to the same line of the
    /// same function on the far side of the switch.
    fn sync<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        set: u32,
        wait_for: u32,
        ticks: u64,
    ) {
        match k.event_group_sync(group, set, wait_for, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => {}
            Err(_) => self.error = true,
        }
    }
}

/// `prvTestSlaveTask`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Slave {
    pc: u8,
    /// `xError`, declared outside the loop.
    error: bool,
    /// `uxReturned`.
    bits: u32,
    /// Whether the call this `pc` stands for is still waiting.
    blocked: bool,
}

impl Slave {
    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let s = state!(s);
        let max = SimKernel::<W>::MAX_DELAY;
        match self.pc {
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            1 => {
                self.wait_bits(k, s.group, BIT_1, true, true, max);
                if !self.blocked {
                    if self.bits != COMBINED_BITS {
                        self.error = true;
                    }
                    self.pc = 2;
                }
            }
            2 => {
                self.wait_bits(k, s.group, COMBINED_BITS, false, true, max);
                if !self.blocked {
                    if self.bits & COMBINED_BITS != COMBINED_BITS {
                        self.error = true;
                    }
                    self.pc = 3;
                }
            }
            3 => {
                let _ = k.suspend(None);
                self.pc = 4;
            }
            4 => {
                self.wait_bits(k, s.group, COMBINED_BITS, true, true, max);
                if !self.blocked {
                    if self.bits != ALL_BITS {
                        self.error = true;
                    }
                    self.pc = 5;
                }
            }
            5 => {
                let _ = k.suspend(None);
                self.pc = 6;
            }
            6 => {
                self.sync(k, s.group, WAIT_BIT_TASK_SYNC_BIT, ALL_SYNC_BITS, max);
                if !self.blocked {
                    if self.bits & ALL_SYNC_BITS != ALL_SYNC_BITS {
                        self.error = true;
                    }
                    self.pc = 7;
                }
            }
            7 => {
                if k.event_group_set_bits(s.group, 0) != Ok(0) {
                    self.error = true;
                }
                self.pc = 8;
            }
            8 => {
                if k.event_group_bits(s.group) != Ok(0) {
                    self.error = true;
                }
                self.pc = 9;
            }
            9 => {
                if !self.error {
                    s.slave_cycles = s.slave_cycles.wrapping_add(1);
                }
                self.pc = 10;
            }
            10 => {
                let _ = k.suspend(None);
                self.pc = 11;
            }
            11 => {
                self.sync(k, s.group, WAIT_BIT_TASK_SYNC_BIT, ALL_SYNC_BITS, max);
                if !self.blocked {
                    if self.bits & ALL_SYNC_BITS != ALL_SYNC_BITS {
                        self.error = true;
                    }
                    self.pc = 12;
                }
            }
            12 => {
                if k.event_group_set_bits(s.group, 0) != Ok(0) {
                    self.error = true;
                }
                self.pc = 13;
            }
            // This one only ever ends by the master deleting the group,
            // which wakes every waiter with a value of zero.
            13 => {
                self.wait_bits(k, s.group, ALL_SYNC_BITS, false, true, max);
                if !self.blocked {
                    if self.bits != 0 {
                        self.error = true;
                    }
                    self.pc = 14;
                }
            }
            _ => {
                if !self.error {
                    s.slave_cycles = s.slave_cycles.wrapping_add(1);
                }
                self.pc = 0;
            }
        }
        Step::Continue
    }

    fn wait_bits<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        wait_for: u32,
        clear_on_exit: bool,
        wait_for_all: bool,
        ticks: u64,
    ) {
        self.blocked = false;
        match k.event_group_wait_bits(group, wait_for, clear_on_exit, wait_for_all, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => self.blocked = true,
            Err(_) => self.error = true,
        }
    }

    fn sync<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        set: u32,
        wait_for: u32,
        ticks: u64,
    ) {
        self.blocked = false;
        match k.event_group_sync(group, set, wait_for, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => self.blocked = true,
            Err(_) => self.error = true,
        }
    }
}

/// `prvSyncTask`, both instances. Which one it is decides both the bits it
/// waits for in the selective test and the bit it contributes to the
/// rendezvous.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sync {
    pc: u8,
    /// `uxSynchronisationBit`, the task's parameter.
    sync_bit: u32,
    /// `uxPendBits`, which the C picks by comparing its own handle.
    pend_bits: u32,
    /// `uxReturned`.
    bits: u32,
    /// Whether the call this `pc` stands for is still waiting.
    blocked: bool,
}

impl Sync {
    /// The two the C creates: task 1 waits on `ebSELECTIVE_BITS_1`.
    #[must_use]
    pub const fn first() -> Self {
        Self {
            pc: 0,
            sync_bit: RENDEZVOUS_TASK_1_SYNC_BIT,
            pend_bits: SELECTIVE_BITS_1,
            bits: 0,
            blocked: false,
        }
    }

    /// And task 2 on `ebSELECTIVE_BITS_2`.
    #[must_use]
    pub const fn second() -> Self {
        Self {
            pc: 0,
            sync_bit: RENDEZVOUS_TASK_2_SYNC_BIT,
            pend_bits: SELECTIVE_BITS_2,
            bits: 0,
            blocked: false,
        }
    }

    pub(crate) fn step<W: fmt::Write>(&mut self, k: &mut SimKernel<W>, s: &mut Shared) -> Step {
        let s = state!(s);
        let max = SimKernel::<W>::MAX_DELAY;
        match self.pc {
            // ------------------- prvSelectiveBitsTestSlaveFunction --
            0 => {
                let _ = k.suspend(None);
                self.pc = 1;
            }
            // A zero answer means the group was deleted under it, which is
            // how the master ends this loop.
            1 => {
                self.wait_bits(k, s.group, self.pend_bits, true, false, max);
                if !self.blocked {
                    self.pc = if self.bits == 0 { 2 } else { 0 };
                }
            }

            // --------------------------------- the for(;;) loop top --
            2 => {
                let _ = k.suspend(None);
                self.pc = 3;
            }
            // The first sync cannot succeed — the other three tasks are not
            // there yet — and with no block time it comes straight back.
            3 => {
                self.sync(k, s.group, self.sync_bit, ALL_SYNC_BITS, DONT_BLOCK);
                self.pc = 4;
            }
            4 => {
                self.sync(k, s.group, self.sync_bit, ALL_SYNC_BITS, max);
                if !self.blocked {
                    self.pc = 5;
                }
            }
            5 => {
                let _ = k.suspend(None);
                self.pc = 6;
            }
            6 => {
                self.sync(k, s.group, self.sync_bit, ALL_SYNC_BITS, max);
                if !self.blocked {
                    self.pc = 7;
                }
            }
            _ => {
                self.wait_bits(k, s.group, ALL_SYNC_BITS, false, true, max);
                if !self.blocked {
                    self.pc = 2;
                }
            }
        }
        Step::Continue
    }

    fn wait_bits<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        wait_for: u32,
        clear_on_exit: bool,
        wait_for_all: bool,
        ticks: u64,
    ) {
        self.blocked = false;
        match k.event_group_wait_bits(group, wait_for, clear_on_exit, wait_for_all, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => self.blocked = true,
            Err(_) => self.bits = 0,
        }
    }

    fn sync<W: fmt::Write>(
        &mut self,
        k: &mut SimKernel<W>,
        group: EventGroupHandle,
        set: u32,
        wait_for: u32,
        ticks: u64,
    ) {
        self.blocked = false;
        match k.event_group_sync(group, set, wait_for, ticks) {
            Ok(Wait::Ready(bits)) => self.bits = bits,
            Ok(Wait::Blocked) => self.blocked = true,
            Err(_) => self.bits = 0,
        }
    }
}

/// `vStartEventGroupTasks`, in the C's order.
///
/// # Errors
/// As the kernel's create calls.
pub fn start<W: fmt::Write>(runner: &mut Runner<'_, W>, max_ticks: u64) -> Result<()> {
    let (slave, master, sync1, sync2, isr_group) = {
        let mut k = runner.kernel_mut();
        let slave = k.create_task("WaitO", WAIT_BIT_TASK_PRIORITY)?;
        let master = k.create_task("SetB", SET_BIT_TASK_PRIORITY)?;
        let sync1 = k.create_task("Rndv", WAIT_BIT_TASK_PRIORITY)?;
        let sync2 = k.create_task("Rndv", WAIT_BIT_TASK_PRIORITY)?;
        let isr_group = k.event_group_create()?;
        (slave, master, sync1, sync2, isr_group)
    };
    runner.shared_mut().state = runner::State::EventGroups(State {
        slave,
        sync1,
        sync2,
        ..State::default()
    });
    *runner.kernel_mut().tick_hook_mut() = TickIsr::EventGroups(Isr {
        group: isr_group,
        ..Isr::default()
    });
    runner.start_common(max_ticks)?;
    runner.attach(slave, runner::Body::EventGroupsSlave(Slave::default()));
    runner.attach(master, runner::Body::EventGroupsMaster(Master::default()));
    runner.attach(sync1, runner::Body::EventGroupsSync(Sync::first()));
    runner.attach(sync2, runner::Body::EventGroupsSync(Sync::second()));
    Ok(())
}
