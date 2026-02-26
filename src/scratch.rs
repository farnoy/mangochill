use std::{cell::RefCell, hint::select_unpredictable, rc::Rc};

use log::info;

pub struct Scratch {
    pub(crate) read_buf: Vec<libc::input_event>,
    pub(crate) timestamps_us: Vec<i64>,
}

pub const SCRATCH_INITIAL_SIZE: usize = if cfg!(test) { 1 } else { 32 };

impl Scratch {
    pub fn new() -> Rc<RefCell<Self>> {
        let mut read_buf = Vec::with_capacity(SCRATCH_INITIAL_SIZE);
        let mut timestamps_us = Vec::with_capacity(SCRATCH_INITIAL_SIZE);
        unsafe {
            read_buf.set_len(SCRATCH_INITIAL_SIZE);
            timestamps_us.set_len(SCRATCH_INITIAL_SIZE);
        }
        let s = Self {
            read_buf,
            timestamps_us,
        };
        s.invariant();
        Rc::new(RefCell::new(s))
    }

    /// # Safety
    ///
    /// The caller must ensure that every element in the slice it reads has been
    /// written to previously.
    #[inline]
    pub unsafe fn read_buffer_mut(&mut self) -> &mut [u8] {
        self.invariant();
        let (l, s, r) = unsafe { self.read_buf.align_to_mut::<u8>() };
        // PERF: confirmed asm output, it compiles away. aligning to 1 byte is infallible
        assert!(l.is_empty() && r.is_empty());
        &mut s[..]
    }

    #[inline]
    pub fn read_buffer_len(&self) -> usize {
        self.invariant();
        self.read_buf.len() * size_of::<libc::input_event>()
    }

    /// # Safety
    ///
    /// The caller must ensure it's filled the `0..read` range of the read buffer before
    /// calling this function.
    unsafe fn record_read(&mut self, read: usize) {
        self.invariant();
        let elements = read / size_of::<libc::input_event>();
        if elements == self.read_buf.len() {
            let _cap = self.read_buf.capacity();
            info!("scratch buffer filled, doubling to {}", elements * 2);
            self.read_buf.reserve(elements);
            self.timestamps_us.reserve(elements);
            unsafe {
                let len = self.timestamps_us.len();
                self.read_buf.set_len(len * 2);
                let len_ts = self.timestamps_us.len();
                self.timestamps_us.set_len(len_ts * 2);
            }
        }
        self.invariant();
    }

    fn invariant(&self) {
        debug_assert_eq!(self.read_buf.len(), self.timestamps_us.len());
    }

    #[inline]
    fn process_input_events_into<F>(
        timestamps_us: &mut [i64],
        input: &[libc::input_event],
        mut f: F,
    ) -> usize
    where
        F: FnMut(&libc::input_event) -> (bool, i64),
    {
        debug_assert!(
            input.len() <= timestamps_us.len(),
            "input batch exceeds scratch timestamp capacity"
        );

        let mut ix = 0;
        for ev in input {
            let (cond, val) = f(ev);
            if cfg!(feature = "branchless") {
                unsafe {
                    let b = timestamps_us.get_unchecked_mut(ix);
                    *b = val;
                }
                ix += select_unpredictable(cond, 1, 0);
            } else if cond {
                unsafe {
                    *timestamps_us.get_unchecked_mut(ix) = val;
                }
                ix += 1;
            }
        }
        ix
    }

    /// # Safety
    ///
    /// Process already-aligned input events without copying them into `self.read_buf`.
    /// The caller must ensure `input.len() <= self.timestamps_us.len()`.
    #[inline]
    pub unsafe fn process_input_events<F>(&mut self, input: &[libc::input_event], mut f: F) -> &[i64]
    where
        F: FnMut(&libc::input_event) -> (bool, i64),
    {
        self.invariant();
        let count = Self::process_input_events_into(&mut self.timestamps_us, input, &mut f);

        &self.timestamps_us[..count]
    }

    /// # Safety
    ///
    /// See [record_read] for the `read` parameter.
    #[inline]
    pub unsafe fn process<F>(&mut self, read: usize, f: F) -> &[i64]
    where
        F: FnMut(&libc::input_event) -> (bool, i64),
    {
        unsafe { self.record_read(read) };
        assert!(read.is_multiple_of(size_of::<libc::input_event>()));
        let events = &self.read_buf[..read / size_of::<libc::input_event>()];
        let count = Self::process_input_events_into(&mut self.timestamps_us, events, f);
        &self.timestamps_us[..count]
    }
}

#[cfg(test)]
mod test {
    use super::Scratch;

    fn input_event(ts: i64) -> Vec<u8> {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: ts / 1_000_000,
                tv_usec: ts % 1_000_000,
            },
            type_: 0,
            code: 0,
            value: 0,
        };
        let v = vec![ev];
        let (left, middle, right) = unsafe { v.align_to::<u8>() };
        assert!(left.is_empty() && right.is_empty());
        middle.iter().copied().collect()
    }

    #[test]
    fn scratch_full_flow() {
        let scratch = Scratch::new();
        let mut scratch = scratch.borrow_mut();
        let mut evdev = vec![];
        evdev.extend(input_event(1234));
        evdev.extend(input_event(1234));
        evdev.extend(input_event(1235));
        evdev.extend(input_event(9999));
        evdev.extend(input_event(9998));
        unsafe {
            let mut ix = 0;
            dbg!(scratch.read_buf.len());
            dbg!(scratch.read_buf.capacity());
            dbg!(scratch.timestamps_us.len());
            dbg!(scratch.timestamps_us.capacity());
            scratch.read_buffer_mut()[..24].copy_from_slice(&evdev[..24]);
            let ts = scratch.process(24, |ts| {
                assert_eq!(1234, ts.time.tv_usec, "{ix}");
                ix += 1;
                (true, ix * 100)
            });
            assert_eq!(1, ix);
            assert_eq!(ts, &[100]);
            dbg!(scratch.read_buf.len());
            dbg!(scratch.read_buf.capacity());
            dbg!(scratch.timestamps_us.len());
            dbg!(scratch.timestamps_us.capacity());
            scratch.read_buffer_mut()[..48].copy_from_slice(&evdev[24..72]);
            let ts = scratch.process(48, |ts| {
                assert_eq!(if ix == 1 { 1234 } else { 1235 }, ts.time.tv_usec, "{ix}");
                ix += 1;
                (ix == 3, ix * 10)
            });
            assert_eq!(3, ix);
            assert_eq!(ts, &[30]);
            scratch.read_buffer_mut()[..48].copy_from_slice(&evdev[72..][..2 * 24]);
            let ts = scratch.process(48, |ts| {
                assert_eq!(if ix == 3 { 9999 } else { 9998 }, ts.time.tv_usec, "{ix}");
                ix += 1;
                (ix == 4, ts.time.tv_usec)
            });
            assert_eq!(5, ix);
            assert_eq!(ts, &[9999]);
        };
        assert_eq!(1, 1);
    }

    #[test]
    #[should_panic]
    fn torn_read() {
        let scratch = Scratch::new();
        let mut scratch = scratch.borrow_mut();
        let mut evdev = vec![];
        evdev.extend(input_event(1234));
        // input_event is 24 B, let's cut off at 15
        unsafe {
            scratch.read_buffer_mut()[..15].copy_from_slice(&evdev[..15]);
            scratch.process(15, |_| (false, 0))
        };
    }
}
