use crate::benchmark::{Sample, summarize};

pub fn print_report(label: &str, samples: &[Sample]) {
    let Some(summary) = summarize(samples) else {
        println!("{label}: no samples");
        return;
    };

    println!("\n{label}");
    println!("  samples: {}", summary.count);
    println!(
        "  latency (ms): min {:.3}, mean {:.3}, median {:.3}, p95 {:.3}, p99 {:.3}, max {:.3}",
        summary.min.as_secs_f64() * 1000.0,
        summary.mean_nanos / 1_000_000.0,
        summary.median.as_secs_f64() * 1000.0,
        summary.p95.as_secs_f64() * 1000.0,
        summary.p99.as_secs_f64() * 1000.0,
        summary.max.as_secs_f64() * 1000.0,
    );
    println!("  bytes: {}", summary.total_bytes);
    if summary.total_elapsed.is_zero() {
        println!("  throughput: n/a (elapsed time rounded to zero)");
    } else {
        let megabytes_per_second =
            summary.total_bytes as f64 / summary.total_elapsed.as_secs_f64() / 1_000_000.0;
        println!("  throughput: {megabytes_per_second:.3} MB/s");
    }
    print_histogram(samples);
}

fn print_histogram(samples: &[Sample]) {
    const BUCKETS: usize = 10;
    const MAX_BAR: usize = 40;
    let Some(summary) = summarize(samples) else {
        return;
    };
    let min = summary.min.as_nanos();
    let max = summary.max.as_nanos();
    let span = max.saturating_sub(min);
    let mut buckets = [0usize; BUCKETS];
    for sample in samples {
        let value = sample.elapsed.as_nanos().saturating_sub(min);
        let bucket = if span == 0 {
            BUCKETS / 2
        } else {
            value
                .saturating_mul(BUCKETS as u128)
                .checked_div(span)
                .unwrap_or(0)
                .min((BUCKETS - 1) as u128) as usize
        };
        buckets[bucket] += 1;
    }
    let largest = buckets.iter().copied().max().unwrap_or(1).max(1);

    println!("  latency distribution (ms):");
    for (index, count) in buckets.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let lower = min as f64 + span as f64 * index as f64 / BUCKETS as f64;
        let upper = min as f64 + span as f64 * (index + 1) as f64 / BUCKETS as f64;
        let bar_len = (count * MAX_BAR / largest).max(1);
        println!(
            "    {:>8.3}..{:>8.3} | {:>5} {}",
            lower / 1_000_000.0,
            upper / 1_000_000.0,
            count,
            "#".repeat(bar_len),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::print_report;
    use crate::benchmark::Sample;
    use std::time::Duration;

    #[test]
    fn report_handles_empty_samples() {
        print_report("empty", &[]);
    }

    #[test]
    fn report_handles_identical_durations() {
        let samples = [Sample {
            elapsed: Duration::from_millis(1),
            bytes: 100,
        }];
        print_report("identical", &samples);
    }
}
