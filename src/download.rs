//! Bounded asynchronous workers, independent retries, and durable chunk resume.
use crate::{
    Error, Result,
    http::{HttpClient, network},
    storage::{Store, StoreLock},
};
use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    sync::{Mutex, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    pub node: u32,
    pub connections: usize,
    /// Additional attempts after the first request.
    pub retries: u32,
    pub backoff: Duration,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            node: 1,
            connections: 8,
            retries: 5,
            backoff: Duration::from_secs(1),
        }
    }
}

#[derive(Default, Debug)]
pub struct Progress {
    /// Payload transferred in this invocation, including discarded failed attempts.
    pub downloaded_bytes: AtomicU64,
    /// Assigned, validated chunks, including resumed chunks.
    pub completed_chunks: AtomicU64,
    pub completed_bytes: AtomicU64,
    pub retries: AtomicU64,
}

#[derive(Debug)]
pub struct DownloadReport {
    pub downloaded_chunks: u64,
    pub skipped_chunks: u64,
    pub bytes: u64,
    pub retries: u64,
}

/// Cancellation drops active request futures. Unpublished stages are ignored on
/// restart; their filenames can never be confused with committed chunks.
pub async fn download(
    store: &Store,
    client: &HttpClient,
    options: DownloadOptions,
    cancel: CancellationToken,
    progress: Arc<Progress>,
) -> Result<DownloadReport> {
    store.manifest.validate()?;
    if options.connections == 0
        || options.retries > 100
        || options.backoff.is_zero()
        || options.backoff > Duration::from_secs(3600)
    {
        return Err(Error::InvalidInput(
            "connections must be positive, retries at most 100, and backoff in (0, 3600s]".into(),
        ));
    }
    let assigned = store.manifest.assigned_indices(options.node)?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let lock = Arc::new(store.lock()?);
    store.prepare()?;
    let mut pending = Vec::new();
    let mut skipped = 0u64;
    let mut complete_bytes = 0u64;
    // Perform the integrity scan once, off the networking runtime. It is not a
    // progress-display operation and intentionally reads each saved chunk.
    let scan_store = store.clone();
    let scan_cancel = cancel.clone();
    let (valid, assigned) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut valid = Vec::with_capacity(assigned.len());
        for &index in &assigned {
            if scan_cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            valid.push(scan_store.validate_chunk(index)?);
        }
        Ok((valid, assigned))
    })
    .await
    .map_err(|e| Error::Integrity(format!("scan task failed: {e}")))??;
    for (&index, is_valid) in assigned.iter().zip(valid) {
        if is_valid {
            skipped += 1;
            complete_bytes += store.manifest.chunk_len(index)?;
        } else {
            pending.push(index);
        }
    }
    progress.downloaded_bytes.store(0, Ordering::Relaxed);
    progress.completed_chunks.store(skipped, Ordering::Relaxed);
    progress
        .completed_bytes
        .store(complete_bytes, Ordering::Relaxed);
    progress.retries.store(0, Ordering::Relaxed);
    let pending_count = pending.len() as u64;
    // A 429/503 Retry-After pauses every worker's next request, not only the
    // worker that happened to receive it. Existing in-flight responses finish.
    let gate = Arc::new(Mutex::new(Instant::now()));
    // Rehashing/flushing a staged chunk is blocking disk work. Bound these jobs
    // independently of network concurrency so 128 connections do not create
    // 128 hashing threads or stop polling the other network responses.
    let commits = Arc::new(Semaphore::new(2));
    let mut workers = stream::iter(pending).map(|index| {
        let gate = gate.clone();
        let progress = progress.clone();
        let lock = lock.clone();
        let commits = commits.clone();
        let options = &options;
        async move {
            for attempt in 0..=options.retries {
                wait_gate(&gate).await;
                match fetch_chunk(store, client, index, &progress, lock.clone(), commits.clone()).await {
                    Ok(()) => {
                        progress.completed_chunks.fetch_add(1, Ordering::Relaxed);
                        progress.completed_bytes.fetch_add(store.manifest.chunk_len(index)?, Ordering::Relaxed);
                        return Ok(());
                    }
                    Err(error) => {
                        tracing::warn!(chunk = index, attempt, error = %error, "chunk attempt failed");
                        if !retryable(&error) || attempt == options.retries { return Err(error); }
                        progress.retries.fetch_add(1, Ordering::Relaxed);
                        let exponential = options.backoff.mul_f64(2f64.powi(attempt.min(16) as i32)).min(Duration::from_secs(60));
                        let jitter = Duration::from_secs_f64(rand::random::<f64>() * exponential.as_secs_f64() * 0.25);
                        let mut delay = exponential + jitter;
                        if let Error::Http { status, retry_after, .. } = &error {
                            if let Some(value) = retry_after { delay = delay.max(*value); }
                            if *status == 429 || retry_after.is_some() {
                                let deadline = Instant::now().checked_add(delay).ok_or_else(|| Error::InvalidInput("Retry-After is too large to schedule; stop and retry later".into()))?;
                                let mut until = gate.lock().await;
                                *until = (*until).max(deadline);
                            }
                        }
                        tracing::info!(chunk = index, retry = attempt + 1, wait_seconds = delay.as_secs_f64(), "retry scheduled");
                        tokio::time::sleep(delay).await;
                    }
                }
            }
            unreachable!("attempt loop always returns")
        }
    }).buffer_unordered(options.connections);
    let outcome = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Err(Error::Cancelled),
            next = workers.next() => match next {
                Some(Ok(())) => {},
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
            },
        }
    };
    // Drop active network futures, then drain any already-started disk commits.
    // Cancellation/error returns only after their locks and temp guards release,
    // so an immediate resume does not race detached publication.
    drop(workers);
    let _drained = commits
        .acquire_many(2)
        .await
        .map_err(|_| Error::Cancelled)?;
    drop(lock);
    outcome?;
    Ok(DownloadReport {
        downloaded_chunks: pending_count,
        skipped_chunks: skipped,
        bytes: progress.downloaded_bytes.load(Ordering::Relaxed),
        retries: progress.retries.load(Ordering::Relaxed),
    })
}

async fn wait_gate(gate: &Mutex<Instant>) {
    loop {
        let until = *gate.lock().await;
        if until <= Instant::now() {
            return;
        }
        tokio::time::sleep_until(until).await;
    }
}

async fn fetch_chunk(
    store: &Store,
    client: &HttpClient,
    index: u64,
    progress: &Progress,
    lock: Arc<StoreLock>,
    commits: Arc<Semaphore>,
) -> Result<()> {
    let response = client.range(&store.manifest, index).await?;
    let expected = store.manifest.chunk_len(index)?;
    let (path, file) = store.stage(index)?;
    let cleanup = tempfile::TempPath::try_from_path(path.clone())?;
    let mut output = BufWriter::with_capacity(256 * 1024, tokio::fs::File::from_std(file));
    let mut stream = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut received = 0u64;
    while let Some(data) = stream.next().await {
        let data = data.map_err(network)?;
        received = received
            .checked_add(data.len() as u64)
            .ok_or_else(|| Error::Protocol("body length overflow".into()))?;
        if received > expected {
            return Err(Error::Protocol(format!(
                "chunk {index}: response exceeds requested length"
            )));
        }
        output.write_all(&data).await?;
        hash.update(&data);
        progress
            .downloaded_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
    }
    if received != expected {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("chunk {index}: body ended at {received}, expected {expected}"),
        )));
    }
    output.flush().await?;
    drop(output);
    let permit = commits
        .acquire_owned()
        .await
        .map_err(|_| Error::Cancelled)?;
    let commit_store = store.clone();
    let digest = hex::encode(hash.finalize());
    tokio::task::spawn_blocking(move || {
        // These guards remain inside the blocking job if its async waiter is
        // cancelled. Publication can never outlive the exclusive directory lock,
        // and the stage survives until commit has finished using it.
        // Reverse drop order releases the lock before returning the permit to
        // the drain above; the permit is also released if commit panics.
        let _permit = permit;
        let _lock = lock;
        let _cleanup = cleanup;
        commit_store.commit(index, &path, &digest)
    })
    .await
    .map_err(|error| Error::Integrity(format!("commit task failed: {error}")))??;
    tracing::debug!(chunk = index, bytes = received, "chunk committed");
    Ok(())
}

fn retryable(error: &Error) -> bool {
    match error {
        // Reqwest can wrap an interrupted HTTP body as Decode even with all
        // content decompression disabled. It still needs a fresh chunk attempt.
        Error::Network(e) => {
            e.is_timeout() || e.is_connect() || e.is_body() || e.is_decode() || e.is_request()
        }
        Error::Http { status, .. } => {
            *status == 408 || *status == 429 || (500..=599).contains(status)
        }
        Error::Io(e) => e.kind() == std::io::ErrorKind::UnexpectedEof,
        _ => false,
    }
}
