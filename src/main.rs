use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cohatch::{
    benchmark::benchmark,
    combine::{CombineReport, combine, verify},
    download::{DownloadOptions, Progress, download},
    http::{HttpClient, HttpOptions},
    manifest::{Manifest, parse_size},
    storage::Store,
};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Durable HTTP range downloads across one or two PCs")]
struct Cli {
    /// Enable timestamped debug logs on stderr. URLs and query tokens are omitted.
    #[arg(long, short, global = true)]
    verbose: bool,
    /// Total deadline for each HTTP request, including its body (seconds).
    #[arg(long, global = true, default_value_t = 1800)]
    timeout_seconds: u64,
    #[arg(long, global = true, default_value = "Cohatch/0.1.0")]
    user_agent: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Probe actual range support and create an immutable project manifest.
    Create {
        url: String,
        #[arg(long, default_value_t = 1)]
        nodes: u32,
        /// M/MiB use binary units; MB uses decimal units.
        #[arg(long, default_value = "64M", value_parser = size_arg)]
        chunk_size: u64,
        #[arg(long)]
        sha256: Option<String>,
        /// Safe basename for the reconstructed output.
        #[arg(long)]
        filename: Option<String>,
        /// New, empty project directory (default downloads/<download-id>).
        #[arg(long)]
        output_dir: Option<PathBuf>,
    },
    /// Describe a manifest without contacting the source.
    Inspect { manifest: PathBuf },
    /// Download this node's chunks; validated completed chunks are skipped.
    Download {
        manifest: PathBuf,
        /// One-based node number. Node 1 receives chunks 0,2,4... with two nodes.
        #[arg(long, default_value_t = 1)]
        node: u32,
        #[arg(long, default_value_t = 8)]
        connections: usize,
        /// Additional attempts per failed chunk.
        #[arg(long, default_value_t = 5)]
        retries: u32,
        #[arg(long, default_value_t = 1000)]
        backoff_ms: u64,
    },
    /// Check local receipts, lengths and hashes, entirely offline.
    Status {
        manifest: PathBuf,
        #[arg(long, default_value_t = 1)]
        node: u32,
    },
    /// Copy validated chunks from a transferred project or its chunks directory.
    Import { manifest: PathBuf, source: PathBuf },
    /// Reconstruct in byte order, verify, and safely publish the output.
    Combine {
        manifest: PathBuf,
        /// Output basename inside the project; existing files are never overwritten.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Compute the reconstructed file's SHA-256 and compare any trusted manifest hash.
    Verify {
        manifest: PathBuf,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Compare concurrency with a bounded sample budget; stops on rate limiting.
    Benchmark {
        url: String,
        #[arg(long, value_delimiter = ',', default_value = "1,2,4,8,16,32,64")]
        connections: Vec<usize>,
        /// Per-level maximum payload, at most 64 MiB.
        #[arg(long, default_value = "8M", value_parser = size_arg)]
        sample_bytes: u64,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        /// Trusted hash permits sampling a source without usable validators.
        #[arg(long)]
        sha256: Option<String>,
    },
}

fn size_arg(s: &str) -> std::result::Result<u64, String> {
    parse_size(s).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error:#}");
        std::process::exit(
            if matches!(
                error.downcast_ref::<cohatch::Error>(),
                Some(cohatch::Error::Cancelled)
            ) {
                130
            } else {
                1
            },
        );
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    // Only Cohatch events: avoid transport trace logs containing private URLs.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(if cli.verbose {
            "cohatch=debug"
        } else {
            "cohatch=warn"
        }))
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let client = || {
        HttpClient::new(HttpOptions {
            timeout: Duration::from_secs(cli.timeout_seconds),
            user_agent: cli.user_agent.clone(),
        })
    };
    match cli.command {
        Command::Create {
            url,
            nodes,
            chunk_size,
            sha256,
            filename,
            output_dir,
        } => {
            let probe = client()?.probe(&url).await?;
            let manifest = Manifest::new(
                url,
                filename.unwrap_or(probe.filename),
                probe.file_size,
                chunk_size,
                nodes,
                probe.source,
                sha256,
            )?;
            let root = output_dir
                .unwrap_or_else(|| PathBuf::from("downloads").join(&manifest.download_id));
            let store = Store::create(&root, &manifest)?;
            println!(
                "Created {}\nFile: {}\nSize: {}\nChunks: {} ({} each, final chunk may be smaller)\nNodes: {}",
                store.root.join("manifest.json").display(),
                manifest.filename,
                human_bytes(manifest.file_size),
                manifest.chunk_count,
                human_bytes(manifest.chunk_size),
                manifest.nodes
            );
            if manifest.expected_sha256.is_none() {
                println!(
                    "No trusted SHA-256 supplied: final output will have a computed hash, without source authentication."
                );
            }
        }
        Command::Inspect { manifest } => {
            let m = Manifest::load(&manifest)?;
            let mut url = url::Url::parse(&m.url)?;
            url.set_query(None);
            println!(
                "Cohatch manifest v{}\nDownload ID: {}\nSource: {}\nFile: {}\nSize: {} bytes\nChunk size: {} bytes\nChunks: {}\nNodes: {}\nStrong ETag: {}\nLast-Modified: {}\nTrusted SHA-256: {}",
                m.version,
                m.download_id,
                url,
                m.filename,
                m.file_size,
                m.chunk_size,
                m.chunk_count,
                m.nodes,
                m.source.etag.as_ref().is_some_and(|v| v.starts_with('"')),
                m.source.last_modified.is_some(),
                m.expected_sha256.as_deref().unwrap_or("not supplied")
            );
        }
        Command::Download {
            manifest,
            node,
            connections,
            retries,
            backoff_ms,
        } => {
            let store = Store::open(&manifest)?;
            let assigned = store.manifest.assigned_indices(node)?;
            let total: u64 = assigned
                .iter()
                .map(|&i| store.manifest.chunk_len(i).expect("validated manifest"))
                .sum();
            println!(
                "Cohatch | Node {node}/{} | {connections} connections | {} assigned chunks\nChecking completed chunks for resume...",
                store.manifest.nodes,
                assigned.len()
            );
            let client = client()?;
            let progress = Arc::new(Progress::default());
            let cancel = cancellation();
            let bar =
                ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr_with_hz(1));
            bar.set_style(ProgressStyle::with_template(
                "{spinner:.green} [{bar:30.cyan/blue}] {bytes}/{total_bytes}\n{msg}",
            )?);
            let started = Instant::now();
            let mut last_bytes = 0;
            let mut last_time = started;
            let mut timer = tokio::time::interval(Duration::from_secs(1));
            let task = download(
                &store,
                &client,
                DownloadOptions {
                    node,
                    connections,
                    retries,
                    backoff: Duration::from_millis(backoff_ms),
                },
                cancel,
                progress.clone(),
            );
            tokio::pin!(task);
            let result = loop {
                tokio::select! {
                    result = &mut task => break result,
                    _ = timer.tick() => {
                        let now = Instant::now();
                        let bytes = progress.downloaded_bytes.load(Ordering::Relaxed);
                        let complete_bytes = progress.completed_bytes.load(Ordering::Relaxed);
                        let completed = progress.completed_chunks.load(Ordering::Relaxed);
                        let average = bytes as f64 / started.elapsed().as_secs_f64().max(0.001);
                        let current = bytes.saturating_sub(last_bytes) as f64 / now.duration_since(last_time).as_secs_f64().max(0.001);
                        let eta = if average > 0.0 { format!("{:.0}s", total.saturating_sub(complete_bytes) as f64 / average) } else { "--".into() };
                        bar.set_position(complete_bytes);
                        bar.set_message(format!("Complete {completed}/{} | remaining {} chunks / {} | current {:.2} MiB/s | avg {:.2} MiB/s | retries {} | ETA {eta}", assigned.len(),
                            (assigned.len() as u64).saturating_sub(completed), human_bytes(total.saturating_sub(complete_bytes)),
                            current / 1048576.0, average / 1048576.0, progress.retries.load(Ordering::Relaxed)));
                        tracing::debug!(completed, assigned = assigned.len(), payload_bytes = bytes, average_mib_per_sec = average / 1048576.0, "download progress");
                        last_bytes = bytes;
                        last_time = now;
                    }
                }
            };
            bar.finish_and_clear();
            let report = result?;
            println!(
                "Node complete: {} downloaded, {} resumed, {} retries.\nTransferred {} in {:.2}s ({:.2} MiB/s average).",
                report.downloaded_chunks,
                report.skipped_chunks,
                report.retries,
                human_bytes(report.bytes),
                started.elapsed().as_secs_f64(),
                report.bytes as f64 / 1048576.0 / started.elapsed().as_secs_f64().max(0.001)
            );
        }
        Command::Status { manifest, node } => {
            let store = Store::open(&manifest)?;
            let assigned = store.manifest.assigned_indices(node)?;
            let scan = store.scan()?;
            let complete: std::collections::HashSet<_> = scan.complete.iter().copied().collect();
            let node_complete = assigned.iter().filter(|i| complete.contains(i)).count();
            println!(
                "File: {}\nTotal: {}\nChunks: {}\nNode {node}/{}: {} assigned, {node_complete} complete, {} missing\nCombined availability: {}/{} chunks ({} complete)\nMissing or corrupt indexes: {:?}",
                store.manifest.filename,
                human_bytes(store.manifest.file_size),
                store.manifest.chunk_count,
                store.manifest.nodes,
                assigned.len(),
                assigned.len() - node_complete,
                scan.complete.len(),
                store.manifest.chunk_count,
                human_bytes(scan.complete_bytes),
                scan.missing
            );
        }
        Command::Import { manifest, source } => {
            let report = Store::open(&manifest)?.import(&source)?;
            println!(
                "Imported: {} | Skipped: {} | Rejected: {}",
                report.imported, report.skipped, report.rejected
            );
            if report.rejected > 0 {
                anyhow::bail!("some candidates were rejected; run status to check availability");
            }
        }
        Command::Combine { manifest, output } => {
            let store = Store::open(&manifest)?;
            let output = output
                .map(|path| {
                    if path.components().count() != 1 || path.file_name().is_none() {
                        anyhow::bail!("--output must be a single filename inside the project");
                    }
                    Ok(store.root.join(path))
                })
                .transpose()?;
            display_result(&store.manifest, combine(&store, output.as_deref())?);
        }
        Command::Verify { manifest, file } => {
            let store = Store::open(&manifest)?;
            display_result(&store.manifest, verify(&store, file.as_deref())?);
        }
        Command::Benchmark {
            url,
            connections,
            sample_bytes,
            seconds,
            sha256,
        } => {
            println!(
                "Sampling at most {} per level, at most {seconds}s each (plus one probe byte).\nSmall samples include request overhead; results are estimates.",
                human_bytes(sample_bytes)
            );
            let rows = benchmark(
                &client()?,
                &url,
                &connections,
                sample_bytes,
                seconds,
                sha256,
                cancellation(),
            )
            .await?;
            println!("Connections   MiB/s      Received sample bytes     Seconds    Time cap");
            for row in rows {
                println!(
                    "{:<13} {:<10.2} {:<25} {:<10.2} {}",
                    row.connections, row.mib_per_second, row.bytes, row.seconds, row.timed_out
                );
            }
        }
    }
    Ok(())
}

fn cancellation() -> CancellationToken {
    let token = CancellationToken::new();
    let signal_token = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c()
            .await
            .context("install Ctrl+C handler")
            .is_ok()
        {
            signal_token.cancel();
        }
    });
    token
}

fn display_result(manifest: &Manifest, result: CombineReport) {
    println!("File: {}\nBytes: {}", result.path.display(), result.bytes);
    if let Some(expected) = &manifest.expected_sha256 {
        println!("Expected SHA-256: {expected}");
    }
    println!("Actual SHA-256:   {}", result.sha256);
    println!(
        "{}",
        if result.verified {
            "Verified: matches the supplied trusted SHA-256."
        } else {
            "SHA-256 computed only. No trusted expected hash was supplied; authenticity is unverified."
        }
    );
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", bytes as f64 / 1073741824.0)
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MiB", bytes as f64 / 1048576.0)
    } else if bytes >= 1024 {
        format!("{:.2} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}
