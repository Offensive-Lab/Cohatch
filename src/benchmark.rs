//! Bounded byte and time sampling using the same response validation as downloads.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;

use crate::{
    Error, Result,
    http::HttpClient,
    manifest::{DEFAULT_CHUNK_SIZE, MAX_CHUNKS, Manifest},
};

const SAMPLE_RANGE_BYTES: u64 = 64 * 1024;

#[derive(Debug)]
pub struct BenchmarkRow {
    pub connections: usize,
    /// Actual payload received, including partial ranges when the time cap expires.
    pub bytes: u64,
    pub seconds: f64,
    pub mib_per_second: f64,
    pub timed_out: bool,
}

/// Each level has its own requested payload cap and wall-clock cap. The initial
/// probe adds one byte to the complete benchmark's budget. There are no retries:
/// a rate limit immediately ends the benchmark instead of applying more pressure.
pub async fn benchmark(
    client: &HttpClient,
    url: &str,
    levels: &[usize],
    sample_bytes: u64,
    seconds: u64,
    expected_sha256: Option<String>,
    cancel: CancellationToken,
) -> Result<Vec<BenchmarkRow>> {
    if levels.is_empty()
        || levels.len() > 16
        || levels.iter().any(|&value| value == 0 || value > 128)
        || !(1..=64 * 1024 * 1024).contains(&sample_bytes)
        || !(1..=60).contains(&seconds)
    {
        return Err(Error::InvalidInput(
            "benchmark requires 1–16 levels of 1–128 connections, a 1 byte–64 MiB sample cap, and 1–60 seconds per level".into(),
        ));
    }
    let probe = tokio::select! {
        _ = cancel.cancelled() => return Err(Error::Cancelled),
        result = client.probe(url) => result?,
    };
    // Keep the layout within manifest limits even for very large sources.
    // Sample ranges are independent of these ordinary download chunk boundaries.
    let chunk_size = DEFAULT_CHUNK_SIZE.max(probe.file_size.div_ceil(MAX_CHUNKS));
    let manifest = Manifest::new(
        url.to_owned(),
        probe.filename,
        probe.file_size,
        chunk_size,
        1,
        probe.source,
        expected_sha256,
    )?;
    let ranges = sample_ranges(manifest.file_size, sample_bytes);
    let mut rows = Vec::new();
    for (position, &connections) in levels.iter().enumerate() {
        let started = Instant::now();
        let received = AtomicU64::new(0);
        let mut timed_out = false;
        let deadline = tokio::time::sleep(Duration::from_secs(seconds));
        tokio::pin!(deadline);
        let manifest_ref = &manifest;
        let received_ref = &received;
        let mut requests = stream::iter(ranges.iter().copied())
            .map(|(start, end)| async move {
                let mut body = client
                    .range_bytes(manifest_ref, start, end)
                    .await?
                    .bytes_stream();
                let mut length = 0u64;
                let expected = end - start + 1;
                while let Some(data) = body.next().await {
                    let amount = data.map_err(crate::http::network)?.len() as u64;
                    length += amount;
                    if length > expected {
                        return Err(Error::Protocol(
                            "benchmark range exceeded its length".into(),
                        ));
                    }
                    received_ref.fetch_add(amount, Ordering::Relaxed);
                }
                if length != expected {
                    return Err(Error::Protocol("benchmark range was truncated".into()));
                }
                Ok(())
            })
            .buffer_unordered(connections);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = &mut deadline => { timed_out = true; break; },
                next = requests.next() => match next {
                    Some(result) => result?,
                    None => break,
                },
            }
        }
        // Dropping in-flight futures cancels the requests at the deadline.
        drop(requests);
        let seconds = started.elapsed().as_secs_f64();
        let bytes = received.load(Ordering::Relaxed);
        rows.push(BenchmarkRow {
            connections,
            bytes,
            seconds,
            mib_per_second: bytes as f64 / (1024.0 * 1024.0) / seconds.max(0.000_001),
            timed_out,
        });
        if position + 1 < levels.len() {
            // Short breathing space between levels, also interruptible.
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
        }
    }
    Ok(rows)
}

fn sample_ranges(file_size: u64, budget: u64) -> Vec<(u64, u64)> {
    let available = budget.min(file_size);
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < available {
        let length = SAMPLE_RANGE_BYTES.min(available - start);
        ranges.push((start, start + length - 1));
        start += length;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_plan_exactly_obeys_budget_and_source_length() {
        for file_size in [1, 100, 65_535, 65_536, 65_537, 100 * 1024 * 1024 * 1024] {
            for budget in [1, 7, 65_535, 65_536, 65_537, 8 * 1024 * 1024] {
                let ranges = sample_ranges(file_size, budget);
                let total: u64 = ranges.iter().map(|(start, end)| end - start + 1).sum();
                assert_eq!(total, budget.min(file_size));
                assert_eq!(ranges.first().unwrap().0, 0);
                assert_eq!(ranges.last().unwrap().1, total - 1);
                assert!(
                    ranges
                        .iter()
                        .all(|(start, end)| end - start < SAMPLE_RANGE_BYTES)
                );
                assert!(ranges.windows(2).all(|pair| pair[0].1 + 1 == pair[1].0));
            }
        }
    }

    #[tokio::test]
    async fn invalid_options_fail_before_network() {
        let client = HttpClient::new(Default::default()).unwrap();
        for (levels, budget, seconds) in [
            (vec![], 1, 1),
            (vec![0], 1, 1),
            (vec![129], 1, 1),
            (vec![1], 0, 1),
            (vec![1], 64 * 1024 * 1024 + 1, 1),
            (vec![1], 1, 0),
            (vec![1], 1, 61),
        ] {
            let result = benchmark(
                &client,
                "invalid-url",
                &levels,
                budget,
                seconds,
                None,
                CancellationToken::new(),
            )
            .await;
            assert!(matches!(result, Err(Error::InvalidInput(_))));
        }
    }
}
