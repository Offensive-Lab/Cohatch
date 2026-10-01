# Cohatch v0.1 — user handbook

This handbook covers the Windows command-line release, running one or two PCs,
transferring work, resuming, reconstruction, and every available command.

## 1. What Cohatch does

Cohatch downloads a large HTTP/HTTPS file in chunks. A **project** is a folder
containing a manifest and its downloaded chunks. The **manifest** records the
source, file size, chunk layout, node count, and optional trusted SHA-256.
A **node** is a download assignment, normally handled by one PC.

With two nodes, PC A downloads chunks 0, 2, 4, … and PC B downloads chunks
1, 3, 5, … . After transferring B's chunks to A, Cohatch imports and combines
them into the original file. Each chunk has a small `.chunk.json` integrity
receipt; always transfer these with the chunk data.

**There is no live PC pairing, account, pairing code, or Cohatch server.** The
PCs coordinate by using identical copies of one manifest. They can download
simultaneously or at different times and do not need to be on the same LAN.
Transfer the completed project through a USB drive, external SSD, or an
existing file-sharing connection. Cohatch does not configure that connection.

```text
                    Same HTTP/HTTPS file
                       /           \
               PC A: node 1     PC B: node 2
                       ^           |
                       |  transfer B's project
                       +-----------+
                       |
                  import → combine → verify
                       |
                  Complete original file
```

The application is a CLI: run commands in PowerShell. Double-clicking the EXE
does not open a download window. v0.1 supports one or two nodes.

## 2. Install and run

### Use the supplied executable

1. Extract `cohatch-0.1.0-windows-x64.zip` on each Windows x64 PC.
2. Put `cohatch.exe` in a convenient folder, such as `C:\Cohatch`.
3. Open PowerShell and run:

```powershell
$cohatch = 'C:\Cohatch\cohatch.exe'
& $cohatch --version
& $cohatch --help
```

Expected version: `cohatch 0.1.0`. The `&` tells PowerShell to execute the path.
Rust and a compiler are not required to run the supplied executable. Ordinary
download usage does not require administrator privileges; use writable folders.

On the original development PC, you can run the existing binary directly:

```powershell
$cohatch = 'C:\Project Cohatch\dist\cohatch.exe'
& $cohatch --help
```

The examples below assume `$cohatch` has been set to your executable. Variables
exist only in the current PowerShell session. Set them again in a new window.
Quote paths containing spaces. If you change the working directory to the
executable folder, ` .\cohatch.exe --help` is another way to run it.

### Optional release checksum check

The delivered `dist\SHA256SUMS.txt` lists hashes for the EXE and both ZIP files.
Compare the downloaded file's hash with the corresponding entry:

```powershell
Get-FileHash -LiteralPath 'C:\Cohatch\cohatch.exe' -Algorithm SHA256
```

The delivered executable's SHA-256 is:

```text
8e3c7bfaf9dd7885308d1702ac920eafcb218d628cd97f964fda6f1dbc4cd76c
```

## 3. Prepare a download

You need a direct, stable file URL. A browser page containing a Download button
is usually not the file URL. The source must provide a known, nonzero file
size and correct HTTP byte-range responses. Both PCs must be able to access
the same unchanged object.

Obtain the publisher's SHA-256 when available. It must be 64 hexadecimal
characters for this exact file/version. A locally computed hash without a
trusted reference does not establish authenticity. Cohatch accepts a source
without a trusted hash only if it provides a usable HTTP consistency validator.

Use a separate project folder for every download. Do not edit the manifest
after creation. The bundled example manifest is illustrative, not a working
download URL.

Plan disk space for a final file of size **S**:

| Location | Approximate storage needed |
|---|---|
| PC A after import and combine | 2 × S for all chunks plus reconstructed file, with extra room for temporary work |
| PC B | Roughly S/2 for its chunks, plus temporary work; assignment may be uneven by a chunk |
| Transfer drive | Roughly the size of B's completed chunks and receipts |
| PC A if B's transfer is also staged locally | About another S/2 on top of the combining requirement |

Use a transfer filesystem that can hold the files you copy. The reconstructed
output is one large file even though the download uses small chunks.

## 4. One-PC walkthrough

Use this when one PC will download the entire file. Choose a new project path.
These prompts let you paste the real URL and checksum without editing a command.

```powershell
$job = 'C:\Downloads\cohatch-single'
$url = Read-Host 'Paste the direct file URL'
$sha = Read-Host 'Paste the trusted SHA-256, or press Enter if unavailable'
$createArgs = @('create', $url, '--nodes', '1', '--chunk-size', '64M', '--output-dir', $job)
if ($sha) { $createArgs += @('--sha256', $sha) }
& $cohatch @createArgs
```

Continue only if creation succeeds. Inspect the file name and size:

```powershell
& $cohatch inspect "$job\manifest.json"
& $cohatch download "$job\manifest.json" --node 1 --connections 8
```

After the download reports completion:

```powershell
& $cohatch status "$job\manifest.json" --node 1
& $cohatch combine "$job\manifest.json"
& $cohatch verify "$job\manifest.json"
```

The final file appears inside `$job`, with the filename shown by `inspect`.
Combine already checks the final digest; `verify` independently rereads the
published output. Keep the chunks until you are satisfied with the result.

## 5. Two-PC walkthrough

### Step A — create the shared manifest on PC A

Set `$cohatch` as described above, then choose a new project directory:

```powershell
$job = 'C:\Downloads\cohatch-job'
$url = Read-Host 'Paste the direct file URL'
$sha = Read-Host 'Paste the trusted SHA-256, or press Enter if unavailable'
$createArgs = @('create', $url, '--nodes', '2', '--chunk-size', '64M', '--output-dir', $job)
if ($sha) { $createArgs += @('--sha256', $sha) }
& $cohatch @createArgs
& $cohatch inspect "$job\manifest.json"
```

Check that creation succeeded and inspection shows **Nodes: 2**. This is the
only time you run `create` for this job. Copy its manifest unchanged to PC B.

### Step B — give PC B the manifest

For a USB example, assume the transfer drive is `E:`. Substitute the drive
letter actually assigned on each PC. In File Explorer, create `E:\cohatch-setup`
and copy PC A's `manifest.json` there. Copy the EXE too if B does not have it.

On PC B, install the EXE and open PowerShell:

```powershell
$cohatch = 'C:\Cohatch\cohatch.exe'
$job = 'C:\Downloads\cohatch-job'
New-Item -ItemType Directory -Path $job
Copy-Item -LiteralPath 'E:\cohatch-setup\manifest.json' -Destination "$job\manifest.json"
& $cohatch inspect "$job\manifest.json"
```

Use a new folder on B. If it already contains another job, choose another path.
The folder paths on the PCs may differ; their manifest contents must match.
Compare the **Download ID** printed by `inspect` on A and B. Do not run
`create` again on B or manually change node count, URL, or chunk size.

### Step C — start each assignment

On **PC A**, with its `$cohatch` and `$job` variables set:

```powershell
& $cohatch download "$job\manifest.json" --node 1 --connections 8
```

On **PC B**:

```powershell
& $cohatch download "$job\manifest.json" --node 2 --connections 8
```

Leave the terminals open while downloading. A and B can run concurrently.
Each contacts the source independently using its own network connection.
No Cohatch inbound port, IP address entry, or firewall port forwarding is needed.
Two PCs using the same Internet link still share that link's capacity.

Node completion means that node's assignment is complete, not that the whole
file is present on that PC. On B, `status --node 2` can show zero missing for
node 2 while still listing node 1's indexes under combined missing chunks.

### Step D — transfer B's completed project

Wait for B's download to finish and exit, or stop it with Ctrl+C and wait for
the prompt before copying. Close any other process modifying the project.

On B, check its assignment:

```powershell
& $cohatch status "$job\manifest.json" --node 2
```

Copy the **whole** `C:\Downloads\cohatch-job` folder to the transfer drive,
naming the copy `E:\cohatch-from-B`. The important contents are:

```text
E:\cohatch-from-B\
  manifest.json
  chunks\
    00000001.chunk
    00000001.chunk.json
    00000003.chunk
    00000003.chunk.json
    ...
```

Copy both `.chunk` and `.chunk.json` files. Do not copy only the manifest or
only the large chunk files. Keep this transferred folder separate from A's
project. You will use `import` to validate and merge it.

### Step E — import on PC A

Attach the transfer drive to A. Wait for A's own download to exit before import.
Set `$job` to A's project if you opened a new terminal, then run:

```powershell
& $cohatch import "$job\manifest.json" 'E:\cohatch-from-B'
& $cohatch status "$job\manifest.json" --node 1
& $cohatch status "$job\manifest.json" --node 2
```

Import validates the matching manifest and each candidate's size and hash.
It copies valid chunks, skips identical ones, and reports rejected candidates.
It does not delete the transfer copy. You can safely repeat an import.

Before combining, **Combined availability** must show all chunks complete and
**Missing or corrupt indexes** must be `[]`. If chunks are missing, resume
the owning node, transfer the new results, and import again.

### Step F — reconstruct and verify on PC A

```powershell
& $cohatch combine "$job\manifest.json"
& $cohatch verify "$job\manifest.json"
```

Import, status, combine, and verify work offline. The final file is placed
beside the manifest. With a trusted digest, success includes:

```text
Verified: matches the supplied trusted SHA-256.
```

Without one, the command reports a computed SHA-256 and says authenticity is
unverified. This is expected for a manifest created without `--sha256`.

### Alternative transfer: an existing network share

If B's finished project is already available through a Windows share, use
File Explorer to copy it into a separate staging directory on A, for example
`C:\Transfers\cohatch-from-B`, then import that local copy:

```powershell
& $cohatch import "$job\manifest.json" 'C:\Transfers\cohatch-from-B'
```

Use your existing sharing credentials and permissions. Copy after B stops
writing. Both PCs should keep their active project on their own local disk.
Network sharing is just a transfer method; Cohatch does not pair the PCs or
automatically synchronize folders. USB is sufficient if no share exists.

## 6. Stop, resume, and recover

**Pause:** press Ctrl+C in the download terminal and wait for it to return.
Cohatch cancels requests and lets already-started chunk publication finish.
There is no separate pause command.

**Resume:** rerun the same download command with the same manifest and node:

```powershell
& $cohatch download "$job\manifest.json" --node 2 --connections 8
```

Valid completed chunks are rehashed and skipped. Incomplete chunks restart
from their beginning. The initial disk scan can take time on large jobs.
You can change concurrency or retry settings on resume without recreating
the manifest. Use node 1 when resuming A's assignment.

After a network interruption or reboot, restore connectivity and rerun the
command. Cohatch does not manage VPNs. A route/IP change can be tolerated only
if the source URL remains accessible and all recorded source checks still match.

If B becomes unavailable, A can finish B's assignment locally after its own
download exits:

```powershell
& $cohatch download "$job\manifest.json" --node 2 --connections 8
```

Import any available B chunks first to avoid downloading them again. Node IDs
are assignments, not hardware identities. Do not change a two-node manifest
to one node. Run one mutating command at a time in each project directory.

If the source's validators or final redirect change, create a fresh project
in a different folder. Do not patch the old manifest to force a resume. Expired
signed links may require starting a new project in v0.1.

## 7. Complete command reference

Examples assume `$cohatch` and `$job` are set. Get built-in help with
`& $cohatch COMMAND --help`, substituting the command name.

| Command | Purpose | Source Internet needed? |
|---|---|---|
| `create URL` | Probe source and create a manifest | Yes |
| `inspect MANIFEST` | Display source and layout | No |
| `download MANIFEST` | Download one assignment or resume | Yes for missing chunks |
| `status MANIFEST` | Hash local chunks and report availability | No |
| `import MANIFEST SOURCE` | Copy validated transferred chunks | No |
| `combine MANIFEST` | Reconstruct and check the final file | No |
| `verify MANIFEST` | Reread the final file and check its digest | No |
| `benchmark URL` | Compare bounded concurrency samples | Yes |

### Global options

These options can be used with commands:

| Option | Default | Meaning |
|---|---|---|
| `--verbose`, `-v` | Off | Timestamped debug output on stderr |
| `--timeout-seconds N` | `1800` | Total deadline per HTTP request, including its body |
| `--user-agent TEXT` | `Cohatch/0.1.0` | HTTP User-Agent |
| `--help`, `-h` | — | Help for the application or command |
| `--version`, `-V` | — | Application version |

### create

```powershell
& $cohatch create $url --nodes 2 --chunk-size 64M --filename 'large.bin' --output-dir $job --sha256 $sha
```

This example requires `$sha` to contain a trusted digest; omit `--sha256 $sha`
when unavailable. The prompt-based walkthroughs handle that omission for you.

| Option | Default | Meaning |
|---|---|---|
| `--nodes` | `1` | `1` or `2` node assignments |
| `--chunk-size` | `64M` | Chunk size, maximum 1 GiB |
| `--sha256` | None | Trusted final SHA-256 |
| `--filename` | Derived from source | Safe output basename |
| `--output-dir` | `downloads/<download-id>` | New empty project directory |

Bare size numbers mean bytes. `64M` and `64MiB` mean 67,108,864 bytes;
`64MB` means 64,000,000 bytes. At most 1,000,000 chunks are allowed. Smaller
chunks reduce repeated work after interruptions but increase file counts.

### inspect and status

```powershell
& $cohatch inspect "$job\manifest.json"
& $cohatch status "$job\manifest.json" --node 1
```

`inspect` reads manifest metadata. `status` checks receipts, sizes, and hashes;
it can take time because it reads the local data. `--node` defaults to 1.
Combined availability refers to every chunk in the local project, regardless
of assignment. Prefer running status while downloads/imports are idle for
stable counts. It does not repair missing data.

### download

```powershell
& $cohatch download "$job\manifest.json" --node 1 --connections 8 --retries 5 --backoff-ms 1000
```

| Option | Default | Meaning |
|---|---|---|
| `--node` | `1` | Assignment to download; must exist in manifest |
| `--connections` | `8` | Concurrent chunk requests; positive integer |
| `--retries` | `5` | Additional attempts per failed chunk; 0–100 |
| `--backoff-ms` | `1000` | Initial retry delay; 1–3,600,000 ms |

Five retries allow up to six attempts per chunk. Transient network errors,
408, 429, and 5xx can retry with exponential delay and jitter. Retry-After can
pause new requests across workers; in-flight requests may finish. Permanent
errors and invalid ranges stop the run. The ordinary request timeout defaults
to 30 minutes, with idle-read and connection limits capped at 60 and 15 seconds.

### import

```powershell
& $cohatch import "$job\manifest.json" 'E:\cohatch-from-B'
# Equivalent source form, when its parent contains the matching manifest:
& $cohatch import "$job\manifest.json" 'E:\cohatch-from-B\chunks'
```

The first argument identifies the destination project. The second identifies
the transferred source project or its chunks folder. Any rejection makes the
exit code nonzero even if other chunks were successfully imported. Check the
counts and run status before proceeding.

### combine and verify

```powershell
& $cohatch combine "$job\manifest.json"
& $cohatch verify "$job\manifest.json"
```

To use another output name:

```powershell
& $cohatch combine "$job\manifest.json" --output 'large-copy.bin'
& $cohatch verify "$job\manifest.json" --file 'large-copy.bin'
```

`--output` must be a single filename inside the project, not an external path.
`--file` defaults to the manifest filename; use a basename inside the project
as shown. Existing outputs are never overwritten. Combine requires every
chunk, rechecks them while copying, and publishes only after successful checks.
Verify checks output length and computes SHA-256; it does not need the chunks.

### benchmark

```powershell
$url = Read-Host 'Paste the direct file URL to sample'
& $cohatch benchmark $url
& $cohatch benchmark $url --connections 1,4,8,16 --sample-bytes 8M --seconds 5
```

| Option | Default | Meaning |
|---|---|---|
| `--connections` | `1,2,4,8,16,32,64` | Comma-separated levels, at most 16; each 1–128 |
| `--sample-bytes` | `8M` | Payload budget per level; maximum 64 MiB |
| `--seconds` | `5` | Time budget per level; 1–60 seconds |
| `--sha256` | None | Trusted digest permitting a source without usable validators |

Sampling consumes real network data and does not contribute chunks to your
project. Each level stops at its time or payload cap. Defaults sample up to
about 56 MiB plus the probe byte. Samples do not verify the complete file's
SHA-256. The benchmark stops on HTTP errors and does not retry them.

Start normal downloads at 8 connections. Try 4, 8, or 16 based on actual
results; more connections do not guarantee higher speed. Sample measurements
include startup overhead and are not a forecast for a long download.

## 8. Progress, logging, and scripting

The interactive display shows completed/remaining chunks and bytes, current
and average transfer speed, retry count, and estimated time remaining.
Resumed chunks count as completed bytes; transfer rates include data received
during failed attempts. ETA can fluctuate. No live combined two-PC dashboard
exists; inspect each PC separately, then check combined availability after import.

Capture diagnostic logs in PowerShell:

```powershell
& $cohatch download "$job\manifest.json" --node 1 --verbose 2> "$job\cohatch-debug.log"
$downloadExit = $LASTEXITCODE
Write-Output "Cohatch exit code: $downloadExit"
```

Redirection can remove progress from the terminal because stderr is redirected.
Standard summaries go to stdout. Logs omit query tokens from HTTP errors and
source inspection, but the manifest contains the full URL. Treat signed URLs
in the manifest as private.

| Exit code | Meaning |
|---|---|
| `0` | Command succeeded |
| `1` | Operational failure; read the message |
| `2` | Invalid command-line arguments |
| `130` | Download cancelled with Ctrl+C |

Read `$LASTEXITCODE` immediately after the Cohatch command when scripting.
Do not automatically combine after a failed download or rejected import.

## 9. Troubleshooting

| Symptom | What to do |
|---|---|
| EXE window opens and closes | Run it from PowerShell with a command, such as `--help`. |
| Command not found | Set `$cohatch` to the real EXE path and use `& $cohatch`. |
| Manifest not found | Check `$job`, the filename, and whether you copied the manifest to this PC. |
| Node 2 invalid | This manifest was created for one node. For two PCs, create a new two-node project. |
| Create rejects the URL/ranges | Use a direct file URL whose server supports exact byte ranges; a web page is insufficient. |
| No usable validator | Supply a trusted SHA-256 at creation or use another source. |
| HTTP 401/403 or expired link | Obtain an accessible direct link; service-specific login/cookie extraction is unsupported. |
| Source or redirect changed | Create a fresh project; do not edit identity metadata. |
| Resume looks idle | Allow the initial hash scan to finish. |
| Directory is in use | Wait for the other modifying command to exit. A lock file's presence alone does not prove a running process. |
| Import rejects chunks | Confirm the same manifest and transfer both data and receipts again; inspect reported counts. |
| Node complete but combined chunks missing | Import the other node, or run its assignment locally. |
| Missing/corrupt indexes | Resume their owning node; with two nodes, even indexes belong to node 1, odd to node 2. |
| SHA-256 mismatch | Do not treat the output as verified. Confirm the publisher digest and source version. |
| Output already exists | Verify it, or combine with a different `--output` basename. |
| Disk full | Free sufficient space and resume; also allow space for the full reconstructed output. |
| HTTP 429 or poor speed | Reduce concurrency and allow cooldowns; raising retries is not a bandwidth improvement. |
| File remains unverified | No trusted hash was supplied. Compare the printed hash against a trusted publisher value. |

## 10. Storage, cleanup, and limitations

A typical local project is:

```text
cohatch-job/
  manifest.json
  .cohatch.lock
  chunks/
    00000000.chunk
    00000000.chunk.json
    ...
  large.bin                  # after successful combine
```

After verifying and backing up the final output, you may remove chunks to
recover space if you no longer need resume/reconstruction. Keep the manifest
with the output for later `verify`. Removing chunks makes `status` report them
missing; it does not invalidate a separately verified final file. If you move
the final file elsewhere, retain its trusted hash for independent verification.

A hard kill may leave `.cohatch-*.tmp` files. They do not count as completed
chunks. Remove stale temporary files only when no process is using the project.
Avoid renaming chunk files, changing receipts, or hand-editing the manifest.

v0.1 has no GUI, background service, scheduled downloading, live synchronization,
automatic inter-PC transfer, partial-chunk resume, mirrors, VPN management,
browser-cookie import, or service-specific source integrations. It cannot
guarantee a particular speed or bypass a shared connection's bandwidth limit.

The release passed 48 tests, including local two-node reconstruction of a
1 GiB fixture with matching SHA-256. Physical two-PC/VPN/power-loss checks were
not performed. See [VALIDATION.md](VALIDATION.md) for the precise test scope.

## 11. Build from source (optional)

Running the packaged EXE does not require this section. To modify or rebuild,
extract the source ZIP and install stable Rust with the Windows C++ build
tools and SDK required by the standard MSVC toolchain. In the source folder:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
cargo build --release --locked
.\target\release\cohatch.exe --version
```

The source uses Rust edition 2024 and includes a dependency lockfile. The
first build needs network access for dependencies. The supplied executable
was built with Rust 1.98.1 for Windows x64 GNU.

Only in the original development workspace, which has the optional local
toolchain, load its environment before Cargo commands:

```powershell
Set-Location 'C:\Project Cohatch'
. .\scripts\dev-env.ps1
cargo build --release --locked --offline
```

The `.tools` toolchain is not included in the source ZIP. Do not expect the
helper to provision it on another PC. See [TESTING.md](TESTING.md) for the
larger opt-in test and [manifest.md](manifest.md) for the data format.
