//! Durable chunk storage. A receipt is the completion marker, and is published
//! only after the chunk and its locally computed digest have been flushed.
use crate::{Error, Result, manifest::Manifest};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) const BUFFER_SIZE: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Store {
    pub manifest: Manifest,
    pub root: PathBuf,
    held: Arc<AtomicBool>,
}

/// Held for the entire operation, including all concurrent download workers.
pub struct StoreLock {
    file: File,
    held: Arc<AtomicBool>,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        self.held.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    pub complete: Vec<u64>,
    pub missing: Vec<u64>,
    pub complete_bytes: u64,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: u64,
    pub skipped: u64,
    pub rejected: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    version: u32,
    download_id: String,
    index: u64,
    length: u64,
    pub(crate) sha256: String,
}

impl Store {
    /// Loading a store performs no writes, so inspect/status also work offline
    /// on read-only media and with only a copied manifest present.
    pub fn open(manifest_path: &Path) -> Result<Self> {
        let path = absolute_path(manifest_path)?;
        check_path(&path)?;
        require_file(&path)?;
        let manifest = Manifest::load(&path)?;
        let root = fs::canonicalize(path.parent().ok_or_else(|| unsafe_path(&path))?)?;
        Ok(Self {
            manifest,
            root,
            held: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Creates a store, refusing an existing incompatible manifest or files at
    /// reserved paths. Existing projects are never silently reinitialized.
    pub fn create(root: &Path, manifest: &Manifest) -> Result<Self> {
        manifest.validate()?;
        let root = absolute_path(root)?;
        check_path(&root)?;
        fs::create_dir_all(&root)?;
        check_path(&root)?;
        let store = Self {
            manifest: manifest.clone(),
            root: fs::canonicalize(root)?,
            held: Arc::new(AtomicBool::new(false)),
        };
        let _lock = store.lock()?;
        let manifest_path = store.root.join("manifest.json");
        check_path(&manifest_path)?;
        match fs::symlink_metadata(&manifest_path) {
            Ok(_) => {
                require_file(&manifest_path)?;
                let existing = Manifest::load(&manifest_path)?;
                if !manifests_equal(&existing, manifest)? {
                    return Err(Error::Integrity(
                        "directory already contains another manifest".into(),
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => manifest.save(&manifest_path)?,
            Err(e) => return Err(e.into()),
        }
        store.prepare()?;
        Ok(store)
    }

    pub fn lock(&self) -> Result<StoreLock> {
        if self
            .held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::Locked);
        }
        let result = (|| {
            check_path(&self.root)?;
            let path = self.root.join(".cohatch.lock");
            check_path(&path)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err(Error::Integrity("lock path must be a regular file".into()));
            }
            file.try_lock_exclusive().map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                {
                    Error::Locked
                } else {
                    Error::Io(error)
                }
            })?;
            Ok(StoreLock {
                file,
                held: self.held.clone(),
            })
        })();
        if result.is_err() {
            self.held.store(false, Ordering::Release);
        }
        result
    }

    fn require_lock(&self) -> Result<()> {
        if !self.held.load(Ordering::Acquire) {
            return Err(Error::InvalidInput(
                "exclusive store lock is required for mutation".into(),
            ));
        }
        check_path(&self.root)
    }

    pub fn chunk_path(&self, index: u64) -> PathBuf {
        self.root.join("chunks").join(format!("{index:08}.chunk"))
    }

    pub fn temp_path(&self, index: u64) -> PathBuf {
        self.root
            .join("chunks")
            .join(format!("{index:08}.chunk.tmp"))
    }

    fn receipt_path(&self, index: u64) -> PathBuf {
        self.root
            .join("chunks")
            .join(format!("{index:08}.chunk.json"))
    }

    pub fn prepare(&self) -> Result<()> {
        self.require_lock()?;
        let path = self.root.join("chunks");
        check_path(&path)?;
        match fs::create_dir(&path) {
            Ok(()) => sync_directory(&self.root)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(&path)?.is_dir() {
                    return Err(unsafe_path(&path));
                }
            }
            Err(e) => return Err(e.into()),
        }
        check_path(&path)
    }

    /// Creates a unique, exclusive staging file. An abandoned staging file is
    /// never interpreted as completed data; no existing path is truncated.
    pub fn stage(&self, index: u64) -> Result<(PathBuf, File)> {
        self.require_lock()?;
        self.manifest.chunk_len(index)?;
        self.prepare()?;
        let temporary = tempfile::Builder::new()
            .prefix(&format!(".cohatch-{index:08}-"))
            .suffix(".tmp")
            .tempfile_in(self.root.join("chunks"))?;
        let (file, path) = temporary.keep().map_err(|e| Error::Io(e.error))?;
        Ok((path, file))
    }

    pub(crate) fn receipt(&self, index: u64) -> Result<Option<Receipt>> {
        let expected = self.manifest.chunk_len(index)?;
        let path = self.receipt_path(index);
        check_path(&path)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() {
            return Err(unsafe_path(&path));
        }
        if metadata.len() > 4096 {
            return Ok(None);
        }
        let file = File::open(path)?;
        let receipt: Receipt = match serde_json::from_reader(file.take(4097)) {
            Ok(receipt) => receipt,
            Err(e) if e.is_io() => return Err(Error::Json(e)),
            Err(_) => return Ok(None),
        };
        if receipt.version != 1
            || receipt.download_id != self.manifest.download_id
            || receipt.index != index
            || receipt.length != expected
            || !valid_digest(&receipt.sha256)
        {
            return Ok(None);
        }
        Ok(Some(receipt))
    }

    pub fn validate_chunk(&self, index: u64) -> Result<bool> {
        let expected = self.manifest.chunk_len(index)?;
        let path = self.chunk_path(index);
        check_path(&path)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() {
            return Err(unsafe_path(&path));
        }
        if metadata.len() != expected {
            return Ok(false);
        }
        let Some(receipt) = self.receipt(index)? else {
            return Ok(false);
        };
        let (length, digest) = hash_reader(&mut File::open(path)?)?;
        Ok(length == expected && digest == receipt.sha256)
    }

    pub fn scan(&self) -> Result<Scan> {
        let mut scan = Scan::default();
        for index in 0..self.manifest.chunk_count {
            if self.validate_chunk(index)? {
                scan.complete.push(index);
                scan.complete_bytes += self.manifest.chunk_len(index)?;
            } else {
                scan.missing.push(index);
            }
        }
        Ok(scan)
    }

    /// Publishes data first and the receipt last. Either crash interval leaves
    /// an incomplete chunk, never a false positive during resume.
    pub fn commit(&self, index: u64, temp_path: &Path, sha256: &str) -> Result<()> {
        self.require_lock()?;
        let expected = self.manifest.chunk_len(index)?;
        let staged = absolute_path(temp_path)?;
        check_path(&staged)?;
        require_file(&staged)?;
        if fs::canonicalize(staged.parent().ok_or_else(|| unsafe_path(&staged))?)?
            != fs::canonicalize(self.root.join("chunks"))?
            || !staged
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".tmp"))
        {
            return Err(unsafe_path(&staged));
        }
        if !valid_digest(sha256) {
            return Err(Error::Integrity("invalid staged SHA-256".into()));
        }
        let mut file = OpenOptions::new().read(true).write(true).open(&staged)?;
        let (length, actual) = hash_reader(&mut file)?;
        if length != expected || actual != sha256 {
            return Err(Error::Integrity(format!(
                "staged chunk {index} length or SHA-256 mismatch"
            )));
        }
        file.sync_all()?;
        drop(file);
        if self.validate_chunk(index)? {
            if self
                .receipt(index)?
                .is_some_and(|receipt| receipt.sha256 == actual)
            {
                fs::remove_file(&staged)?;
                return Ok(());
            }
            return Err(Error::Integrity(format!(
                "chunk {index} conflicts with an existing valid chunk"
            )));
        }
        let receipt = Receipt {
            version: 1,
            download_id: self.manifest.download_id.clone(),
            index,
            length,
            sha256: actual,
        };
        let mut receipt_temp = tempfile::Builder::new()
            .prefix(".cohatch-receipt-")
            .suffix(".tmp")
            .tempfile_in(self.root.join("chunks"))?;
        #[cfg(test)]
        if FAIL_RECEIPT_WRITE.with(|fail| fail.replace(false)) {
            return Err(io::Error::other("simulated disk full while writing receipt").into());
        }
        serde_json::to_writer(&mut receipt_temp, &receipt)?;
        receipt_temp.write_all(b"\n")?;
        receipt_temp.as_file().sync_all()?;
        // Remove only deterministic chunk paths that failed validation, never
        // truncate them: a hard link cannot cause an unrelated file to change.
        remove_regular_if_present(&self.receipt_path(index))?;
        remove_regular_if_present(&self.chunk_path(index))?;
        let temporary = tempfile::TempPath::try_from_path(staged)?;
        temporary
            .persist_noclobber(self.chunk_path(index))
            .map_err(|e| Error::Io(e.error))?;
        sync_directory(&self.root.join("chunks"))?;
        receipt_temp
            .persist_noclobber(self.receipt_path(index))
            .map_err(|e| Error::Io(e.error))?;
        sync_directory(&self.root.join("chunks"))?;
        tracing::debug!(
            chunk = index,
            bytes = length,
            "chunk committed with local integrity receipt"
        );
        Ok(())
    }

    /// Import requires the source manifest and receipts. Metadata alone cannot
    /// establish that arbitrary loose chunks belong to this remote object.
    pub fn import(&self, source: &Path) -> Result<ImportReport> {
        let _destination_lock = self.lock()?;
        self.prepare()?;
        let source = absolute_path(source)?;
        check_path(&source)?;
        let source_root = if source.file_name().is_some_and(|name| name == "chunks") {
            source
                .parent()
                .ok_or_else(|| unsafe_path(&source))?
                .to_path_buf()
        } else {
            source
        };
        let other = Store::open(&source_root.join("manifest.json"))?;
        if !manifests_equal(&self.manifest, &other.manifest)? {
            return Err(Error::Integrity(
                "import source manifest does not match destination manifest".into(),
            ));
        }
        let _source_lock = if other.root == self.root {
            None
        } else {
            Some(other.lock()?)
        };
        let directory = other.root.join("chunks");
        check_path(&directory)?;
        let mut report = ImportReport::default();
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                report.rejected += 1;
                continue;
            };
            // Receipts and incomplete attempts are not candidate chunks.
            if name.ends_with(".tmp") || name.ends_with(".chunk.json") {
                continue;
            }
            let index = name
                .strip_suffix(".chunk")
                .and_then(|s| s.parse::<u64>().ok());
            let Some(index) = index.filter(|index| {
                *index < self.manifest.chunk_count && name == format!("{index:08}.chunk")
            }) else {
                report.rejected += 1;
                continue;
            };
            if !matches!(other.validate_chunk(index), Ok(true)) {
                report.rejected += 1;
                continue;
            }
            let Some(receipt) = other.receipt(index)? else {
                report.rejected += 1;
                continue;
            };
            if self.validate_chunk(index)? {
                if self
                    .receipt(index)?
                    .is_some_and(|current| current.sha256 == receipt.sha256)
                {
                    report.skipped += 1;
                } else {
                    report.rejected += 1;
                }
                continue;
            }
            let (path, mut output) = self.stage(index)?;
            let temporary = tempfile::TempPath::try_from_path(path.clone())?;
            check_path(&other.chunk_path(index))?;
            let mut input = File::open(other.chunk_path(index))?;
            let (length, actual) = copy_hash(&mut input, &mut output)?;
            output.sync_all()?;
            drop(output);
            if length != self.manifest.chunk_len(index)? || actual != receipt.sha256 {
                report.rejected += 1;
                continue;
            }
            self.commit(index, &path, &actual)?;
            // commit consumed the source path; dropping this guard is harmless.
            drop(temporary);
            report.imported += 1;
        }
        Ok(report)
    }
}

fn manifests_equal(left: &Manifest, right: &Manifest) -> Result<bool> {
    left.validate()?;
    right.validate()?;
    Ok(left.download_id == right.download_id
        && serde_json::to_value(left)? == serde_json::to_value(right)?)
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn hash_reader(reader: &mut impl Read) -> Result<(u64, String)> {
    copy_hash(reader, &mut io::sink())
}

pub(crate) fn copy_hash(reader: &mut impl Read, writer: &mut impl Write) -> Result<(u64, String)> {
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut digest = Sha256::new();
    let mut length = 0_u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        length = length
            .checked_add(count as u64)
            .ok_or_else(|| Error::Integrity("file length overflow".into()))?;
    }
    Ok((length, hex::encode(digest.finalize())))
}

pub(crate) fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
        || (!path.is_absolute()
            && path
                .components()
                .any(|component| matches!(component, Component::Prefix(_))))
    {
        return Err(unsafe_path(path));
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(path
        .components()
        .filter(|component| *component != Component::CurDir)
        .collect())
}

/// Refuse links and Windows junctions/reparse points in every existing path
/// component. The directory should be private to this user; locks coordinate
/// Cohatch processes, not a hostile process changing paths concurrently.
pub(crate) fn check_path(path: &Path) -> Result<()> {
    let path = absolute_path(path)?;
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component.as_os_str());
        // C: is not an independent filesystem object; wait for its root.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&prefix) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || is_reparse(&metadata) {
                    return Err(unsafe_path(&prefix));
                }
                if prefix != path && !metadata.is_dir() {
                    return Err(unsafe_path(&prefix));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

pub(crate) fn require_file(path: &Path) -> Result<()> {
    check_path(path)?;
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(unsafe_path(path));
    }
    Ok(())
}

fn unsafe_path(path: &Path) -> Error {
    Error::InvalidInput(format!(
        "unsafe path (links, traversal, or nonregular files are not allowed): {}",
        path.display()
    ))
}

fn remove_regular_if_present(path: &Path) -> Result<()> {
    check_path(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(unsafe_path(path));
            }
            fs::remove_file(path)?;
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    // Windows has no portable directory fsync. Data and receipt file handles
    // are flushed before the platform's no-clobber rename publication.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
thread_local! { static FAIL_RECEIPT_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Source;

    pub(crate) fn fixture(root: &Path, hash: Option<String>) -> Store {
        let manifest = Manifest::new(
            "https://example.invalid/file".into(),
            "payload.bin".into(),
            10,
            4,
            2,
            Source {
                etag: Some("\"stable\"".into()),
                last_modified: None,
                content_type: None,
                final_url: "https://example.invalid/file".into(),
                accept_ranges: true,
            },
            hash,
        )
        .unwrap();
        Store::create(root, &manifest).unwrap()
    }

    pub(crate) fn put(store: &Store, index: u64, data: &[u8]) {
        let _lock = store.lock().unwrap();
        let (path, mut file) = store.stage(index).unwrap();
        file.write_all(data).unwrap();
        drop(file);
        store
            .commit(index, &path, &hex::encode(Sha256::digest(data)))
            .unwrap();
    }

    #[test]
    fn receipts_required_corruption_missing_and_partial_repair() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        fs::write(store.chunk_path(0), b"abcd").unwrap();
        fs::write(store.temp_path(1), b"ef").unwrap();
        assert_eq!(store.scan().unwrap().missing, vec![0, 1, 2]);
        put(&store, 0, b"abcd");
        put(&store, 2, b"ij");
        assert_eq!(store.scan().unwrap().complete_bytes, 6);
        fs::write(store.chunk_path(0), b"abce").unwrap();
        assert_eq!(store.scan().unwrap().missing, vec![0, 1]);
        put(&store, 0, b"abcd");
        assert!(store.validate_chunk(0).unwrap());
    }

    #[test]
    fn exclusive_locks_and_required_mutation_guard() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        assert!(store.stage(0).is_err());
        let other = Store::open(&dir.path().join("manifest.json")).unwrap();
        let lock = store.lock().unwrap();
        assert!(matches!(store.lock(), Err(Error::Locked)));
        assert!(matches!(other.lock(), Err(Error::Locked)));
        drop(lock);
        assert!(other.lock().is_ok());
    }

    #[test]
    fn import_validates_duplicates_corruption_and_out_of_range() {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let source = fixture(source_dir.path(), None);
        let target = fixture(target_dir.path(), None);
        put(&source, 0, b"abcd");
        put(&source, 1, b"efgh");
        put(&source, 2, b"ij");
        put(&target, 0, b"abcd");
        fs::write(source.chunk_path(2), b"XX").unwrap();
        fs::write(source.chunk_path(99), b"bad").unwrap();
        let report = target.import(&source.root.join("chunks")).unwrap();
        assert_eq!(
            (report.imported, report.skipped, report.rejected),
            (1, 1, 2)
        );
        assert_eq!(target.scan().unwrap().missing, vec![2]);
        fs::write(source.chunk_path(0), b"XXXX").unwrap();
        let report = target.import(&source.root).unwrap();
        assert_eq!(
            (report.imported, report.skipped, report.rejected),
            (0, 1, 3)
        );
        assert_eq!(fs::read(target.chunk_path(0)).unwrap(), b"abcd");
    }

    #[test]
    fn manifest_mismatch_and_valid_but_conflicting_chunk_are_rejected() {
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let a = fixture(a_dir.path(), None);
        let b = fixture(b_dir.path(), None);
        put(&a, 0, b"abcd");
        put(&b, 0, b"XXXX");
        let report = a.import(&b.root).unwrap();
        assert_eq!(report.rejected, 1);
        assert_eq!(fs::read(a.chunk_path(0)).unwrap(), b"abcd");
        let mut mismatch = serde_json::to_value(&b.manifest).unwrap();
        mismatch["download_id"] = serde_json::Value::String("f".repeat(64));
        fs::write(
            b.root.join("manifest.json"),
            serde_json::to_vec(&mismatch).unwrap(),
        )
        .unwrap();
        assert!(a.import(&b.root).is_err());
    }

    #[test]
    fn receipt_write_failure_preserves_other_durable_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        put(&store, 0, b"abcd");
        let _lock = store.lock().unwrap();
        let (path, mut file) = store.stage(1).unwrap();
        file.write_all(b"efgh").unwrap();
        drop(file);
        FAIL_RECEIPT_WRITE.with(|fail| fail.set(true));
        assert!(
            store
                .commit(1, &path, &hex::encode(Sha256::digest(b"efgh")))
                .is_err()
        );
        assert!(store.validate_chunk(0).unwrap());
        assert!(!store.validate_chunk(1).unwrap());
        assert!(!store.chunk_path(1).exists());
        store
            .commit(1, &path, &hex::encode(Sha256::digest(b"efgh")))
            .unwrap();
        assert!(store.validate_chunk(1).unwrap());
    }

    #[test]
    fn rejects_traversal_reserved_directories_and_false_staged_digest() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        assert!(Store::open(&dir.path().join("../manifest.json")).is_err());
        let _lock = store.lock().unwrap();
        let (path, mut file) = store.stage(0).unwrap();
        file.write_all(b"abcd").unwrap();
        drop(file);
        assert!(store.commit(0, &path, &"0".repeat(64)).is_err());
        assert!(!store.validate_chunk(0).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_chunk_directory_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        fs::remove_dir(store.root.join("chunks")).unwrap();
        std::os::unix::fs::symlink(target.path(), store.root.join("chunks")).unwrap();
        let _lock = store.lock().unwrap();
        assert!(store.stage(0).is_err());
        assert!(store.scan().is_err());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_junction_chunk_directory_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        let chunks = store.root.join("chunks");
        fs::remove_dir(&chunks).unwrap();
        // Directory junctions do not require Windows Developer Mode or the
        // elevated privilege needed for creating symbolic links.
        let output = std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(dir.path().join("chunks"))
            .arg(target.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _lock = store.lock().unwrap();
        assert!(store.stage(0).is_err());
        assert!(store.scan().is_err());
        assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
        fs::remove_dir(chunks).unwrap();
    }
}
