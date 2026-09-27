use crate::benchmark::{self, Config, Sample};
use crate::progress;
use crate::report::print_report;
use lazy_archive::{detect_archive_kind, open_archive};
use std::error::Error;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;
use vfs_archive::{ArchiveEntry, ArchiveVfs};

type CliResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    Cold,
    Warm,
}

#[derive(Debug, Clone, Copy)]
struct ScanSample {
    sample: Sample,
    files: u64,
    read_calls: u64,
    data_read_calls: u64,
}

#[derive(Debug, Clone)]
struct RandomRequest {
    path: PathBuf,
    offset: u64,
    length: usize,
}

pub fn inspect(path: &Path) -> CliResult<()> {
    let kind = detect_archive_kind(path)?;
    let archive = open_archive(path)?;
    let entries = archive.entries()?;
    let files: Vec<_> = entries
        .iter()
        .filter(|entry| !entry.is_directory())
        .collect();
    let directories = entries.len() - files.len();
    let total_size = files
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.size()));

    println!("archive: {}", path.display());
    println!("format: {kind:?}");
    println!("entries: {}", entries.len());
    println!("files: {}", files.len());
    println!("directories: {directories}");
    println!("uncompressed size: {total_size} bytes");
    println!("sample entries:");
    for entry in entries.iter().take(5) {
        let kind = if entry.is_directory() {
            "directory"
        } else {
            "file"
        };
        println!("  {kind:9} {:>12} {}", entry.size(), entry.path().display());
    }
    Ok(())
}

pub fn verify(path: &Path) -> CliResult<()> {
    let kind = detect_archive_kind(path)?;
    let archive = open_archive(path)?;
    let entries = archive.entries()?;
    println!("archive: {} ({kind:?})", path.display());
    println!("entries: {}", entries.len());

    let Some(entry) = entries.iter().find(|entry| !entry.is_directory()) else {
        println!("verification: listing passed; no regular file to read/seek");
        return Ok(());
    };

    let mut reader = archive.open(entry.path())?;
    let prefix_len = entry.size().min(32) as usize;
    let mut prefix = vec![0; prefix_len];
    reader.read_exact(&mut prefix)?;
    reader.seek(SeekFrom::Start(0))?;
    let mut reread = vec![0; prefix_len];
    reader.read_exact(&mut reread)?;
    if prefix != reread {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "prefix changed after seek").into());
    }
    if entry.size() > 0 {
        let current = reader.seek(SeekFrom::Current(-1))?;
        if current != prefix_len.saturating_sub(1) as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "relative seek returned an unexpected position",
            )
            .into());
        }
        reader.seek(SeekFrom::End(-1))?;
        let mut last_byte = [0];
        reader.read_exact(&mut last_byte)?;
    }

    println!(
        "verified entry: {} ({} bytes)",
        entry.path().display(),
        entry.size()
    );
    println!("verification: listing, read, and seek passed");
    Ok(())
}

pub fn bench_open(path: &Path, config: Config) -> CliResult<()> {
    let total_steps = (config.warmup + config.iterations).saturating_mul(2) as u64;
    let bar = progress::start("preparing open/list benchmark", Some(total_steps));
    let open_samples = benchmark::measure(config, |is_warmup, index| -> CliResult<Sample> {
        let phase = progress::phase(is_warmup, index, config.warmup, config.iterations);
        bar.set_message(format!("{phase}: opening archive"));
        let start = Instant::now();
        let archive = open_archive(path)?;
        let elapsed = start.elapsed();
        drop(archive);
        bar.inc(1);
        Ok(Sample { elapsed, bytes: 0 })
    })?;

    let listing_samples = benchmark::measure(config, |is_warmup, index| -> CliResult<Sample> {
        let phase = progress::phase(is_warmup, index, config.warmup, config.iterations);
        bar.set_message(format!("{phase}: listing entries"));
        let archive = open_archive(path)?;
        let start = Instant::now();
        let entries = archive.entries()?;
        let elapsed = start.elapsed();
        drop(entries);
        bar.inc(1);
        Ok(Sample { elapsed, bytes: 0 })
    })?;

    bar.finish_with_message("open/list benchmark complete");
    print_measurement_notes();
    print_report("archive open", &open_samples);
    print_report("entry listing", &listing_samples);
    Ok(())
}

pub fn bench_scan(
    path: &Path,
    config: Config,
    max_entries: Option<usize>,
    chunk_size: usize,
) -> CliResult<()> {
    if max_entries == Some(0) {
        return Err("--max-entries must be greater than zero when specified".into());
    }

    let bar = progress::start("preparing scan targets", None);
    let archive = open_archive(path)?;
    let entries = regular_files(archive.entries()?);
    let selected_count = entries.len().min(max_entries.unwrap_or(usize::MAX));
    let total_steps = selected_count.saturating_mul(config.warmup + config.iterations);
    bar.set_length(total_steps.max(1) as u64);

    let scan_samples = benchmark::measure(config, |is_warmup, index| {
        let phase = progress::phase(is_warmup, index, config.warmup, config.iterations);
        scan_once(
            archive.as_ref(),
            &entries,
            max_entries,
            chunk_size,
            &bar,
            &phase,
        )
    })?;
    bar.finish_with_message("sequential scan complete");
    print_measurement_notes();
    print_scan_metrics(&scan_samples, chunk_size, config.iterations);
    let samples: Vec<_> = scan_samples.iter().map(|scan| scan.sample).collect();
    print_report("sequential scan", &samples);
    Ok(())
}

fn scan_once(
    archive: &dyn ArchiveVfs,
    entries: &[ArchiveEntry],
    max_entries: Option<usize>,
    chunk_size: usize,
    bar: &indicatif::ProgressBar,
    phase: &str,
) -> CliResult<ScanSample> {
    let limit = max_entries.unwrap_or(usize::MAX);
    let mut buffer = vec![0u8; chunk_size];
    let start = Instant::now();
    let mut bytes = 0u64;
    let mut read_calls = 0u64;
    let mut data_read_calls = 0u64;

    for (index, entry) in entries.iter().take(limit).enumerate() {
        bar.set_message(format!(
            "{phase}: file {}/{} {}",
            index + 1,
            entries.len().min(limit),
            entry.path().display()
        ));
        let mut reader = archive.open(entry.path())?;
        loop {
            read_calls = read_calls.saturating_add(1);
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            data_read_calls = data_read_calls.saturating_add(1);
            bytes = bytes.saturating_add(read as u64);
        }
        bar.inc(1);
    }

    Ok(ScanSample {
        sample: Sample {
            elapsed: start.elapsed(),
            bytes,
        },
        files: entries.len().min(limit) as u64,
        read_calls,
        data_read_calls,
    })
}

fn print_scan_metrics(samples: &[ScanSample], chunk_size: usize, iterations: usize) {
    let files: u64 = samples
        .iter()
        .map(|sample| sample.files)
        .fold(0, u64::saturating_add);
    let read_calls: u64 = samples
        .iter()
        .map(|sample| sample.read_calls)
        .fold(0, u64::saturating_add);
    let data_read_calls: u64 = samples
        .iter()
        .map(|sample| sample.data_read_calls)
        .fold(0, u64::saturating_add);
    let bytes: u64 = samples
        .iter()
        .map(|sample| sample.sample.bytes)
        .fold(0, u64::saturating_add);

    println!("scan read buffer: {chunk_size} bytes (maximum requested per Read call)");
    println!(
        "files read: {files} across {} measured scans",
        samples.len()
    );
    println!(
        "Read calls: {read_calls} total ({data_read_calls} returned data, {iterations} measured iterations; EOF probes included)"
    );
    if data_read_calls == 0 {
        println!("average bytes per data Read call: n/a");
    } else {
        println!(
            "average bytes per data Read call: {:.2}",
            bytes as f64 / data_read_calls as f64
        );
    }
}

pub fn bench_random(
    path: &Path,
    config: Config,
    max_entries: Option<usize>,
    requests_per_iteration: usize,
    read_size: usize,
    seed: u64,
    cache_state: CacheState,
) -> CliResult<()> {
    if requests_per_iteration == 0 {
        return Err("--requests must be greater than zero".into());
    }
    if read_size == 0 {
        return Err("--read-size must be greater than zero".into());
    }

    let bar = progress::start("preparing random-access requests", None);
    let setup_archive = open_archive(path)?;
    let entries = regular_files(setup_archive.entries()?)
        .into_iter()
        .filter(|entry| entry.size() > 0)
        .collect::<Vec<_>>();
    let entries = select_random_entries(entries, max_entries, seed)?;
    if entries.is_empty() {
        return Err("archive has no non-empty regular files for random access".into());
    }

    let total_requests = config
        .warmup
        .saturating_add(config.iterations)
        .saturating_mul(requests_per_iteration);
    let requests = generate_requests(&entries, total_requests, read_size, seed);
    drop(setup_archive);

    let measured_archive = open_archive(path)?;
    let warm_paths: std::collections::BTreeSet<_> = requests
        .iter()
        .map(|request| request.path.clone())
        .collect();
    let prewarm_count = if cache_state == CacheState::Warm {
        warm_paths.len()
    } else {
        0
    };
    bar.set_length(total_requests.saturating_add(prewarm_count).max(1) as u64);

    if cache_state == CacheState::Warm {
        for (index, candidate) in warm_paths.iter().enumerate() {
            bar.set_message(format!(
                "warming cache {}/{} {}",
                index + 1,
                warm_paths.len(),
                candidate.display()
            ));
            drop(measured_archive.open(candidate)?);
            bar.inc(1);
        }
    }

    let mut request_index = 0;
    let measured = benchmark::measure(config, |is_warmup, index| -> CliResult<Vec<Sample>> {
        let phase = progress::phase(is_warmup, index, config.warmup, config.iterations);
        if is_warmup && cache_state == CacheState::Cold {
            let warmup_archive = open_archive(path)?;
            return random_iteration(
                warmup_archive.as_ref(),
                &requests,
                &mut request_index,
                requests_per_iteration,
                &bar,
                &phase,
            );
        }
        random_iteration(
            measured_archive.as_ref(),
            &requests,
            &mut request_index,
            requests_per_iteration,
            &bar,
            &phase,
        )
    })?;
    let samples: Vec<_> = measured.into_iter().flatten().collect();

    bar.finish_with_message("random benchmark complete");
    print_measurement_notes();
    println!("random cache mode: {cache_state:?}");
    println!("distinct candidate files: {}", entries.len());
    println!("request selection: with replacement; a file may appear more than once");
    if cache_state == CacheState::Cold {
        println!(
            "cold mode: one empty VFS cache at measured start; reused across measured requests and iterations"
        );
    } else {
        println!("warm mode: requested candidate entries were pre-opened before measurement");
    }
    print_report("random open/seek/read latency", &samples);
    Ok(())
}

fn random_iteration(
    archive: &dyn ArchiveVfs,
    requests: &[RandomRequest],
    request_index: &mut usize,
    requests_per_iteration: usize,
    bar: &indicatif::ProgressBar,
    phase: &str,
) -> CliResult<Vec<Sample>> {
    let mut samples = Vec::with_capacity(requests_per_iteration);
    for (local_index, request) in requests
        .iter()
        .skip(*request_index)
        .take(requests_per_iteration)
        .enumerate()
    {
        bar.set_message(format!(
            "{phase}: random request {}/{} ({})",
            local_index + 1,
            requests_per_iteration,
            request.path.display()
        ));
        samples.push(random_read(archive, request)?);
        bar.inc(1);
    }
    *request_index += requests_per_iteration;
    Ok(samples)
}

fn select_random_entries(
    mut entries: Vec<ArchiveEntry>,
    max_entries: Option<usize>,
    seed: u64,
) -> CliResult<Vec<ArchiveEntry>> {
    let Some(limit) = max_entries else {
        return Ok(entries);
    };
    if limit == 0 {
        return Err("--max-entries must be greater than zero when specified".into());
    }
    if entries.len() <= limit {
        return Ok(entries);
    }

    let mut state = if seed == 0 {
        0xD1B5_4A32_D192_ED03
    } else {
        seed ^ 0xD1B5_4A32_D192_ED03
    };
    for index in (1..entries.len()).rev() {
        let selected = (next_random(&mut state) % (index as u64 + 1)) as usize;
        entries.swap(index, selected);
    }
    entries.truncate(limit);
    entries.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(entries)
}

fn random_read(archive: &dyn ArchiveVfs, request: &RandomRequest) -> CliResult<Sample> {
    let start = Instant::now();
    let mut reader = archive.open(&request.path)?;
    reader.seek(SeekFrom::Start(request.offset))?;
    let mut buffer = vec![0u8; request.length];
    reader.read_exact(&mut buffer)?;
    Ok(Sample {
        elapsed: start.elapsed(),
        bytes: request.length as u64,
    })
}

pub fn bench_thumbnail_read(
    path: &Path,
    config: Config,
    max_images: usize,
    chunk_size: usize,
) -> CliResult<()> {
    if max_images == 0 {
        return Err("--max-images must be greater than zero".into());
    }
    if chunk_size == 0 {
        return Err("--chunk-size must be greater than zero".into());
    }

    let bar = progress::start("searching image candidates", None);
    let archive = open_archive(path)?;
    let candidates = archive
        .entries()?
        .into_iter()
        .filter(|entry| !entry.is_directory() && is_image_path(entry.path()))
        .take(max_images)
        .map(|entry| entry.path().to_owned())
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err("archive has no supported image entries for thumbnail-read".into());
    }

    let total_steps = candidates
        .len()
        .saturating_mul(config.warmup + config.iterations);
    bar.set_length(total_steps.max(1) as u64);
    let measured = benchmark::measure(config, |is_warmup, index| -> CliResult<Vec<Sample>> {
        let phase = progress::phase(is_warmup, index, config.warmup, config.iterations);
        thumbnail_iteration(path, &candidates, chunk_size, &bar, &phase)
    })?;
    let samples: Vec<_> = measured.into_iter().flatten().collect();

    bar.finish_with_message("thumbnail-read complete");
    print_measurement_notes();
    println!("thumbnail-read candidates: {}", candidates.len());
    println!(
        "note: this measures archive byte retrieval only; it does not decode or resize images"
    );
    print_report("thumbnail candidate read latency", &samples);
    Ok(())
}

fn thumbnail_iteration(
    path: &Path,
    candidates: &[PathBuf],
    chunk_size: usize,
    bar: &indicatif::ProgressBar,
    phase: &str,
) -> CliResult<Vec<Sample>> {
    let archive = open_archive(path)?;
    let mut samples = Vec::with_capacity(candidates.len());
    let mut buffer = vec![0u8; chunk_size];
    for candidate in candidates {
        bar.set_message(format!("{phase}: reading {}", candidate.display()));
        let start = Instant::now();
        let mut reader = archive.open(candidate)?;
        let bytes = read_to_sink(&mut reader, &mut buffer)?;
        samples.push(Sample {
            elapsed: start.elapsed(),
            bytes,
        });
        bar.inc(1);
    }
    Ok(samples)
}

fn read_to_sink(reader: &mut dyn Read, buffer: &mut [u8]) -> io::Result<u64> {
    let mut total = 0u64;
    loop {
        let read = reader.read(buffer)?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
    }
}

fn regular_files(entries: Vec<ArchiveEntry>) -> Vec<ArchiveEntry> {
    entries
        .into_iter()
        .filter(|entry| !entry.is_directory())
        .collect()
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff"
            )
        })
}

fn generate_requests(
    entries: &[ArchiveEntry],
    count: usize,
    read_size: usize,
    seed: u64,
) -> Vec<RandomRequest> {
    let mut state = if seed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        seed
    };
    (0..count)
        .map(|_| {
            let entry = &entries[(next_random(&mut state) % entries.len() as u64) as usize];
            let offset = next_random(&mut state) % entry.size();
            let length = (entry.size() - offset).min(read_size as u64) as usize;
            RandomRequest {
                path: entry.path().to_owned(),
                offset,
                length,
            }
        })
        .collect()
}

fn next_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn print_measurement_notes() {
    println!("note: OS page cache is not cleared; results depend on host and storage state");
}

#[cfg(test)]
mod tests {
    use super::{generate_requests, is_image_path, select_random_entries};
    use std::path::Path;
    use vfs_archive::ArchiveEntry;

    #[test]
    fn image_extension_match_is_case_insensitive() {
        assert!(is_image_path(Path::new("photos/COVER.PNG")));
        assert!(!is_image_path(Path::new("docs/readme.txt")));
    }

    #[test]
    fn random_requests_are_deterministic_and_inside_file_bounds() {
        let entries = vec![ArchiveEntry::file("one.bin", 3)];
        let first = generate_requests(&entries, 32, 8, 7);
        let second = generate_requests(&entries, 32, 8, 7);

        assert_eq!(first.len(), 32);
        assert_eq!(
            first
                .iter()
                .map(|request| (request.offset, request.length))
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|request| (request.offset, request.length))
                .collect::<Vec<_>>()
        );
        assert!(first.iter().all(|request| {
            request.offset < 3 && request.length > 0 && request.offset + request.length as u64 <= 3
        }));
    }

    #[test]
    fn random_candidate_limit_selects_a_seeded_distinct_subset() {
        let entries = (0..20)
            .map(|index| ArchiveEntry::file(format!("file-{index:02}.bin"), 10))
            .collect::<Vec<_>>();
        let first =
            select_random_entries(entries.clone(), Some(5), 42).expect("select random candidates");
        let second = select_random_entries(entries.clone(), Some(5), 42)
            .expect("repeat candidate selection");

        assert_eq!(first.len(), 5, "candidate set must respect the limit");
        assert_eq!(
            first.iter().map(|entry| entry.path()).collect::<Vec<_>>(),
            second.iter().map(|entry| entry.path()).collect::<Vec<_>>(),
            "candidate selection must be reproducible for the same seed"
        );
        assert!(
            select_random_entries(entries, Some(0), 42).is_err(),
            "zero candidate limit must be rejected"
        );
    }
}
