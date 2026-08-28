// SPDX-License-Identifier: LGPL-2.1-or-later
//! End-to-end benchmark for locally supplied PSF and PSF2 modules.

#![allow(clippy::cast_precision_loss)]

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use upse::{AudioAction, PlayerBuilder};

const DEFAULT_SECONDS: u64 = 10;
const DEFAULT_WARMUP_SECONDS: u64 = 1;
const DEFAULT_RUNS: usize = 3;
const DEFAULT_QUANTUM: usize = 1024;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

struct Config {
    cargo_bench: bool,
    seconds: u64,
    warmup_seconds: u64,
    runs: usize,
    quantum: usize,
    paths: Vec<PathBuf>,
}

struct Measurement {
    open: Duration,
    render: Duration,
    frames: u64,
    sample_rate: u32,
    checksum: u64,
}

fn parse_count(name: &str, value: Option<String>, allow_zero: bool) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{name} requires a value"))?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name} value: {value}"))?;
    if !allow_zero && parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut config = Config {
        cargo_bench: false,
        seconds: DEFAULT_SECONDS,
        warmup_seconds: DEFAULT_WARMUP_SECONDS,
        runs: DEFAULT_RUNS,
        quantum: DEFAULT_QUANTUM,
        paths: Vec::new(),
    };
    let mut positional = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--bench" {
            config.cargo_bench = true;
            continue;
        }
        if positional {
            config.paths.push(PathBuf::from(argument));
            continue;
        }
        match argument.as_str() {
            "-h" | "--help" => {
                usage();
                return Ok(None);
            }
            "--" => positional = true,
            "--seconds" => config.seconds = parse_count("--seconds", args.next(), false)?,
            "--warmup-seconds" => {
                config.warmup_seconds = parse_count("--warmup-seconds", args.next(), true)?;
            }
            "--runs" => {
                let runs = parse_count("--runs", args.next(), false)?;
                config.runs =
                    usize::try_from(runs).map_err(|_| format!("--runs is too large: {runs}"))?;
            }
            "--quantum" => {
                let quantum = parse_count("--quantum", args.next(), false)?;
                config.quantum = usize::try_from(quantum)
                    .map_err(|_| format!("--quantum is too large: {quantum}"))?;
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unknown argument: {argument}"));
            }
            _ => config.paths.push(PathBuf::from(argument)),
        }
    }
    Ok(Some(config))
}

fn usage() {
    println!(
        "usage: cargo bench -p upse --bench render -- [OPTIONS] PSF...\n\
         options:\n\
           --seconds COUNT         measured emulated seconds (default: 10)\n\
           --warmup-seconds COUNT  unmeasured emulated seconds (default: 1)\n\
           --runs COUNT            independent runs per module (default: 3)\n\
           --quantum COUNT         callback frames per block (default: 1024)"
    );
}

fn benchmark(path: &Path, config: &Config) -> Result<Measurement, Box<dyn Error>> {
    let open_start = Instant::now();
    let mut player = PlayerBuilder::new()
        .callback_quantum(config.quantum)
        .open_path(path)?;
    let open = open_start.elapsed();
    let sample_rate = u64::from(player.audio_format().sample_rate());
    let warmup_frames = config
        .warmup_seconds
        .checked_mul(sample_rate)
        .ok_or("warmup frame count overflow")?;
    let warmup = player.render(warmup_frames)?;
    if warmup.frames() != warmup_frames {
        return Err(format!(
            "module ended after {} of {warmup_frames} warmup frames",
            warmup.frames()
        )
        .into());
    }

    let checksum = Arc::new(AtomicU64::new(FNV_OFFSET));
    let callback_checksum = Arc::clone(&checksum);
    player.set_callback(move |block| {
        let mut value = callback_checksum.load(Ordering::Relaxed);
        for sample in block.samples() {
            value ^= u64::from(sample.to_bits());
            value = value.wrapping_mul(FNV_PRIME);
        }
        callback_checksum.store(value, Ordering::Relaxed);
        AudioAction::Continue
    });

    let requested = config
        .seconds
        .checked_mul(sample_rate)
        .ok_or("measured frame count overflow")?;
    let render_start = Instant::now();
    let outcome = player.render(requested)?;
    let render = render_start.elapsed();
    let frames = outcome.frames();
    if frames == 0 {
        return Err("module ended before the measured interval".into());
    }
    Ok(Measurement {
        open,
        render,
        frames,
        sample_rate: player.audio_format().sample_rate(),
        checksum: checksum.load(Ordering::Relaxed),
    })
}

fn run() -> Result<(), Box<dyn Error>> {
    let Some(config) = parse_args()? else {
        return Ok(());
    };
    if config.paths.is_empty() {
        if config.cargo_bench {
            usage();
        }
        return Ok(());
    }
    for path in &config.paths {
        println!("{}", path.display());
        let mut samples = Vec::with_capacity(config.runs);
        let mut expected_checksum = None;
        for run in 1..=config.runs {
            let sample = benchmark(path, &config)?;
            if expected_checksum
                .replace(sample.checksum)
                .is_some_and(|value| value != sample.checksum)
            {
                return Err("render checksum changed between runs".into());
            }
            let emulated = sample.frames as f64 / f64::from(sample.sample_rate);
            let realtime = emulated / sample.render.as_secs_f64();
            println!(
                "run {run}: open {:.6} s, render {:.6} s, {realtime:.3}x realtime",
                sample.open.as_secs_f64(),
                sample.render.as_secs_f64()
            );
            samples.push(sample);
        }
        samples.sort_unstable_by_key(|sample| sample.render);
        let median = &samples[samples.len() / 2];
        let emulated = median.frames as f64 / f64::from(median.sample_rate);
        println!(
            "median render: {:.6} s, {:.3}x realtime",
            median.render.as_secs_f64(),
            emulated / median.render.as_secs_f64()
        );
        println!("checksum: {:#018x}", median.checksum);
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("render benchmark: {error}");
            ExitCode::FAILURE
        }
    }
}
