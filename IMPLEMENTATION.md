# Cohatch v0.1 implementation contract

Core library and separate clap CLI, stable Rust/Tokio, reqwest (Windows native TLS;
rustls on other platforms) with
transparent content decoding disabled. One-based CLI node numbering.

Invariants: accept only exact 206 ranges and identity encoding; use strong ETag
or Last-Modified with If-Range, check validators on every response; fail closed
without validators unless a trusted final SHA-256 is supplied. Manifest ID is a
deterministic digest of its identity/layout. Final chunks require a matching
receipt with download ID, index, length and locally computed SHA-256. Hashes
detect local corruption, not original authenticity. Use exclusive directory
locks for mutation. Stream all large data; bound futures by concurrency. Chunk
receipts and data are flushed before publication; interrupted writes never
count as complete. Combine streams into a temporary file and never overwrites.

## Shared API

`manifest.rs`: `Source { etag: Option<String>, last_modified: Option<String>,
content_type: Option<String>, final_url: String, accept_ranges: bool }`;
`Manifest { format: String, version: u32, download_id: String, url: String,
filename: String, file_size: u64, chunk_size: u64, chunk_count: u64,
nodes: u32, source: Source, expected_sha256: Option<String> }`.
`Manifest::new(url, filename, file_size, chunk_size, nodes, source, expected_sha256)
-> Result<Self>` (String inputs); `validate`, `load(&Path)`, `save(&Path)`;
`range(index) -> Result<(u64,u64)>`, `chunk_len(index) -> Result<u64>`,
`assigned(index,node: u32) -> bool`, `assigned_indices(node) -> Result<Vec<u64>>`.
`sanitize_filename(&str) -> String`, `parse_size(&str) -> Result<u64>`.

`storage.rs`: `Store::open(manifest_path: &Path) -> Result<Store>` loads manifest
and uses its parent as root, creates no files; `Store::create(root: &Path,
manifest: &Manifest) -> Result<Store>` safely creates project; `Store` is Clone
with public `manifest: Manifest`, `root: PathBuf`.
`Store::lock() -> Result<StoreLock>` exclusive mutation lock;
`chunk_path(index) -> PathBuf`, `temp_path(index) -> PathBuf`;
`prepare() -> Result<()>` creates chunks dir safely (caller holds lock);
`validate_chunk(index) -> Result<bool>` verifies length, receipt, hash;
`scan() -> Result<Scan>` with `complete: Vec<u64>, missing: Vec<u64>,
complete_bytes: u64`; `commit(index, temp_path: &Path, sha256: &str) -> Result<()>`
publishes synced staged file and receipt (caller holds lock);
`import(source: &Path) -> Result<ImportReport>` takes own destination lock;
source directory must contain manifest.json or be its chunks/ child.
`ImportReport { imported: u64, skipped: u64, rejected: u64 }`.

`combine.rs`: `combine(store: &Store, output: Option<&Path>) -> Result<CombineReport>`
takes own lock; output defaults to root/manifest.filename and supplied output
must remain inside root (filename only CLI). `CombineReport { path: PathBuf,
sha256: String, verified: bool, bytes: u64 }`.
`verify(store: &Store, file: Option<&Path>) -> Result<CombineReport>`.

`http.rs`: `HttpOptions { timeout: Duration, user_agent: String }` Default;
`HttpClient::new(options: HttpOptions) -> Result<Self>` Clone;
`probe(&self,url:&str) -> Result<Probe>` with `Probe { file_size:u64, source:Source,
filename:String }`; `range(&self, manifest:&Manifest,index:u64) -> Result<Response>`
validates headers before returning streaming response.

`download.rs`: `DownloadOptions { node:u32, connections:usize, retries:u32,
backoff:Duration }` Default; `Progress` fields public u64 atomics: downloaded_bytes,
completed_chunks, completed_bytes, retries; `download(store:&Store,
client:&HttpClient, options:DownloadOptions, cancel:CancellationToken,
progress:Arc<Progress>) -> Result<DownloadReport>`;
`DownloadReport { downloaded_chunks:u64, skipped_chunks:u64, bytes:u64, retries:u64 }`.
Download owns lock, scans validated local chunks, initializes progress with
assigned complete chunks; downloaded_bytes counts this session payload.

Integration tests use own local TCP HTTP range server and these APIs, entirely
offline. Test harness can generate deterministic bytes on demand for large tests.
