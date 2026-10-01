mod support;

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use cohatch::{
    combine::{combine, verify},
    download::{DownloadOptions, Progress, download},
    http::{HttpClient, HttpOptions},
    manifest::Manifest,
    storage::Store,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use support::{Behavior, Server, source_sha256};

fn client(timeout: Duration) -> HttpClient {
    HttpClient::new(HttpOptions {
        timeout,
        ..HttpOptions::default()
    })
    .unwrap()
}

fn options(node: u32, connections: usize, retries: u32) -> DownloadOptions {
    DownloadOptions {
        node,
        connections,
        retries,
        backoff: Duration::from_millis(10),
    }
}

async fn project(server: &Server, chunk_size: u64, nodes: u32) -> (TempDir, Store, HttpClient) {
    let client = client(Duration::from_secs(10));
    let probe = client.probe(&server.url).await.unwrap();
    assert_eq!(probe.file_size, server.size);
    let manifest = Manifest::new(
        server.url.clone(),
        probe.filename,
        probe.file_size,
        chunk_size,
        nodes,
        probe.source,
        Some(source_sha256(server.size)),
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = Store::create(&directory.path().join("project"), &manifest).unwrap();
    (directory, store, client)
}

async fn run(
    store: &Store,
    client: &HttpClient,
    options: DownloadOptions,
) -> cohatch::Result<cohatch::download::DownloadReport> {
    download(
        store,
        client,
        options,
        CancellationToken::new(),
        Arc::new(Progress::default()),
    )
    .await
}

async fn two_node_round_trip(size: u64, chunk_size: u64) {
    let server = Server::start(size).await;
    let (directory, first, client) = project(&server, chunk_size, 2).await;
    let second = Store::create(&directory.path().join("second"), &first.manifest).unwrap();
    let trusted = first.manifest.expected_sha256.clone().unwrap();
    // Independently computed with Node's crypto SHA-256 over the specified byte pattern.
    if size == 16 * 1024 * 1024 + 137 {
        assert_eq!(
            trusted,
            "257df51527fb27701c127b6e44bf342db40be8af13724a5d11d79a48a43fcff3"
        );
    } else if size == 1024 * 1024 * 1024 + 137 {
        assert_eq!(
            trusted,
            "e77fee44be6d8fc214232a29c6d7e751630fe36fc430f8d0cc62d942c9b21bcd"
        );
    }
    let (a, b) = tokio::join!(
        run(&first, &client, options(1, 4, 2)),
        run(&second, &client, options(2, 4, 2))
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(
        a.downloaded_chunks + b.downloaded_chunks,
        first.manifest.chunk_count
    );
    assert_eq!(a.bytes + b.bytes, size);
    assert_eq!(
        first.scan().unwrap().complete_bytes + second.scan().unwrap().complete_bytes,
        size
    );

    let request_count = server.requests().len();
    let resumed = run(&first, &client, options(1, 4, 2)).await.unwrap();
    assert_eq!(resumed.downloaded_chunks, 0);
    assert_eq!(resumed.skipped_chunks, a.downloaded_chunks);
    assert_eq!(resumed.bytes, 0);
    assert_eq!(
        server.requests().len(),
        request_count,
        "resume must not contact the origin for valid chunks"
    );

    let requests = server.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(
        requests[0].range, "bytes=0-0",
        "probe must verify a real one-byte response"
    );
    for request in &requests[1..] {
        assert_eq!(request.method, "GET");
        assert_eq!(request.if_range.as_deref(), Some("\"cohatch-test-v1\""));
        assert_eq!(request.accept_encoding.as_deref(), Some("identity"));
    }
    // Import, combine and verify must work after the origin is gone.
    drop(server);
    let imported = first.import(&second.root).unwrap();
    assert_eq!(imported.imported, b.downloaded_chunks);
    assert_eq!(imported.rejected, 0);
    let duplicate = first.import(&second.root.join("chunks")).unwrap();
    assert_eq!(duplicate.imported, 0);
    assert_eq!(duplicate.skipped, b.downloaded_chunks);
    assert!(first.scan().unwrap().missing.is_empty());
    let combined = combine(&first, None).unwrap();
    println!("source SHA-256:        {trusted}");
    println!(
        "reconstructed SHA-256: {} ({} bytes)",
        combined.sha256, combined.bytes
    );
    assert_eq!(combined.sha256, trusted);
    assert_eq!(combined.bytes, size);
    assert!(combined.verified);
    let verified = verify(&first, Some(&combined.path)).unwrap();
    assert_eq!(verified.sha256, trusted);
    assert!(verified.verified);
    assert!(
        combine(&first, None).is_err(),
        "combine must never overwrite an existing output"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_resume_import_and_combine_match_source_sha256() {
    // A deliberately partial final chunk exercises the terminal range.
    two_node_round_trip(16 * 1024 * 1024 + 137, 1024 * 1024).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in 1 GiB disk and SHA-256 streaming correctness test"]
async fn large_one_gib_two_node_streaming_round_trip() {
    two_node_round_trip(1024 * 1024 * 1024 + 137, 16 * 1024 * 1024).await;
}

#[tokio::test]
async fn probe_refuses_ignored_range_and_malformed_content_range() {
    for behavior in [
        Behavior::IgnoreRange,
        Behavior::WrongRange,
        Behavior::Encoded,
        Behavior::MissingRange,
        Behavior::WrongLength,
        Behavior::ExtraChunkedBody,
    ] {
        let server = Server::start(4096).await;
        server.behavior(behavior.clone());
        let error = client(Duration::from_secs(2)).probe(&server.url).await;
        assert!(error.is_err(), "unsafe probe accepted: {behavior:?}");
    }
}

#[tokio::test]
async fn unsafe_responses_never_publish_chunks_and_are_not_retried() {
    for behavior in [
        Behavior::IgnoreRange,
        Behavior::WrongRange,
        Behavior::WrongTotal,
        Behavior::Encoded,
        Behavior::ChangedEtag,
        Behavior::ChangedLastModified,
        Behavior::NoValidators,
        Behavior::MissingRange,
        Behavior::WrongLength,
        Behavior::ExtraChunkedBody,
    ] {
        let server = Server::start(65536).await;
        let (_directory, store, client) = project(&server, 65536, 1).await;
        server.behavior(behavior.clone());
        let error = run(&store, &client, options(1, 1, 3)).await;
        assert!(error.is_err(), "unsafe range accepted: {behavior:?}");
        assert!(store.scan().unwrap().complete.is_empty());
        assert_eq!(
            server.requests().len(),
            2,
            "fatal consistency errors must stop immediately: {behavior:?}"
        );
    }
}

#[tokio::test]
async fn last_modified_is_used_when_strong_etag_is_absent() {
    let server = Server::start(65536).await;
    server.behavior(Behavior::LastModifiedOnly);
    let (_directory, store, client) = project(&server, server.size, 1).await;
    assert!(store.manifest.source.etag.is_none());
    run(&store, &client, options(1, 1, 0)).await.unwrap();
    assert_eq!(
        server.requests()[1].if_range.as_deref(),
        Some("Wed, 01 Jan 2025 00:00:00 GMT")
    );
    assert!(store.validate_chunk(0).unwrap());
}

#[tokio::test]
async fn source_without_validators_requires_and_verifies_trusted_final_hash() {
    let server = Server::start(65536).await;
    server.behavior(Behavior::NoValidators);
    let (_directory, store, client) = project(&server, server.size, 1).await;
    assert!(store.manifest.source.etag.is_none());
    assert!(store.manifest.source.last_modified.is_none());
    let manifest = &store.manifest;
    assert!(
        Manifest::new(
            manifest.url.clone(),
            manifest.filename.clone(),
            manifest.file_size,
            manifest.chunk_size,
            manifest.nodes,
            manifest.source.clone(),
            None,
        )
        .is_err()
    );
    run(&store, &client, options(1, 1, 0)).await.unwrap();
    assert!(server.requests()[1].if_range.is_none());
    let combined = combine(&store, None).unwrap();
    assert!(combined.verified);
    assert_eq!(combined.sha256, source_sha256(server.size));
}

#[tokio::test]
async fn dropped_and_truncated_connections_retry_without_publishing_partial_data() {
    for behavior in [
        Behavior::DropOnce,
        Behavior::TruncateOnce,
        Behavior::ShortChunkedOnce,
        Behavior::StatusOnce(500),
    ] {
        let server = Server::start(256 * 1024 + 19).await;
        let (_directory, store, client) = project(&server, server.size, 1).await;
        server.behavior(behavior.clone());
        let report = run(&store, &client, options(1, 1, 2)).await.unwrap();
        assert_eq!(report.downloaded_chunks, 1);
        assert!(
            report.retries >= 1,
            "fault did not exercise retries: {behavior:?}"
        );
        assert!(store.validate_chunk(0).unwrap());
        let combined = combine(&store, None).unwrap();
        assert_eq!(combined.sha256, source_sha256(server.size));
    }
}

#[tokio::test]
async fn truncated_body_with_no_retries_is_missing_on_resume() {
    let server = Server::start(65536).await;
    let (_directory, store, client) = project(&server, server.size, 1).await;
    server.behavior(Behavior::TruncateOnce);
    assert!(run(&store, &client, options(1, 1, 0)).await.is_err());
    assert!(!store.validate_chunk(0).unwrap());
    assert!(combine(&store, None).is_err());
    server.behavior(Behavior::Correct);
    assert_eq!(
        run(&store, &client, options(1, 1, 0))
            .await
            .unwrap()
            .downloaded_chunks,
        1
    );
    assert!(store.validate_chunk(0).unwrap());
}

#[tokio::test]
async fn retry_after_is_honored() {
    let server = Server::start(65536).await;
    let (_directory, store, client) = project(&server, server.size, 1).await;
    server.behavior(Behavior::StatusOnce(429));
    let report = run(&store, &client, options(1, 1, 2)).await.unwrap();
    assert_eq!(report.retries, 1);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[2].at.duration_since(requests[1].at) >= Duration::from_millis(950),
        "Retry-After: 1 was ignored"
    );
    assert!(store.validate_chunk(0).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_after_pauses_new_requests_across_workers() {
    let server = Server::start(8 * 65536).await;
    let (_directory, store, client) = project(&server, 65536, 1).await;
    server.behavior(Behavior::StatusOnce(429));
    let report = run(&store, &client, options(1, 3, 2)).await.unwrap();
    assert_eq!(report.downloaded_chunks, 8);
    assert_eq!(report.retries, 1);
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        10,
        "one probe, eight successful chunks, one 429"
    );
    // Up to three requests can already be in flight when the first 429 arrives.
    // Every subsequently queued request must respect the shared deadline.
    for request in &requests[4..] {
        assert!(
            request.at.duration_since(requests[1].at) >= Duration::from_millis(950),
            "another worker ignored the shared Retry-After pause"
        );
    }
    assert!(store.scan().unwrap().missing.is_empty());
}

#[tokio::test]
async fn delayed_response_obeys_timeout_and_retry_budget() {
    let server = Server::start(65536).await;
    let (_directory, store, _) = project(&server, server.size, 1).await;
    server.behavior(Behavior::Delay(Duration::from_secs(2)));
    let short_timeout = client(Duration::from_millis(100));
    let outcome = tokio::time::timeout(
        Duration::from_secs(3),
        run(&store, &short_timeout, options(1, 1, 1)),
    )
    .await
    .expect("request timeout/retry budget must bound elapsed time");
    assert!(outcome.is_err());
    assert_eq!(
        server.requests().len(),
        3,
        "one initial attempt plus one retry"
    );
    assert!(store.scan().unwrap().complete.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_preserves_completed_chunks_and_resume_skips_them() {
    let server = Server::start(4 * 65536).await;
    let (_directory, store, client) = project(&server, 65536, 1).await;
    server.behavior(Behavior::SlowAfter(65536));
    let token = CancellationToken::new();
    let progress = Arc::new(Progress::default());
    let worker_store = store.clone();
    let worker_client = client.clone();
    let worker_token = token.clone();
    let worker_progress = Arc::clone(&progress);
    let worker = tokio::spawn(async move {
        download(
            &worker_store,
            &worker_client,
            options(1, 1, 2),
            worker_token,
            worker_progress,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while progress.completed_chunks.load(Ordering::Relaxed) == 0 || server.requests().len() < 3
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("first chunk should commit before cancellation");
    token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "cancellation should not be reported as a completed download"
    );
    let before_resume = store.scan().unwrap();
    assert_eq!(before_resume.complete, vec![0]);
    assert!(!store.validate_chunk(1).unwrap());
    let requests_before_resume = server.requests().len();
    server.behavior(Behavior::Correct);
    let resumed = run(&store, &client, options(1, 2, 1)).await.unwrap();
    assert_eq!(resumed.skipped_chunks, 1);
    assert_eq!(resumed.downloaded_chunks, 3);
    for request in &server.requests()[requests_before_resume..] {
        assert_ne!(
            request.range, "bytes=0-65535",
            "committed chunk was redownloaded"
        );
    }
    assert_eq!(
        combine(&store, None).unwrap().sha256,
        source_sha256(server.size)
    );
}

#[tokio::test]
async fn invalid_options_fail_before_sending_download_requests() {
    let server = Server::start(65536).await;
    let (_directory, store, client) = project(&server, server.size, 1).await;
    let invalid = [
        options(1, 0, 0),
        options(0, 1, 0),
        options(2, 1, 0),
        options(1, 1, 101),
        DownloadOptions {
            backoff: Duration::ZERO,
            ..options(1, 1, 0)
        },
    ];
    for options in invalid {
        assert!(run(&store, &client, options).await.is_err());
    }
    assert_eq!(server.requests().len(), 1);
    assert!(store.scan().unwrap().complete.is_empty());
    // Concurrency controls active work, independently of a small chunk count.
    assert_eq!(
        run(&store, &client, options(1, 128, 0))
            .await
            .unwrap()
            .downloaded_chunks,
        1
    );
}

#[tokio::test]
async fn benchmark_obeys_even_a_sub_chunk_sample_cap() {
    let server = Server::start(1024 * 1024).await;
    let client = client(Duration::from_secs(2));
    let rows = cohatch::benchmark::benchmark(
        &client,
        &server.url,
        &[1, 4],
        137,
        1,
        None,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(
            row.bytes > 0 && row.bytes <= 137,
            "benchmark exceeded its payload cap: {}",
            row.bytes
        );
        assert!(!row.timed_out);
    }
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "one probe and one bounded range per level"
    );
    for request in &requests[1..] {
        assert_eq!(request.range, "bytes=0-136");
        assert_eq!(request.if_range.as_deref(), Some("\"cohatch-test-v1\""));
    }
}

async fn cli(arguments: &[&str]) -> std::process::Output {
    let arguments: Vec<std::ffi::OsString> = arguments
        .iter()
        .map(|argument| std::ffi::OsString::from(*argument))
        .collect();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_cohatch"))
            .args(arguments)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

async fn cli_success(arguments: &[&str]) -> String {
    let result = cli(arguments).await;
    assert!(
        result.status.success(),
        "CLI failed: {arguments:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abruptly_killed_cli_process_resumes_durable_chunks() {
    let server = Server::start(4 * 65536).await;
    let (_directory, store, _) = project(&server, 65536, 1).await;
    server.behavior(Behavior::SlowAfter(65536));
    let manifest = store.root.join("manifest.json");
    let manifest_arg = manifest.to_str().unwrap();
    let mut child = KillOnDrop(
        std::process::Command::new(env!("CARGO_BIN_EXE_cohatch"))
            .args([
                "download",
                manifest_arg,
                "--node",
                "1",
                "--connections",
                "1",
                "--retries",
                "0",
                "--timeout-seconds",
                "3",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "worker exited before the kill test reached an active chunk"
            );
            if store.validate_chunk(0).unwrap() && server.requests().len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("child must commit one chunk and start another");
    let committed_time = std::fs::metadata(store.chunk_path(0))
        .unwrap()
        .modified()
        .unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert_eq!(store.scan().unwrap().complete, vec![0]);
    let previous_requests = server.requests().len();
    server.behavior(Behavior::Correct);
    let output = cli_success(&[
        "download",
        manifest_arg,
        "--node",
        "1",
        "--connections",
        "2",
        "--retries",
        "0",
        "--timeout-seconds",
        "3",
    ])
    .await;
    assert!(
        output.contains("3 downloaded, 1 resumed"),
        "unexpected resume report: {output}"
    );
    assert_eq!(
        std::fs::metadata(store.chunk_path(0))
            .unwrap()
            .modified()
            .unwrap(),
        committed_time
    );
    for request in &server.requests()[previous_requests..] {
        assert_ne!(request.range, "bytes=0-65535");
    }
    assert_eq!(
        combine(&store, None).unwrap().sha256,
        source_sha256(server.size)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_two_pc_walkthrough_and_offline_commands() {
    let server = Server::start(3 * 65536 + 19).await;
    let temporary = tempfile::tempdir().unwrap();
    let first = temporary.path().join("pc-a");
    let second = temporary.path().join("pc-b");
    let first_manifest = first.join("manifest.json");
    let second_manifest = second.join("manifest.json");
    let first_arg = first_manifest.to_str().unwrap();
    let second_arg = second_manifest.to_str().unwrap();
    let trusted = source_sha256(server.size);
    let output = cli_success(&[
        "create",
        &server.url,
        "--nodes",
        "2",
        "--chunk-size",
        "64K",
        "--sha256",
        &trusted,
        "--output-dir",
        first.to_str().unwrap(),
        "--timeout-seconds",
        "3",
    ])
    .await;
    assert!(output.contains("Created"));
    std::fs::create_dir(&second).unwrap();
    std::fs::copy(&first_manifest, &second_manifest).unwrap();
    assert!(
        cli_success(&["inspect", second_arg])
            .await
            .contains("Nodes: 2")
    );
    assert!(
        cli_success(&["status", first_arg, "--node", "1"])
            .await
            .contains("0 complete")
    );
    let missing = cli(&["combine", first_arg]).await;
    assert!(!missing.status.success());
    assert!(!first.join("deterministic.bin").exists());
    let first_download = [
        "download",
        first_arg,
        "--node",
        "1",
        "--connections",
        "2",
        "--timeout-seconds",
        "3",
    ];
    let second_download = [
        "download",
        second_arg,
        "--node",
        "2",
        "--connections",
        "2",
        "--timeout-seconds",
        "3",
    ];
    let (a, b) = tokio::join!(cli_success(&first_download), cli_success(&second_download));
    assert!(a.contains("2 downloaded"));
    assert!(b.contains("2 downloaded"));
    drop(server);
    let imported = cli_success(&["import", first_arg, second.to_str().unwrap()]).await;
    assert!(imported.contains("Imported: 2"));
    assert!(
        cli_success(&["status", first_arg])
            .await
            .contains("4/4 chunks")
    );
    assert!(
        cli_success(&["combine", first_arg, "--output", "reconstructed.bin"])
            .await
            .contains(&trusted)
    );
    let output = first.join("reconstructed.bin");
    let verified = cli_success(&["verify", first_arg, "--file", output.to_str().unwrap()]).await;
    assert!(verified.contains("Verified: matches the supplied trusted SHA-256"));
    assert!(
        !cli(&["combine", first_arg, "--output", "reconstructed.bin"])
            .await
            .status
            .success()
    );
}
