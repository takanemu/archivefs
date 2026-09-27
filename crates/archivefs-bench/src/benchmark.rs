use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub warmup: usize,
    pub iterations: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub elapsed: Duration,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct Summary {
    pub count: usize,
    pub min: Duration,
    pub mean_nanos: f64,
    pub median: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub total_elapsed: Duration,
    pub total_bytes: u64,
}

pub fn measure<T, E>(
    config: Config,
    mut operation: impl FnMut(bool, usize) -> Result<T, E>,
) -> Result<Vec<T>, E> {
    for index in 0..config.warmup {
        let _ = operation(true, index)?;
    }

    let mut measured = Vec::with_capacity(config.iterations);
    for index in 0..config.iterations {
        measured.push(operation(false, index)?);
    }
    Ok(measured)
}

pub fn summarize(samples: &[Sample]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }

    let mut elapsed: Vec<_> = samples.iter().map(|sample| sample.elapsed).collect();
    elapsed.sort_unstable();
    let total_nanos: u128 = elapsed.iter().map(Duration::as_nanos).sum();
    let total_elapsed = Duration::from_nanos(total_nanos.min(u128::from(u64::MAX)) as u64);
    let total_bytes = samples
        .iter()
        .fold(0u64, |total, sample| total.saturating_add(sample.bytes));

    Some(Summary {
        count: samples.len(),
        min: elapsed[0],
        mean_nanos: total_nanos as f64 / samples.len() as f64,
        median: percentile(&elapsed, 50),
        p95: percentile(&elapsed, 95),
        p99: percentile(&elapsed, 99),
        max: *elapsed.last().expect("non-empty duration list"),
        total_elapsed,
        total_bytes,
    })
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    let rank = percentile.saturating_mul(sorted.len()).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::{Config, Sample, measure, summarize};
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn excludes_warmups_from_measured_results() {
        let calls = Cell::new(0);
        let result = measure(
            Config {
                warmup: 2,
                iterations: 3,
            },
            |is_warmup, index| {
                calls.set(calls.get() + 1);
                Ok::<_, ()>((is_warmup, index, calls.get()))
            },
        )
        .expect("measure operation");

        assert_eq!(calls.get(), 5);
        assert_eq!(result, vec![(false, 0, 3), (false, 1, 4), (false, 2, 5)]);
    }

    #[test]
    fn computes_nearest_rank_percentiles_and_byte_totals() {
        let samples: Vec<_> = (1..=100)
            .map(|millis| Sample {
                elapsed: Duration::from_millis(millis),
                bytes: 10,
            })
            .collect();
        let summary = summarize(&samples).expect("summarize samples");

        assert_eq!(summary.count, 100);
        assert_eq!(summary.min, Duration::from_millis(1));
        assert_eq!(summary.median, Duration::from_millis(50));
        assert_eq!(summary.p95, Duration::from_millis(95));
        assert_eq!(summary.p99, Duration::from_millis(99));
        assert_eq!(summary.max, Duration::from_millis(100));
        assert_eq!(summary.total_bytes, 1000);
    }

    #[test]
    fn empty_measurements_have_no_summary() {
        assert!(summarize(&[]).is_none());
    }
}
