mod benchmark;
mod progress;
mod report;
mod scenarios;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "archivefs-bench",
    version,
    about = "Inspect and benchmark archivefs archives"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show the detected format and archive contents.
    Inspect {
        /// Path to a supported archive.
        archive: PathBuf,
    },
    /// Run one or more performance scenarios.
    Bench(BenchArgs),
    /// Smoke-test a user-provided archive using the public VFS API.
    Verify {
        /// Path to a supported archive.
        archive: PathBuf,
    },
}

#[derive(Debug, Args)]
struct BenchArgs {
    /// Path to a supported archive.
    archive: PathBuf,
    /// Number of untimed warmup runs.
    #[arg(long, default_value_t = 2)]
    warmup: usize,
    /// Number of measured runs. Must be greater than zero.
    #[arg(long, default_value_t = 10)]
    iterations: usize,
    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Debug, Subcommand)]
enum Scenario {
    /// Measure archive opening and directory listing separately.
    Open,
    /// Read regular files sequentially.
    Scan {
        /// Maximum number of files to read (defaults to all files).
        #[arg(long)]
        max_entries: Option<usize>,
        /// Read buffer size in bytes.
        #[arg(long, default_value_t = 64 * 1024)]
        chunk_size: usize,
    },
    /// Measure entry-open, seek, and small-read latency.
    Random {
        /// Deterministic request seed.
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Maximum number of distinct non-empty files used as random candidates.
        #[arg(long)]
        max_entries: Option<usize>,
        /// Number of random requests to measure.
        #[arg(long, default_value_t = 100)]
        requests: usize,
        /// Bytes read for each request.
        #[arg(long, default_value_t = 4096)]
        read_size: usize,
        /// Internal archivefs cache state for each request.
        #[arg(long, value_enum, default_value_t = CacheState::Cold)]
        cache_state: CacheState,
    },
    /// Read image-like entries as thumbnail input candidates (no image decoding).
    ThumbnailRead {
        /// Maximum number of image entries to read.
        #[arg(long, default_value_t = 32)]
        max_images: usize,
        /// Read buffer size in bytes.
        #[arg(long, default_value_t = 64 * 1024)]
        chunk_size: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CacheState {
    Cold,
    Warm,
}

impl From<CacheState> for scenarios::CacheState {
    fn from(value: CacheState) -> Self {
        match value {
            CacheState::Cold => Self::Cold,
            CacheState::Warm => Self::Warm,
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Inspect { archive } => scenarios::inspect(&archive),
        Command::Verify { archive } => scenarios::verify(&archive),
        Command::Bench(args) => {
            if args.iterations == 0 {
                return Err("--iterations must be greater than zero".into());
            }
            let config = benchmark::Config {
                warmup: args.warmup,
                iterations: args.iterations,
            };
            match args.scenario {
                Scenario::Open => scenarios::bench_open(&args.archive, config),
                Scenario::Scan {
                    max_entries,
                    chunk_size,
                } => {
                    if chunk_size == 0 {
                        return Err("--chunk-size must be greater than zero".into());
                    }
                    scenarios::bench_scan(&args.archive, config, max_entries, chunk_size)
                }
                Scenario::Random {
                    seed,
                    max_entries,
                    requests,
                    read_size,
                    cache_state,
                } => {
                    if requests == 0 {
                        return Err("--requests must be greater than zero".into());
                    }
                    if read_size == 0 {
                        return Err("--read-size must be greater than zero".into());
                    }
                    if max_entries == Some(0) {
                        return Err("--max-entries must be greater than zero when specified".into());
                    }
                    scenarios::bench_random(
                        &args.archive,
                        config,
                        max_entries,
                        requests,
                        read_size,
                        seed,
                        cache_state.into(),
                    )
                }
                Scenario::ThumbnailRead {
                    max_images,
                    chunk_size,
                } => {
                    if max_images == 0 {
                        return Err("--max-images must be greater than zero".into());
                    }
                    if chunk_size == 0 {
                        return Err("--chunk-size must be greater than zero".into());
                    }
                    scenarios::bench_thumbnail_read(&args.archive, config, max_images, chunk_size)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn parses_random_benchmark_arguments() {
        let cli = Cli::try_parse_from([
            "archivefs-bench",
            "bench",
            "sample.zip",
            "--iterations",
            "3",
            "random",
            "--max-entries",
            "4",
            "--requests",
            "5",
            "--cache-state",
            "warm",
        ])
        .expect("parse random benchmark arguments");

        assert!(format!("{cli:?}").contains("Warm"));
    }
}
