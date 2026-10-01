# Cohatch v0.1 validation record

Executed locally on 2026-09-30, on Windows 10.0.26200 x86_64 with Rust/Cargo
1.98.1 and the `x86_64-pc-windows-gnu` target.

## Completed checks

| Check | Result |
|---|---|
| `cargo fmt --all -- --check` | Passed |
| `cargo check --all-targets --locked --offline` | Passed |
| `cargo clippy --all-targets --all-features --locked --offline -- -D warnings` | Passed |
| `cargo test --locked --offline` | 32 unit and 15 integration tests passed; large test deliberately ignored here |
| Explicit large integration test in release mode | Passed |
| `cargo build --release --locked --offline` | Passed |
| Release executable with only Windows system directories on PATH | Version, help, and example-manifest inspection passed |

**48 distinct tests passed**, including the explicitly enabled large test.
The suite exercises both node assignments locally, import, reconstruction,
resume, source consistency, and failure handling. See [TESTING.md](TESTING.md)
for the coverage details and commands.

## Reconstruction evidence

The normal two-node round trip reconstructed 16,777,353 bytes. Original and
reconstructed SHA-256 both equaled:

```text
257df51527fb27701c127b6e44bf342db40be8af13724a5d11d79a48a43fcff3
```

The explicit large round trip reconstructed 1,073,741,961 bytes (1 GiB + 137
bytes). Original and reconstructed SHA-256 both equaled:

```text
e77fee44be6d8fc214232a29c6d7e751630fe36fc430f8d0cc62d942c9b21bcd
```

The large test completed in 39.34 seconds, excluding compilation. This is a
local fixture result, not an Internet throughput measurement. Its command was:

```powershell
cargo test --release --locked --offline --test http_integration large_one_gib_two_node_streaming_round_trip -- --ignored --nocapture
```

## Release artifact

`dist/cohatch.exe` is 4,221,440 bytes. Its SHA-256 is:

```text
8e3c7bfaf9dd7885308d1702ac920eafcb218d628cd97f964fda6f1dbc4cd76c
```

Archive and executable checksums are supplied in `dist/SHA256SUMS.txt`.
The local portable build needed a GNU assembler; the executable smoke check
did not need Rust or compiler directories on PATH. Standard Windows build
instructions are in the [README](../README.md).

## Validation boundaries

Both simulated PCs ran on one Windows host. Physical second-PC operation,
USB transfer, actual VPN/IP switching, Windows console Ctrl+C delivery,
disk exhaustion, and power loss were not performed. Automated tests cover
the documented simulated failures, including a real child-process kill;
these do not replace physical hardware and network testing. No 100 GB
performance result is claimed.

Linux, macOS, and Windows MSVC builds were not executed here. Windows/Linux
CI is configured, but no hosted CI result is claimed.
