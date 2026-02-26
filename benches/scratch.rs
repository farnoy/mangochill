use std::cell::RefCell;
use std::hint::black_box;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mangochill::{
    input_parsing::{self, AXIS_COUNT, Axes, Buttons, DeviceCapabilities},
    scratch::Scratch,
};

const EVENT_SIZE: usize = size_of::<libc::input_event>();
const MAX_BATCH_EVENTS: usize = 128;
const MODE_NAME: &str = if cfg!(feature = "branchless") {
    "branchless"
} else {
    "default"
};

struct Fixture {
    name: String,
    caps: DeviceCapabilities,
    raw: Vec<libc::input_event>,
}

fn parse_caps(stem: &str) -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    let mut parts: Vec<&str> = stem.split('-').collect();
    while let Some(part) = parts.pop() {
        match part {
            "trackpad" => caps.is_pointer = true,
            "accelerometer" => caps.is_accelerometer = true,
            "evabs" => caps.has_abs = true,
            "evkey" => caps.has_key = true,
            _ => break,
        }
    }
    caps
}

fn load_fixtures() -> Vec<Fixture> {
    let data_dir = Path::new("benches/data");
    let mut files: Vec<Fixture> = std::fs::read_dir(data_dir)
        .expect("failed to read benches/data")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let fname = path.file_name()?.to_str()?;
            if !fname.ends_with(".bin.zstd") {
                return None;
            }
            let name = fname.strip_suffix(".bin.zstd").unwrap().to_string();
            let caps = parse_caps(&name);
            if caps.is_accelerometer {
                return None;
            }
            let output = Command::new("zstdcat")
                .arg(&path)
                .output()
                .expect("failed to run zstdcat");
            assert!(
                output.status.success(),
                "zstdcat failed for {}",
                path.display()
            );
            Some(Fixture {
                name,
                caps,
                raw: decode_events(output.stdout),
            })
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    files
}

fn decode_events(bytes: Vec<u8>) -> Vec<libc::input_event> {
    assert!(
        bytes.len().is_multiple_of(EVENT_SIZE),
        "fixture length must be a multiple of input_event size"
    );
    let n_events = bytes.len() / EVENT_SIZE;
    let mut events = Vec::<libc::input_event>::with_capacity(n_events);
    unsafe {
        events.set_len(n_events);
        let dst = std::slice::from_raw_parts_mut(events.as_mut_ptr().cast::<u8>(), bytes.len());
        dst.copy_from_slice(&bytes);
    }
    events
}

fn default_axes() -> Axes {
    let min_max = [(-32_768, 32_767); AXIS_COUNT];
    Axes::from_min_max(&min_max)
}

fn make_warm_scratch(batch_events: usize) -> Rc<RefCell<Scratch>> {
    let scratch = Scratch::new();
    let batch = batch_events * EVENT_SIZE;
    {
        let mut s = scratch.borrow_mut();
        while s.read_buffer_len() < batch {
            let n = s.read_buffer_len();
            unsafe {
                s.process(n, |_| (false, 0));
            }
        }
    }
    scratch
}

trait BenchProcessor {
    unsafe fn process_events<'a>(
        &mut self,
        scratch: &'a mut Scratch,
        events: &[libc::input_event],
    ) -> input_parsing::BatchOutcome<'a>;
}

struct BenchProcessorImpl<const HAS_ABS: bool, const HAS_KEY: bool>
where
    input_parsing::Processor: input_parsing::ProcessorState<HAS_ABS, HAS_KEY>,
{
    state: <input_parsing::Processor as input_parsing::ProcessorState<HAS_ABS, HAS_KEY>>::State,
}

impl<const HAS_ABS: bool, const HAS_KEY: bool> BenchProcessor
    for BenchProcessorImpl<HAS_ABS, HAS_KEY>
where
    input_parsing::Processor: input_parsing::ProcessorState<HAS_ABS, HAS_KEY>,
{
    unsafe fn process_events<'a>(
        &mut self,
        scratch: &'a mut Scratch,
        events: &[libc::input_event],
    ) -> input_parsing::BatchOutcome<'a> {
        unsafe {
            <input_parsing::Processor as input_parsing::ProcessorState<
                HAS_ABS,
                HAS_KEY,
            >>::process_events(scratch, events, &mut self.state)
        }
    }
}

fn make_boxed_processor(caps: &DeviceCapabilities) -> Box<dyn BenchProcessor> {
    match (caps.has_abs, caps.has_key) {
        (true, true) => Box::new(BenchProcessorImpl::<true, true> {
            state: (default_axes(), Buttons::default(), caps.is_pointer),
        }),
        (true, false) => Box::new(BenchProcessorImpl::<true, false> {
            state: (default_axes(), caps.is_pointer),
        }),
        (false, true) => Box::new(BenchProcessorImpl::<false, true> {
            state: Buttons::default(),
        }),
        (false, false) => Box::new(BenchProcessorImpl::<false, false> { state: () }),
    }
}

unsafe fn process_chunk(
    s: &mut Scratch,
    processor: &mut dyn BenchProcessor,
    chunk: &[libc::input_event],
) -> (usize, bool, bool) {
    unsafe {
        let outcome = processor.process_events(s, chunk);
        (
            outcome.timestamps_us.len(),
            outcome.overflowed,
            outcome.is_active_indefinitely,
        )
    }
}

fn bench_process(c: &mut Criterion) {
    let fixtures = load_fixtures();

    let per_file_batches = [2usize, 8, 32, 128];

    for fixture in &fixtures {
        let usable = fixture.raw.len() / MAX_BATCH_EVENTS * MAX_BATCH_EVENTS;
        let raw = &fixture.raw[..usable];
        let total_events = usable;

        let mut group = c.benchmark_group(format!("process/{}", fixture.name));
        group.throughput(Throughput::Elements(total_events as u64));

        for &batch in &per_file_batches {
            group.bench_with_input(BenchmarkId::new(MODE_NAME, batch), &batch, |b, &batch| {
                let scratch = make_warm_scratch(batch);
                let mut processor = make_boxed_processor(&fixture.caps);
                b.iter(|| {
                    let mut s = scratch.borrow_mut();
                    let mut total = 0usize;
                    let mut overflowed = false;
                    let mut is_active_indefinitely = false;
                    for chunk in raw.chunks_exact(batch) {
                        let (count, local_overflowed, local_active) =
                            unsafe { process_chunk(&mut s, processor.as_mut(), chunk) };
                        total += count;
                        overflowed |= local_overflowed;
                        is_active_indefinitely |= local_active;
                    }
                    black_box((total, overflowed, is_active_indefinitely))
                });
            });
        }
        group.finish();
    }

    // Interleaved benchmark: cycles through batches from all files to simulate mixed workloads
    let interleaved_batches = [2usize, 8, 32];

    let largest_usable = fixtures
        .iter()
        .map(|fixture| fixture.raw.len() / MAX_BATCH_EVENTS * MAX_BATCH_EVENTS)
        .max()
        .unwrap_or(0);

    let trimmed: Vec<(&Fixture, &[libc::input_event])> = fixtures
        .iter()
        .map(|fixture| {
            let usable = fixture.raw.len() / MAX_BATCH_EVENTS * MAX_BATCH_EVENTS;
            (fixture, &fixture.raw[..usable])
        })
        .collect();

    let num_files = trimmed.len();
    let mut group = c.benchmark_group("process/interleaved");

    for &batch in &interleaved_batches {
        let n_batches_per_file = largest_usable / batch;
        let total_events = n_batches_per_file * num_files * batch;

        group.throughput(Throughput::Elements(total_events as u64));

        group.bench_with_input(BenchmarkId::new(MODE_NAME, batch), &batch, |b, &batch| {
            let scratch = make_warm_scratch(batch);
            let mut processors: Vec<Box<dyn BenchProcessor>> = trimmed
                .iter()
                .map(|(fixture, _)| make_boxed_processor(&fixture.caps))
                .collect();
            b.iter(|| {
                let mut s = scratch.borrow_mut();
                let mut total = 0usize;
                let mut overflowed = false;
                let mut is_active_indefinitely = false;
                for batch_idx in 0..n_batches_per_file {
                    for (fixture_ix, (_fixture, file_data)) in trimmed.iter().enumerate() {
                        let file_n_chunks = file_data.len() / batch;
                        let chunk_idx = batch_idx % file_n_chunks;
                        let chunk = &file_data[chunk_idx * batch..(chunk_idx + 1) * batch];
                        let (count, local_overflowed, local_active) = unsafe {
                            process_chunk(&mut s, processors[fixture_ix].as_mut(), chunk)
                        };
                        total += count;
                        overflowed |= local_overflowed;
                        is_active_indefinitely |= local_active;
                    }
                }
                black_box((total, overflowed, is_active_indefinitely))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_process);
criterion_main!(benches);
