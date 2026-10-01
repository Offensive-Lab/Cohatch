//! Offline reconstruction and trusted final-digest verification.
use crate::{
    Error, Result,
    manifest::sanitize_filename,
    storage::{
        BUFFER_SIZE, Store, absolute_path, check_path, hash_reader, require_file, sync_directory,
    },
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct CombineReport {
    pub path: PathBuf,
    pub sha256: String,
    /// True only when matched against the expected hash in the manifest.
    pub verified: bool,
    pub bytes: u64,
}

/// Reconstruct into a unique temporary file, then publish without replacing
/// any existing output. Missing/corrupt chunk indexes are reported together.
pub fn combine(store: &Store, output: Option<&Path>) -> Result<CombineReport> {
    let _lock = store.lock()?;
    let path = output_path(store, output)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("output already exists: {}", path.display()),
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let scan = store.scan()?;
    if !scan.missing.is_empty() {
        return Err(Error::MissingChunks(scan.missing));
    }
    #[cfg(test)]
    if let Some(index) = AFTER_SCAN_MUTATE_CHUNK.with(|slot| slot.take()) {
        fs::OpenOptions::new()
            .write(true)
            .open(store.chunk_path(index))?
            .write_all(&[0xff])?;
    }

    let mut temporary = tempfile::Builder::new()
        .prefix(".cohatch-combine-")
        .suffix(".tmp")
        .tempfile_in(&store.root)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    {
        let mut writer = BufWriter::with_capacity(BUFFER_SIZE, &mut temporary);
        for index in 0..store.manifest.chunk_count {
            // Recheck during the actual copy: a scan followed by unchecked
            // copying could publish corruption introduced between the steps.
            let receipt = store
                .receipt(index)?
                .ok_or_else(|| Error::MissingChunks(vec![index]))?;
            let chunk_path = store.chunk_path(index);
            require_file(&chunk_path)?;
            let expected = store.manifest.chunk_len(index)?;
            let mut reader = File::open(&chunk_path)?;
            let mut chunk_digest = Sha256::new();
            let mut chunk_bytes = 0_u64;
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                chunk_bytes = chunk_bytes
                    .checked_add(count as u64)
                    .ok_or_else(|| Error::Integrity(format!("chunk {index} length overflow")))?;
                if chunk_bytes > expected {
                    return Err(Error::Integrity(format!(
                        "chunk {index} grew during reconstruction"
                    )));
                }
                writer.write_all(&buffer[..count])?;
                chunk_digest.update(&buffer[..count]);
                digest.update(&buffer[..count]);
            }
            if chunk_bytes != expected || hex::encode(chunk_digest.finalize()) != receipt.sha256 {
                return Err(Error::Integrity(format!(
                    "chunk {index} changed during reconstruction"
                )));
            }
            total += chunk_bytes;
        }
        writer.flush()?;
    }
    if total != store.manifest.file_size {
        return Err(Error::Integrity(
            "reconstructed length does not match manifest".into(),
        ));
    }
    let sha256 = hex::encode(digest.finalize());
    let verified = check_expected(store, &sha256)?;
    temporary.as_file().sync_all()?;
    check_path(&path)?;
    temporary
        .persist_noclobber(&path)
        .map_err(|error| Error::Io(error.error))?;
    sync_directory(&store.root)?;
    Ok(CombineReport {
        path,
        sha256,
        verified,
        bytes: total,
    })
}

/// Compute the whole-file digest offline. Without a trusted expected digest,
/// the result is deliberately reported as computed, never verified.
pub fn verify(store: &Store, file: Option<&Path>) -> Result<CombineReport> {
    let path = output_path(store, file)?;
    require_file(&path)?;
    let mut file = File::open(&path)?;
    if file.metadata()?.len() != store.manifest.file_size {
        return Err(Error::Integrity(format!(
            "file length differs from expected {} bytes",
            store.manifest.file_size
        )));
    }
    let (bytes, sha256) = hash_reader(&mut file)?;
    if bytes != store.manifest.file_size {
        return Err(Error::Integrity(
            "file length changed during verification".into(),
        ));
    }
    let verified = check_expected(store, &sha256)?;
    Ok(CombineReport {
        path,
        sha256,
        verified,
        bytes,
    })
}

fn check_expected(store: &Store, actual: &str) -> Result<bool> {
    if let Some(expected) = &store.manifest.expected_sha256 {
        if actual != expected {
            return Err(Error::Integrity(format!(
                "final SHA-256 mismatch; expected {expected}, actual {actual}"
            )));
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

fn output_path(store: &Store, output: Option<&Path>) -> Result<PathBuf> {
    let input = output.unwrap_or_else(|| Path::new(&store.manifest.filename));
    let path = if input.is_absolute() {
        absolute_path(input)?
    } else if input.components().count() == 1 {
        store.root.join(input)
    } else {
        return Err(Error::InvalidInput(
            "output must be a filename inside the download directory".into(),
        ));
    };
    check_path(&path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::InvalidInput("invalid output filename".into()))?;
    if sanitize_filename(name) != name
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "manifest.json" | "chunks" | ".cohatch.lock"
        )
        || fs::canonicalize(
            path.parent()
                .ok_or_else(|| Error::InvalidInput("missing output parent".into()))?,
        )? != fs::canonicalize(&store.root)?
    {
        return Err(Error::InvalidInput(
            "output must be a safe filename inside the download directory".into(),
        ));
    }
    Ok(path)
}

#[cfg(test)]
thread_local! {
    static AFTER_SCAN_MUTATE_CHUNK: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        manifest::{Manifest, Source},
        storage::copy_hash,
    };

    fn fixture(root: &Path, expected: Option<String>) -> Store {
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
            expected,
        )
        .unwrap();
        Store::create(root, &manifest).unwrap()
    }

    fn put(store: &Store, index: u64, data: &[u8]) {
        let _lock = store.lock().unwrap();
        let (path, mut file) = store.stage(index).unwrap();
        file.write_all(data).unwrap();
        drop(file);
        store
            .commit(index, &path, &hex::encode(Sha256::digest(data)))
            .unwrap();
    }

    fn complete(store: &Store) {
        put(store, 0, b"abcd");
        put(store, 1, b"efgh");
        put(store, 2, b"ij");
    }

    #[test]
    fn reconstructs_partial_final_chunk_and_verifies_trusted_digest() {
        let dir = tempfile::tempdir().unwrap();
        let expected = hex::encode(Sha256::digest(b"abcdefghij"));
        let store = fixture(dir.path(), Some(expected.clone()));
        complete(&store);
        let report = combine(&store, None).unwrap();
        assert!(report.verified);
        assert_eq!(report.bytes, 10);
        assert_eq!(report.sha256, expected);
        assert_eq!(fs::read(&report.path).unwrap(), b"abcdefghij");
        assert!(verify(&store, None).unwrap().verified);
        fs::write(&report.path, b"abcdefghik").unwrap();
        assert!(verify(&store, None).is_err());
    }

    #[test]
    fn computed_digest_is_not_reported_as_trusted_verification() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        complete(&store);
        assert!(!combine(&store, None).unwrap().verified);
        assert!(!verify(&store, None).unwrap().verified);
    }

    #[test]
    fn refuses_missing_corrupt_output_overwrite_and_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        put(&store, 1, b"efgh");
        assert!(
            matches!(combine(&store, None), Err(Error::MissingChunks(indexes)) if indexes == vec![0, 2])
        );
        assert!(!store.root.join("payload.bin").exists());
        put(&store, 0, b"abcd");
        put(&store, 2, b"ij");
        fs::write(store.chunk_path(2), b"XX").unwrap();
        assert!(
            matches!(combine(&store, None), Err(Error::MissingChunks(indexes)) if indexes == vec![2])
        );
        put(&store, 2, b"ij");
        fs::write(store.root.join("payload.bin"), b"unrelated").unwrap();
        assert!(combine(&store, None).is_err());
        assert_eq!(
            fs::read(store.root.join("payload.bin")).unwrap(),
            b"unrelated"
        );
        assert!(combine(&store, Some(Path::new("../escape.bin"))).is_err());
        assert!(combine(&store, Some(Path::new("manifest.json"))).is_err());
    }

    #[test]
    fn expected_hash_failure_never_publishes_output() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), Some("0".repeat(64)));
        complete(&store);
        assert!(combine(&store, None).is_err());
        assert!(!store.root.join("payload.bin").exists());
        assert!(!fs::read_dir(&store.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cohatch-combine-")
        }));
        assert_eq!(store.scan().unwrap().complete, vec![0, 1, 2]);
    }

    #[test]
    fn detects_chunk_changed_after_scan_even_without_trusted_final_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = fixture(dir.path(), None);
        complete(&store);
        AFTER_SCAN_MUTATE_CHUNK.with(|slot| slot.set(Some(1)));
        assert!(matches!(combine(&store, None), Err(Error::Integrity(_))));
        assert!(!store.root.join("payload.bin").exists());
        assert!(store.validate_chunk(0).unwrap());
        assert!(store.validate_chunk(2).unwrap());
    }

    #[test]
    fn streaming_copy_propagates_disk_write_failure() {
        struct FullDisk {
            remaining: usize,
        }
        impl Write for FullDisk {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    return Err(io::Error::other("simulated disk full"));
                }
                let count = bytes.len().min(self.remaining);
                self.remaining -= count;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut input = &b"abcdefghij"[..];
        let mut output = FullDisk { remaining: 4 };
        assert!(matches!(
            copy_hash(&mut input, &mut output),
            Err(Error::Io(_))
        ));
        assert_eq!(output.remaining, 0);
    }
}
