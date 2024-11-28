use core::{cell::RefCell, sync::atomic::AtomicBool};

use critical_section::Mutex;

struct State {
    /// Time since the CPU utilization started gathering, in ticks.
    pub start: u64,
    /// Last time the CPU went to sleep, in ticks.
    ///
    /// This is used to calculate the time spent sleeping.
    pub last: u64,
    /// Total time spent in WFE, in ticks.
    ///
    /// This is the sum of all the time the CPU spent sleeping, waiting for an event.
    ///
    /// This is a 64-bit value, because it can overflow quickly on 32-bit systems.
    pub total: u64,
}

pub struct CpuUtil {
    was_low_power: AtomicBool,
    state: Mutex<RefCell<State>>,
}

impl CpuUtil {
    pub(crate) const fn new() -> Self {
        Self {
            was_low_power: AtomicBool::new(false),
            state: Mutex::new(RefCell::new(State{
                start: 0,
                last: 0,
                total: 0,
            }))
        }
    }

    /// Get the CPU utilization (from 0.0 to 1.0) across both thread-mode and
    /// interrupt-mode tasks. A higher utilization means the CPU is busier.
    pub fn utilization(&self) -> f32 {
        let (start_diff, total) = critical_section::with(|cs| {
            let mut state = self.state.borrow_ref_mut(cs);
            let time_now = embassy_time_driver::now();

            // Get delta passed (delta) time and time spent in WFE (total)
            let start_diff = time_now - state.start;
            let total = state.total;

            // Reset state to current time
            state.start = time_now;
            state.last = time_now;
            state.total = 0;

            (start_diff, total)
        });

        if start_diff > 0 && total < start_diff {
            1.0 - total as f32 / start_diff as f32
        } else {
            1.0
        }
    }

    /// This is called immediately after entering the `__pender` function,
    /// to register the time we may have just spent in low-power mode.
    pub(crate) fn entering_pender(&self) {
        use core::sync::atomic::Ordering;
        let time = embassy_time_driver::now();
        if self.was_low_power.swap(false, Ordering::AcqRel) {
            critical_section::with(|cs|{
                let mut state = self.state.borrow_ref_mut(cs);

                let last_diff = time - state.last;
                state.total += last_diff;
            });
        }
    }

    /// This is called immediately before entering low-power mode, to note current time.
    pub(crate) fn before_low_power(&self) {
        use core::sync::atomic::Ordering;
        critical_section::with(|cs|{
            self.was_low_power.store(true, Ordering::Release);
            let mut state = self.state.borrow_ref_mut(cs);
            state.last = embassy_time_driver::now();
        });
    }
}
