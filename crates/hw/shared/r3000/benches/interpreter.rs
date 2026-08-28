// SPDX-License-Identifier: LGPL-2.1-or-later
//! Deterministic R3000 interpreter throughput benchmark.

#![allow(clippy::cast_precision_loss)]

use std::error::Error;
use std::hint::black_box;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use upse_r3000::{Bus, BusFault, Cpu, ResetProfile, StepEvent};

const DEFAULT_INSTRUCTIONS: u64 = 20_000_000;
const DEFAULT_RUNS: usize = 3;
const RAM_SIZE: usize = 4096;
const DATA_ADDRESS: usize = 0x100;

#[derive(Clone, Copy)]
struct Config {
    instructions: u64,
    runs: usize,
}

struct Ram {
    bytes: Vec<u8>,
}

impl Ram {
    fn with_program() -> Self {
        let mut ram = Self {
            bytes: vec![0; RAM_SIZE],
        };
        for (index, instruction) in program().into_iter().enumerate() {
            let address = index * size_of::<u32>();
            ram.bytes[address..address + size_of::<u32>()]
                .copy_from_slice(&instruction.to_le_bytes());
        }
        ram
    }

    fn range(&self, address: u32, size: usize) -> Result<std::ops::Range<usize>, BusFault> {
        let start = usize::try_from(address).map_err(|_| fault(address, size))?;
        let end = start
            .checked_add(size)
            .ok_or_else(|| fault(address, size))?;
        if end > self.bytes.len() {
            return Err(fault(address, size));
        }
        Ok(start..end)
    }
}

impl Bus for Ram {
    fn read_u8(&mut self, address: u32) -> Result<u8, BusFault> {
        let range = self.range(address, 1)?;
        Ok(self.bytes[range.start])
    }

    fn read_u16(&mut self, address: u32) -> Result<u16, BusFault> {
        let range = self.range(address, 2)?;
        Ok(u16::from_le_bytes(
            self.bytes[range]
                .try_into()
                .expect("validated halfword range"),
        ))
    }

    fn read_u32(&mut self, address: u32) -> Result<u32, BusFault> {
        let range = self.range(address, 4)?;
        Ok(u32::from_le_bytes(
            self.bytes[range].try_into().expect("validated word range"),
        ))
    }

    fn write_u8(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
        let range = self.range(address, 1)?;
        self.bytes[range.start] = value;
        Ok(())
    }

    fn write_u16(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        let range = self.range(address, 2)?;
        self.bytes[range].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn write_u32(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        let range = self.range(address, 4)?;
        self.bytes[range].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn interrupt_pending(&self) -> bool {
        false
    }
}

fn fault(address: u32, size: usize) -> BusFault {
    BusFault::new(format!(
        "benchmark RAM access at {address:#010x} for {size} bytes"
    ))
}

const fn special(rs: u32, rt: u32, rd: u32, shift: u32, function: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (shift << 6) | function
}

const fn immediate(opcode: u32, rs: u32, rt: u32, value: u16) -> u32 {
    (opcode << 26) | (rs << 21) | (rt << 16) | value as u32
}

const fn program() -> [u32; 15] {
    [
        immediate(0x09, 1, 1, 1),      // addiu r1, r1, 1
        special(1, 2, 2, 0, 0x26),     // xor   r2, r1, r2
        special(3, 2, 3, 0, 0x21),     // addu  r3, r3, r2
        special(0, 3, 4, 3, 0x00),     // sll   r4, r3, 3
        immediate(0x2b, 0, 4, 0x0100), // sw    r4, 0x100(r0)
        immediate(0x23, 0, 5, 0x0100), // lw    r5, 0x100(r0)
        immediate(0x09, 6, 6, 3),      // addiu r6, r6, 3
        special(5, 6, 7, 0, 0x24),     // and   r7, r5, r6
        special(7, 2, 8, 0, 0x25),     // or    r8, r7, r2
        special(8, 1, 9, 0, 0x23),     // subu  r9, r8, r1
        special(9, 3, 10, 0, 0x2b),    // sltu  r10, r9, r3
        special(4, 6, 0, 0, 0x19),     // multu r4, r6
        special(0, 0, 11, 0, 0x12),    // mflo  r11
        immediate(0x04, 0, 0, 0xfff2), // beq   r0, r0, 0
        special(11, 7, 12, 0, 0x21),   // addu  r12, r11, r7
    ]
}

fn execute(instructions: u64) -> Result<(Duration, u64, u64), Box<dyn Error>> {
    let mut cpu = black_box(Cpu::new(ResetProfile {
        pc: 0,
        exception_vector: 0,
        bootstrap_exception_vector: 0,
        status: 0,
        processor_id: 2,
    }));
    let mut ram = black_box(Ram::with_program());
    let mut cycles = 0_u64;
    let start = Instant::now();
    for _ in 0..instructions {
        let outcome = cpu.step_without_external_interrupts(&mut ram)?;
        if outcome.event != StepEvent::Instruction {
            return Err(format!("unexpected CPU event at {:#010x}", outcome.pc).into());
        }
        cycles = cycles.wrapping_add(u64::from(outcome.cycles));
    }
    let elapsed = start.elapsed();
    let mut checksum = u64::from(cpu.hi()) << 32 | u64::from(cpu.lo());
    for register in 1..=12 {
        checksum = checksum.rotate_left(5) ^ u64::from(cpu.register(register).unwrap_or(0));
    }
    let stored = u32::from_le_bytes(
        ram.bytes[DATA_ADDRESS..DATA_ADDRESS + size_of::<u32>()]
            .try_into()
            .expect("fixed data word range"),
    );
    checksum ^= u64::from(stored);
    black_box(checksum);
    Ok((elapsed, cycles, checksum))
}

fn parse_count(name: &str, value: Option<String>) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{name} requires a value"))?;
    value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name} value: {value}"))
        .and_then(|value| {
            if value == 0 {
                Err(format!("{name} must be greater than zero"))
            } else {
                Ok(value)
            }
        })
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut config = Config {
        instructions: DEFAULT_INSTRUCTIONS,
        runs: DEFAULT_RUNS,
    };
    let mut requested = false;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                usage();
                return Ok(None);
            }
            "--bench" => requested = true,
            "--instructions" => {
                requested = true;
                config.instructions = parse_count("--instructions", args.next())?;
            }
            "--runs" => {
                requested = true;
                let runs = parse_count("--runs", args.next())?;
                config.runs =
                    usize::try_from(runs).map_err(|_| format!("--runs is too large: {runs}"))?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok(requested.then_some(config))
}

fn usage() {
    println!(
        "usage: cargo bench -p upse-r3000 --bench interpreter -- \
         [--instructions COUNT] [--runs COUNT]"
    );
}

fn run() -> Result<(), Box<dyn Error>> {
    let Some(config) = parse_args()? else {
        return Ok(());
    };
    println!("R3000 interpreter benchmark");
    println!("instructions per run: {}", config.instructions);
    let mut samples = Vec::with_capacity(config.runs);
    let mut expected_checksum = None;
    for run in 1..=config.runs {
        let (elapsed, cycles, checksum) = execute(config.instructions)?;
        if expected_checksum
            .replace(checksum)
            .is_some_and(|value| value != checksum)
        {
            return Err("benchmark checksum changed between runs".into());
        }
        let seconds = elapsed.as_secs_f64();
        println!(
            "run {run}: {seconds:.6} s, {:.3} MIPS, {:.3} Mcycles/s",
            config.instructions as f64 / seconds / 1_000_000.0,
            cycles as f64 / seconds / 1_000_000.0
        );
        samples.push(elapsed);
    }
    samples.sort_unstable();
    let median = samples[samples.len() / 2].as_secs_f64();
    println!(
        "median: {median:.6} s, {:.3} MIPS",
        config.instructions as f64 / median / 1_000_000.0
    );
    println!("checksum: {:#018x}", expected_checksum.unwrap_or(0));
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("interpreter benchmark: {error}");
            usage();
            ExitCode::FAILURE
        }
    }
}
