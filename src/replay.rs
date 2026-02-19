use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use clap::Parser;
use mangochill::LogRecord;
use mangochill::ewm::{DeviceEwm, FpsDetail};
use serde::Serialize;

#[derive(Parser)]
#[command(
    name = "replay",
    version,
    about = "Replay recorded events through the EWM algorithm"
)]
struct Cli {
    /// NDJSON file to replay (produced by raw_export)
    #[arg(short, long)]
    input: PathBuf,

    /// Output NDJSON file (default: stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Attack (rising) half-life in milliseconds
    #[arg(long, default_value_t = 10.0)]
    attack_half_life_ms: f64,

    /// Release (falling) half-life in milliseconds
    #[arg(long, default_value_t = 100.0)]
    release_half_life_ms: f64,

    /// Minimum FPS limit
    #[arg(long, default_value_t = 45.0)]
    min_fps: f64,

    /// Maximum FPS limit
    #[arg(long, default_value_t = 115.0)]
    max_fps: f64,

    /// Tick interval in milliseconds
    #[arg(long, default_value_t = 1.0)]
    tick_ms: f64,
}

#[derive(Serialize)]
struct TickRecord {
    time_us: i64,
    #[serde(flatten)]
    detail: FpsDetail,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let file = std::fs::File::open(&cli.input)?;
    let reader = io::BufReader::new(file);

    let mut timestamps_us: Vec<i64> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let record: LogRecord = serde_json::from_str(&line)?;
        for reading in &record.readings {
            let us = reading.timestamp_micros();
            timestamps_us.push(us);
        }
    }

    timestamps_us.sort_unstable();
    timestamps_us.dedup();

    if timestamps_us.is_empty() {
        eprintln!("no events found");
        return Ok(());
    }

    let t0 = timestamps_us[0];
    for ts in &mut timestamps_us {
        *ts -= t0;
    }

    let attack_hl_us = cli.attack_half_life_ms * 1000.0;
    let release_hl_us = cli.release_half_life_ms * 1000.0;
    let mut ewm = DeviceEwm::new(attack_hl_us, release_hl_us);

    let last_event = *timestamps_us.last().unwrap();
    let tick = (cli.tick_ms * 1000.0) as i64;

    let mut out: Box<dyn Write> = match &cli.output {
        Some(path) => Box::new(io::BufWriter::new(std::fs::File::create(path)?)),
        None => Box::new(io::BufWriter::new(io::stdout().lock())),
    };

    let mut ev_idx = 0;
    let mut t = tick;
    while t <= last_event + tick {
        let start = ev_idx;
        while ev_idx < timestamps_us.len() && timestamps_us[ev_idx] <= t {
            ev_idx += 1;
        }
        if start < ev_idx {
            ewm.observe_batch(&timestamps_us[start..ev_idx]);
        }
        let detail = ewm.compute_fps_detailed(t, cli.min_fps, cli.max_fps);
        let record = TickRecord { time_us: t, detail };
        serde_json::to_writer(&mut out, &record)?;
        out.write_all(b"\n")?;
        t += tick;
    }

    out.flush()?;
    Ok(())
}
