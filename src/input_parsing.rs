use std::hint;

use crate::scratch::Scratch;

pub const AXIS_COUNT: usize = libc::ABS_CNT;
const EV_SYN: u16 = 0;
const SYN_DROPPED: u16 = 3;
pub const EV_ABS: u16 = 3;

#[cfg(feature = "branchless")]
const LAST_VALUES_COUNT: usize = AXIS_COUNT + 1;
#[cfg(not(feature = "branchless"))]
const LAST_VALUES_COUNT: usize = AXIS_COUNT;

#[derive(Clone, Copy, Default)]
pub struct DeviceCapabilities {
    pub is_pointer: bool,
    pub is_accelerometer: bool,
    pub has_abs: bool,
}

pub struct Axes {
    pub last_values: [i32; LAST_VALUES_COUNT],
    pub midpoints: [i32; AXIS_COUNT],
    pub deadzones: [u32; AXIS_COUNT],
}

impl Axes {
    pub fn from_min_max(min_max: &[(i32, i32); AXIS_COUNT]) -> Self {
        let mut midpoints = [0; AXIS_COUNT];
        let mut deadzones = [0; AXIS_COUNT];
        for i in 0..AXIS_COUNT {
            let (minimum, maximum) = min_max[i];
            let span = maximum - minimum;
            let midpoint = minimum + span / 2;
            let deadzone = span / 10;
            midpoints[i] = midpoint;
            deadzones[i] = u32::try_from(deadzone).expect("axis deadzone must be non-negative");
        }
        Self {
            last_values: [i32::MIN; LAST_VALUES_COUNT],
            midpoints,
            deadzones,
        }
    }
}

pub struct BatchOutcome<'a> {
    pub timestamps_us: &'a [i64],
    pub overflowed: bool,
    pub is_active_indefinitely: bool,
}

pub trait ProcessorState<const HAS_ABS: bool> {
    type State;

    #[inline]
    fn handle_abs(_state: &mut Self::State, _ev: &libc::input_event) -> bool {
        false
    }

    #[inline]
    fn process_event_specialized(
        ev: &libc::input_event,
        state: &mut Self::State,
        prev_ts_us: &mut i64,
        overflowed: &mut bool,
    ) -> (bool, i64) {
        let ts_us = ev.time.tv_sec * 1_000_000 + ev.time.tv_usec;
        let is_ev_syn = ev.type_ == EV_SYN;
        let is_ev_abs = ev.type_ == EV_ABS;

        let mut keep = if cfg!(feature = "branchless") {
            let is_abs_keep = if HAS_ABS {
                Self::handle_abs(state, ev)
            } else {
                false
            };
            hint::select_unpredictable(
                is_ev_syn,
                false,
                hint::select_unpredictable(HAS_ABS && is_ev_abs, is_abs_keep, true),
            )
        } else if is_ev_syn {
            false
        } else if HAS_ABS && is_ev_abs {
            Self::handle_abs(state, ev)
        } else {
            true
        };

        *overflowed |= is_ev_syn && ev.code == SYN_DROPPED;
        keep &= ts_us != *prev_ts_us;
        *prev_ts_us = ts_us;

        (keep, ts_us)
    }

    fn is_active_indefinitely(state: &Self::State) -> bool;

    /// # Safety
    ///
    /// See [Scratch::process_input_events]
    unsafe fn process_events<'a>(
        scratch: &'a mut Scratch,
        events: &[libc::input_event],
        state: &mut Self::State,
    ) -> BatchOutcome<'a> {
        let mut prev_ts_us = i64::MIN;
        let mut overflowed = false;
        let timestamps_us = unsafe {
            scratch.process_input_events(events, |ev| {
                Self::process_event_specialized(ev, state, &mut prev_ts_us, &mut overflowed)
            })
        };

        BatchOutcome {
            timestamps_us,
            overflowed,
            is_active_indefinitely: Self::is_active_indefinitely(state),
        }
    }

    /// # Safety
    ///
    /// See [Scratch::process]
    unsafe fn process_batch<'a>(
        scratch: &'a mut Scratch,
        read: usize,
        state: &mut Self::State,
    ) -> BatchOutcome<'a> {
        let mut prev_ts_us = i64::MIN;
        let mut overflowed = false;
        let timestamps_us = unsafe {
            scratch.process(read, |ev| {
                Self::process_event_specialized(ev, state, &mut prev_ts_us, &mut overflowed)
            })
        };

        BatchOutcome {
            timestamps_us,
            overflowed,
            is_active_indefinitely: Self::is_active_indefinitely(state),
        }
    }
}

pub struct Processor;

impl ProcessorState<true> for Processor {
    type State = (Axes, bool);

    #[inline]
    fn handle_abs(state: &mut Self::State, ev: &libc::input_event) -> bool {
        let (axes, _) = state;
        let abs_id = usize::from(ev.code % AXIS_COUNT as u16);
        let abs_midpoint = axes.midpoints[abs_id];
        let abs_deadzone = axes.deadzones[abs_id];

        if cfg!(feature = "branchless") {
            let slot =
                hint::select_unpredictable(ev.type_ == EV_ABS, abs_id, LAST_VALUES_COUNT - 1);
            axes.last_values[slot] = ev.value;
        } else {
            axes.last_values[abs_id] = ev.value;
        }

        ev.value.abs_diff(abs_midpoint) >= abs_deadzone
    }

    #[inline]
    fn is_active_indefinitely(state: &Self::State) -> bool {
        let (axes, is_pointer) = state;
        !is_pointer
            && (0..AXIS_COUNT).any(|ix| {
                axes.last_values[ix] != i32::MIN
                    && axes.last_values[ix].abs_diff(axes.midpoints[ix]) >= axes.deadzones[ix]
            })
    }
}

impl ProcessorState<false> for Processor {
    type State = ();

    #[inline]
    fn is_active_indefinitely(_state: &Self::State) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    fn input_event(ts_us: i64, type_: u16, code: u16, value: i32) -> libc::input_event {
        libc::input_event {
            time: libc::timeval {
                tv_sec: ts_us / 1_000_000,
                tv_usec: ts_us % 1_000_000,
            },
            type_,
            code,
            value,
        }
    }

    fn ensure_scratch_event_len(scratch: &mut Scratch, events: usize) {
        let event_size = size_of::<libc::input_event>();
        while scratch.read_buffer_len() / event_size < events {
            let n = scratch.read_buffer_len();
            unsafe {
                scratch.process(n, |_| (false, 0));
            }
        }
    }

    #[test]
    fn process_filled_read_without_abs_reports_overflow_only() {
        let scratch = Scratch::new();
        let mut scratch = scratch.borrow_mut();
        let events = vec![
            input_event(100, EV_SYN, SYN_DROPPED, 0),
            input_event(101, 1, 30, 1),
        ];
        ensure_scratch_event_len(&mut scratch, events.len());

        let mut state = ();
        let outcome = unsafe {
            <Processor as ProcessorState<false>>::process_events(&mut scratch, &events, &mut state)
        };

        assert_eq!(outcome.timestamps_us, &[101]);
        assert!(outcome.overflowed);
        assert!(!outcome.is_active_indefinitely);
    }

    #[test]
    fn process_filled_read_with_abs_reports_activity() {
        let scratch = Scratch::new();
        let mut scratch = scratch.borrow_mut();
        let events = vec![input_event(200, EV_ABS, 0, 10)];
        ensure_scratch_event_len(&mut scratch, events.len());

        let min_max = [(-10, 10); AXIS_COUNT];
        let mut state = (Axes::from_min_max(&min_max), false);
        let outcome = unsafe {
            <Processor as ProcessorState<true>>::process_events(&mut scratch, &events, &mut state)
        };

        assert_eq!(outcome.timestamps_us, &[200]);
        assert!(!outcome.overflowed);
        assert!(outcome.is_active_indefinitely);
    }

    #[test]
    fn process_filled_read_dedups_equal_timestamps() {
        let scratch = Scratch::new();
        let mut scratch = scratch.borrow_mut();
        let events = vec![input_event(300, 1, 31, 1), input_event(300, 1, 31, 0)];
        ensure_scratch_event_len(&mut scratch, events.len());

        let mut state = ();
        let outcome = unsafe {
            <Processor as ProcessorState<false>>::process_events(&mut scratch, &events, &mut state)
        };

        assert_eq!(outcome.timestamps_us, &[300]);
        assert!(!outcome.overflowed);
        assert!(!outcome.is_active_indefinitely);
    }
}
