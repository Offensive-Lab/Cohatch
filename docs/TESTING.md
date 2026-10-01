# Testing Cohatch

See [the validation record](VALIDATION.md) for checks actually executed in this
workspace, including the passing 1 GiB test and platform limitations.

The automated HTTP suite binds only to `127.0.0.1` on a randomly assigned port.
It needs no public network, credentials, VPN, or remote server. Downloading Rust
dependencies on the first build does require network access or a populated Cargo
cache.

Run the normal verification sequence from the repository root:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

To see the end-to-end test's original and reconstructed digests:

```powershell
cargo test --test http_integration two_nodes_resume_import_and_combine_match_source_sha256 -- --nocapture
```

The default test streams 16 MiB + 137 bytes. The partial last chunk is deliberate.
The deterministic original's byte at offset `i` is `i % 251`. The test computes
its SHA-256, records it as the trusted expected digest, downloads alternating
chunks into two separate project directories, repeats Node 1 to prove resume
does not issue HTTP requests, shuts down the origin, imports Node 2 twice to
exercise duplicate handling, combines, and verifies the reconstructed file.
Both digests are printed with `--nocapture`.

The independently computed expected digest for the default fixture is
`257df51527fb27701c127b6e44bf342db40be8af13724a5d11d79a48a43fcff3`.

The larger opt-in test performs the same workflow with 1 GiB + 137 bytes and
16 MiB chunks. Source generation, HTTP responses, and hashing use small buffers;
the original is generated on demand rather than held in memory or stored as a
second full-size fixture. Allow roughly 2.6 GiB of free temporary disk space for
the two nodes, imported chunks, and combined output. Use release mode to make
the repeated integrity passes practical:

```powershell
cargo test --release --test http_integration large_one_gib_two_node_streaming_round_trip -- --ignored --nocapture
```

The 1 GiB + 137-byte fixture's expected digest is
`e77fee44be6d8fc214232a29c6d7e751630fe36fc430f8d0cc62d942c9b21bcd`.

The suite also checks:

- A real one-byte GET probe, exact range validation, identity content encoding,
  and `If-Range` on download requests when a usable validator is available.
  Sources without validators require a trusted final SHA-256 instead.
- Servers returning HTTP 200, incorrect range bounds, incorrect total size,
  encoded content, or a changed ETag. None may publish a complete chunk.
- Connection drops, truncated responses, transient HTTP 500, bounded retries,
  and HTTP 429 with a measured `Retry-After` delay.
- Delayed headers exceeding the configured request timeout.
- Cancellation during an active transfer, preservation of a previously
  completed chunk, and restart without redownloading that chunk.
- Abrupt termination of a real Cohatch child process during a download, followed
  by a CLI restart that keeps the committed chunk and downloads only missing data.
- The CLI create, inspect, status, two-node download, import, combine, and verify
  walkthrough, including a manually copied manifest and offline reconstruction.
- Refusal to combine missing data or overwrite an existing output file.

GitHub Actions is configured to run formatting, strict Clippy, tests, and a
release build on Windows and Linux. Its manual workflow can also run the 1 GiB
test. A workflow file is a repeatable check, not evidence that a hosted CI run
has occurred.

## Physical failure checks

Socket drops and cancellation-token tests simulate the data-integrity effects
of network failure and Ctrl+C. A separate test actually kills a child process
through the operating system. These do **not** prove that a specific VPN adapter,
Windows console signal, router, disk controller, or power-loss event behaves
correctly. Validate these manually against a disposable project and a server
whose file is stable and has a trusted SHA-256:

1. Start a multi-chunk download and allow at least one chunk to finish. Record
   its filename, size, modification time, and SHA-256.
2. Disconnect the network or VPN during another chunk. Reconnect, including a
   different IP address if available, and restart the same download command.
   The finished chunk must remain valid and keep its modification time. The
   incomplete chunk must be retried. A changed remote validator must stop work.
3. Repeat with Ctrl+C. Check that the process exits cleanly and that restarting
   skips committed chunks. Repeat separately with `Stop-Process -Force` (or a
   process termination from Task Manager) to exercise abrupt termination.
4. On a disposable quota-limited or nearly full test volume, exhaust disk space
   while a chunk is being written. Existing completed chunks must remain valid;
   the incomplete chunk must not be recognized as complete. Free space and
   restart before combining.
5. Copy the other node's whole project directory via USB or another offline
   medium, import it, combine, and compare against the original trusted SHA-256.

Do not perform destructive disk-full or power-loss testing on a production
volume. Report physical checks as untested until actually performed; successful
simulations are narrower evidence.
