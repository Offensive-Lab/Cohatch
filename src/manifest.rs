//! Versioned, bounded manifests and deterministic byte-range layout.
//!
//! The download ID detects accidental manifest changes. It is not a signature:
//! a trusted expected SHA-256 must come from an independent trusted source.

use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{Error, Result};

pub const FORMAT: &str = "cohatch";
pub const VERSION: u32 = 1;
pub const DEFAULT_CHUNK_SIZE: u64 = 64 * 1024 * 1024;
pub const MAX_CHUNK_SIZE: u64 = 1024 * 1024 * 1024;
pub const MAX_CHUNKS: u64 = 1_000_000;
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_FILENAME_BYTES: usize = 180;
const MAX_HEADER_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub final_url: String,
    pub accept_ranges: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub download_id: String,
    pub url: String,
    pub filename: String,
    pub file_size: u64,
    pub chunk_size: u64,
    pub chunk_count: u64,
    pub nodes: u32,
    pub source: Source,
    pub expected_sha256: Option<String>,
}

impl Manifest {
    /// Construct a manifest from a successful probe. Node numbers are one-based.
    /// Remote filenames are sanitized; loaded manifests must already be safe.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: String,
        filename: String,
        file_size: u64,
        chunk_size: u64,
        nodes: u32,
        source: Source,
        expected_sha256: Option<String>,
    ) -> Result<Self> {
        let chunk_count = count_chunks(file_size, chunk_size)?;
        let mut manifest = Self {
            format: FORMAT.to_owned(),
            version: VERSION,
            download_id: String::new(),
            url,
            filename: sanitize_filename(&filename),
            file_size,
            chunk_size,
            chunk_count,
            nodes,
            source,
            expected_sha256: expected_sha256.map(|hash| hash.to_ascii_lowercase()),
        };
        manifest.validate_fields()?;
        manifest.download_id = manifest.calculate_id()?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_fields()?;
        if !is_sha256(&self.download_id) || self.download_id != self.calculate_id()? {
            return invalid(
                "download_id does not match manifest identity/layout; recreate the manifest instead of editing it",
            );
        }
        Ok(())
    }

    fn validate_fields(&self) -> Result<()> {
        if self.format != FORMAT || self.version != VERSION {
            return invalid("unsupported format or version (expected cohatch version 1)");
        }
        validate_url(&self.url)?;
        validate_url(&self.source.final_url)?;
        if self.filename != sanitize_filename(&self.filename) {
            return invalid("filename is unsafe or reserved; use a single safe filename");
        }
        let expected_count = count_chunks(self.file_size, self.chunk_size)?;
        if self.chunk_count != expected_count {
            return invalid("chunk_count does not match file_size and chunk_size");
        }
        if !(1..=2).contains(&self.nodes) {
            return invalid("v0.1 supports one or two nodes");
        }
        if !self.source.accept_ranges {
            return invalid("source has not demonstrated byte-range support");
        }
        for value in [
            &self.source.etag,
            &self.source.last_modified,
            &self.source.content_type,
        ]
        .into_iter()
        .flatten()
        {
            if value.is_empty()
                || value.len() > MAX_HEADER_BYTES
                || value.chars().any(char::is_control)
            {
                return invalid("source header is empty, too long, or contains control characters");
            }
        }
        if let Some(etag) = &self.source.etag {
            let opaque = etag.strip_prefix("W/").unwrap_or(etag);
            if opaque.len() < 2
                || !opaque.starts_with('"')
                || !opaque.ends_with('"')
                || opaque[1..opaque.len() - 1]
                    .bytes()
                    .any(|byte| byte < 0x21 || byte == b'"' || byte == 0x7f)
            {
                return invalid("ETag is not a quoted HTTP entity tag");
            }
        }
        if let Some(modified) = &self.source.last_modified
            && httpdate::parse_http_date(modified).is_err()
        {
            return invalid("Last-Modified is not a valid HTTP date");
        }
        if let Some(expected) = &self.expected_sha256
            && !is_sha256(expected)
        {
            return invalid("expected_sha256 must be 64 lowercase hexadecimal characters");
        }
        // Strong ETag or Last-Modified permits conditional range requests.
        // A trusted final hash is the only safe fallback when validators are absent.
        let strong_etag = self
            .source
            .etag
            .as_ref()
            .is_some_and(|etag| !etag.starts_with("W/"));
        if !strong_etag && self.source.last_modified.is_none() && self.expected_sha256.is_none() {
            return invalid(
                "source requires a strong ETag, Last-Modified, or trusted expected SHA-256",
            );
        }
        Ok(())
    }

    /// Bound reads before deserializing so a malicious manifest cannot allocate
    /// arbitrary memory. Unknown fields and duplicate fields are rejected by serde.
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return invalid("manifest must be a regular file, not a symbolic link");
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            return invalid("manifest exceeds the 64 KiB size limit");
        }
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return invalid("manifest exceeds the 64 KiB size limit");
        }
        let manifest: Self = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Publish a complete, flushed manifest without replacing any existing file.
    /// The containing download directory must already exist.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if path.file_name().is_none() {
            return invalid("manifest destination needs a filename");
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return invalid("serialized manifest exceeds the 64 KiB size limit");
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".cohatch-manifest-")
            .tempfile_in(parent)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Inclusive HTTP byte range. Every arithmetic operation is checked even
    /// when the caller constructed this public struct without `new`/`load`.
    pub fn range(&self, index: u64) -> Result<(u64, u64)> {
        let count = count_chunks(self.file_size, self.chunk_size)?;
        if self.chunk_count != count || index >= count {
            return invalid("chunk index is out of range or chunk_count is inconsistent");
        }
        let start = index
            .checked_mul(self.chunk_size)
            .ok_or_else(|| Error::Manifest("chunk start overflows u64".to_owned()))?;
        let len = (self.file_size - start).min(self.chunk_size);
        let end = start
            .checked_add(len - 1)
            .ok_or_else(|| Error::Manifest("chunk end overflows u64".to_owned()))?;
        Ok((start, end))
    }

    pub fn chunk_len(&self, index: u64) -> Result<u64> {
        let (start, end) = self.range(index)?;
        Ok(end - start + 1)
    }

    /// Node 1 gets even indexes and node 2 gets odd indexes with two nodes.
    pub fn assigned(&self, index: u64, node: u32) -> bool {
        (1..=2).contains(&self.nodes)
            && (1..=self.nodes).contains(&node)
            && index < self.chunk_count
            && index % u64::from(self.nodes) == u64::from(node - 1)
    }

    pub fn assigned_indices(&self, node: u32) -> Result<Vec<u64>> {
        self.validate()?;
        if !(1..=self.nodes).contains(&node) {
            return invalid("node number must be between 1 and the manifest node count");
        }
        Ok((u64::from(node - 1)..self.chunk_count)
            .step_by(self.nodes as usize)
            .collect())
    }

    fn calculate_id(&self) -> Result<String> {
        // A fixed-order JSON array avoids map-order/canonicalization ambiguity.
        // Including every field other than the ID makes edits fail validation.
        let identity = (
            &self.format,
            self.version,
            &self.url,
            &self.filename,
            self.file_size,
            self.chunk_size,
            self.chunk_count,
            self.nodes,
            &self.source,
            &self.expected_sha256,
        );
        let mut digest = Sha256::new();
        digest.update(b"cohatch-manifest-v1\0");
        digest.update(serde_json::to_vec(&identity)?);
        Ok(hex::encode(digest.finalize()))
    }
}

fn invalid<T>(message: &str) -> Result<T> {
    Err(Error::Manifest(message.to_owned()))
}

fn count_chunks(file_size: u64, chunk_size: u64) -> Result<u64> {
    if file_size == 0 {
        return invalid("empty remote objects are not supported");
    }
    if chunk_size == 0 || chunk_size > MAX_CHUNK_SIZE {
        return invalid("chunk_size must be between 1 byte and 1 GiB");
    }
    // Division avoids overflow from the common (size + chunk - 1) formula.
    let count = (file_size - 1) / chunk_size + 1;
    if count > MAX_CHUNKS {
        return invalid("chunk_count exceeds 1,000,000; choose a larger chunk size");
    }
    Ok(count)
}

pub(crate) fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Only public-style HTTP(S) URLs are supported. Userinfo is deliberately
/// rejected so credentials cannot leak into manifests or logs.
pub(crate) fn validate_url(value: &str) -> Result<()> {
    if value.len() > MAX_URL_BYTES || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return invalid("URL is too long or contains whitespace/control characters");
    }
    if !value.split_once("://").is_some_and(|(scheme, _)| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    }) {
        return invalid("URL must be an absolute HTTP(S) URL with ://");
    }
    let url = Url::parse(value).map_err(|_| Error::Manifest("malformed HTTP(S) URL".to_owned()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || value.contains('\\')
    {
        return invalid(
            "URL must use HTTP(S), have a host, and contain no credentials, fragment, or backslash",
        );
    }
    Ok(())
}

/// Create a portable single-component filename, including Windows restrictions
/// when running on Unix. Sanitizing never interprets a remote path as a local one.
pub fn sanitize_filename(value: &str) -> String {
    let mut filename = String::with_capacity(value.len().min(MAX_FILENAME_BYTES));
    for character in value.chars() {
        let safe = if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
            || matches!(character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            '_'
        } else {
            character
        };
        if filename.len() + safe.len_utf8() > MAX_FILENAME_BYTES {
            break;
        }
        filename.push(safe);
    }
    filename = filename.trim_matches([' ', '.']).to_owned();
    if filename.is_empty() {
        return "download.bin".to_owned();
    }
    let stem = filename
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let device = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    let lower = filename.to_ascii_lowercase();
    let reserved = matches!(
        lower.as_str(),
        "manifest.json" | "chunks" | "cohatch.lock" | ".cohatch.lock"
    ) || lower.starts_with(".cohatch-");
    if device || reserved {
        filename.insert(0, '_');
        // Keep the same length invariant after adding a reserved-name prefix.
        while filename.len() > MAX_FILENAME_BYTES {
            filename.pop();
        }
        filename = filename.trim_end_matches([' ', '.']).to_owned();
    }
    // Prefixing cannot remove a reserved suffix. Replace its final dot instead,
    // preserving the byte bound and making repeated sanitization idempotent.
    // Do this after truncation, which can itself expose the reserved suffix.
    if filename.to_ascii_lowercase().ends_with(".cohatch.tmp") {
        let final_dot = filename.len() - 4;
        filename.replace_range(final_dot..final_dot + 1, "_");
    }
    filename
}

/// Parse bytes or an integer size. K/M/G/T and KiB/MiB/GiB/TiB are binary;
/// KB/MB/GB/TB are decimal. For example, `64M` is exactly 64 MiB.
pub fn parse_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split]
        .parse()
        .map_err(|_| Error::InvalidInput("size must begin with a positive integer".to_owned()))?;
    let suffix = value[split..].trim().to_ascii_uppercase();
    let multiplier: u64 = match suffix.as_str() {
        "" | "B" => 1,
        "K" | "KI" | "KIB" => 1024,
        "M" | "MI" | "MIB" => 1024 * 1024,
        "G" | "GI" | "GIB" => 1024 * 1024 * 1024,
        "T" | "TI" | "TIB" => 1024 * 1024 * 1024 * 1024,
        "KB" => 1000,
        "MB" => 1000 * 1000,
        "GB" => 1000 * 1000 * 1000,
        "TB" => 1000 * 1000 * 1000 * 1000,
        _ => {
            return Err(Error::InvalidInput(
                "unknown size suffix; use B, K/M/G/T, KiB/MiB/GiB/TiB, or KB/MB/GB/TB".to_owned(),
            ));
        }
    };
    number
        .checked_mul(multiplier)
        .filter(|size| *size != 0)
        .ok_or_else(|| Error::InvalidInput("size is zero or exceeds u64".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(size: u64, chunk: u64, nodes: u32) -> Manifest {
        Manifest::new(
            "https://example.com/data.bin".to_owned(),
            "data.bin".to_owned(),
            size,
            chunk,
            nodes,
            Source {
                etag: Some("\"revision-1\"".to_owned()),
                last_modified: None,
                content_type: Some("application/octet-stream".to_owned()),
                final_url: "https://example.com/data.bin".to_owned(),
                accept_ranges: true,
            },
            None,
        )
        .unwrap()
    }

    #[test]
    fn exact_and_partial_ranges() {
        let m = manifest(100, 32, 2);
        assert_eq!(m.chunk_count, 4);
        assert_eq!(m.range(0).unwrap(), (0, 31));
        assert_eq!(m.range(2).unwrap(), (64, 95));
        assert_eq!(m.range(3).unwrap(), (96, 99));
        assert_eq!(m.chunk_len(3).unwrap(), 4);
        assert!(m.range(4).is_err());
        assert!(m.range(u64::MAX).is_err());
        let exact = manifest(96, 32, 1);
        assert_eq!(exact.chunk_count, 3);
        assert_eq!(exact.chunk_len(2).unwrap(), 32);
        assert_eq!(manifest(1, 64, 1).range(0).unwrap(), (0, 0));
    }

    #[test]
    fn assigned_one_based_nodes_partition_all_chunks() {
        let m = manifest(9, 1, 2);
        assert_eq!(m.assigned_indices(1).unwrap(), vec![0, 2, 4, 6, 8]);
        assert_eq!(m.assigned_indices(2).unwrap(), vec![1, 3, 5, 7]);
        assert!(!m.assigned(0, 0));
        assert!(!m.assigned(0, 3));
        assert!(!m.assigned(9, 2));
        assert!(m.assigned_indices(0).is_err());
        assert_eq!(
            manifest(3, 1, 1).assigned_indices(1).unwrap(),
            vec![0, 1, 2]
        );
        assert_eq!(
            manifest(1, 1, 2).assigned_indices(2).unwrap(),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn deterministic_identity_and_serde_roundtrip() {
        let m = manifest(100, 32, 2);
        assert_eq!(m.download_id, manifest(100, 32, 2).download_id);
        let serialized = serde_json::to_string_pretty(&m).unwrap();
        let restored: Manifest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(m, restored);
        restored.validate().unwrap();
        let mut changed = restored.clone();
        changed.source.etag = Some("\"revision-2\"".to_owned());
        assert!(changed.validate().is_err());
        changed = restored.clone();
        changed.url.push_str("?other=source");
        assert!(changed.validate().is_err());
        changed = restored;
        changed.nodes = 1;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn documented_example_has_a_valid_deterministic_id() {
        let m: Manifest = serde_json::from_str(include_str!("../examples/manifest.json")).unwrap();
        m.validate().unwrap();
        assert_eq!(m.file_size, 100 * 1024 * 1024 * 1024);
        assert_eq!(m.chunk_count, 1600);
    }

    #[test]
    fn refuses_invalid_manifest_fields_and_arithmetic() {
        let m = manifest(100, 32, 2);
        for mutate in [
            |m: &mut Manifest| m.format = "other".to_owned(),
            |m: &mut Manifest| m.version = 2,
            |m: &mut Manifest| m.file_size = 0,
            |m: &mut Manifest| m.chunk_size = 0,
            |m: &mut Manifest| m.chunk_size = MAX_CHUNK_SIZE + 1,
            |m: &mut Manifest| m.chunk_count = 5,
            |m: &mut Manifest| m.nodes = 0,
            |m: &mut Manifest| m.nodes = 3,
            |m: &mut Manifest| m.filename = "../outside.bin".to_owned(),
            |m: &mut Manifest| m.expected_sha256 = Some("not-a-hash".to_owned()),
            |m: &mut Manifest| m.source.accept_ranges = false,
            |m: &mut Manifest| m.source.etag = Some("\"bad\r\nheader\"".to_owned()),
        ] {
            let mut altered = m.clone();
            mutate(&mut altered);
            assert!(altered.validate().is_err());
        }
        assert!(count_chunks(u64::MAX, 1).is_err());
        assert!(count_chunks(u64::MAX, u64::MAX).is_err());
        assert!(count_chunks(MAX_CHUNKS + 1, 1).is_err());
        let largest = manifest(MAX_CHUNKS * MAX_CHUNK_SIZE, MAX_CHUNK_SIZE, 2);
        assert_eq!(
            largest.range(MAX_CHUNKS - 1).unwrap().1,
            largest.file_size - 1
        );
        let mut corrupted = m;
        corrupted.chunk_size = 0;
        assert!(corrupted.range(0).is_err());
        corrupted.nodes = 0;
        assert!(!corrupted.assigned(0, 1));
    }

    #[test]
    fn no_validator_requires_trusted_hash() {
        let m = manifest(100, 32, 1);
        let mut source = m.source.clone();
        source.etag = None;
        let make = |source: Source, hash| {
            Manifest::new(m.url.clone(), m.filename.clone(), 100, 32, 1, source, hash)
        };
        assert!(make(source.clone(), None).is_err());
        source.etag = Some("W/\"weak\"".to_owned());
        assert!(make(source.clone(), None).is_err());
        let hash = "A".repeat(64);
        assert_eq!(
            make(source.clone(), Some(hash)).unwrap().expected_sha256,
            Some("a".repeat(64))
        );
        source.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".to_owned());
        assert!(make(source.clone(), None).is_ok());
        source.last_modified = Some("yesterday".to_owned());
        assert!(make(source, None).is_err());
    }

    #[test]
    fn filename_safety_is_portable_and_idempotent() {
        for unsafe_name in [
            "../evil.exe",
            "..\\evil.exe",
            "C:\\Windows\\evil.exe",
            "CON",
            "con.txt",
            "NUL.foo",
            "LPT1",
            "COM¹.txt",
            "CON .txt",
            "bad:stream",
            "trailing. ",
            "manifest.json",
            "CHUNKS",
            ".cohatch.lock",
            ".",
            "..",
            "",
            "has\0nul",
            "file\u{202e}txt.exe",
            "output.cohatch.tmp",
        ] {
            let safe = sanitize_filename(unsafe_name);
            assert_ne!(safe, unsafe_name, "{unsafe_name:?}");
            assert_eq!(sanitize_filename(&safe), safe, "{unsafe_name:?}");
            assert!(!safe.contains(['/', '\\', ':', '\0']));
        }
        assert_eq!(sanitize_filename("model.safetensors"), "model.safetensors");
        let long = sanitize_filename(&"界".repeat(200));
        assert!(long.len() <= MAX_FILENAME_BYTES);
        assert_eq!(sanitize_filename(&long), long);
    }

    #[test]
    fn reserved_suffixes_are_changed_once_and_loaded_names_remain_rejected() {
        for original in [
            "output.cohatch.tmp",
            "OUTPUT.COHATCH.TMP",
            "NUL.cohatch.tmp",
            ".cohatch.tmp",
        ] {
            let safe = sanitize_filename(original);
            assert_ne!(safe, original);
            assert!(!safe.to_ascii_lowercase().ends_with(".cohatch.tmp"));
            assert_eq!(sanitize_filename(&safe), safe);
            let mut m = manifest(100, 32, 1);
            m.filename = original.to_owned();
            m.download_id = m.calculate_id().unwrap();
            assert!(m.validate().is_err());
        }
        // Inserting the device-name prefix truncates the final X and exposes
        // the reserved suffix; that suffix still needs to be sanitized.
        let long_device = format!("NUL.{}.cohatch.tmpX", "a".repeat(163));
        assert_eq!(long_device.len(), MAX_FILENAME_BYTES);
        let safe = sanitize_filename(&long_device);
        assert!(safe.len() <= MAX_FILENAME_BYTES);
        assert_eq!(sanitize_filename(&safe), safe);

        // Chunk state lives under chunks/, so these root output basenames do
        // not collide with chunk data or receipts and need no suffix rewrite.
        for name in [
            "00000000.chunk",
            "00000000.chunk.json",
            "chunk",
            "chunk.json",
        ] {
            assert_eq!(sanitize_filename(name), name);
        }
        assert_eq!(sanitize_filename(".chunk"), "chunk");
        assert_eq!(sanitize_filename(".chunk.json"), "chunk.json");
    }

    #[test]
    fn strict_public_url_fields() {
        for bad in [
            "file:///C:/data.bin",
            "https://user:password@example.com/x",
            "http://",
            "not a url",
            "https://example.com/x#fragment",
            "https://example.com/\nsecret",
            "https:\\example.com/x",
            "https://example.com/file name",
            "http:example.com/file",
        ] {
            assert!(validate_url(bad).is_err(), "{bad:?}");
        }
        for good in [
            "https://example.com/x",
            "http://127.0.0.1:1234/a?token=value",
            "http://[::1]/x",
        ] {
            validate_url(good).unwrap();
        }
    }

    #[test]
    fn size_units_are_explicit_and_checked() {
        assert_eq!(parse_size("64M").unwrap(), DEFAULT_CHUNK_SIZE);
        assert_eq!(parse_size("64 MiB").unwrap(), DEFAULT_CHUNK_SIZE);
        assert_eq!(parse_size("64MB").unwrap(), 64_000_000);
        assert_eq!(parse_size(" 1g ").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        for bad in [
            "",
            "0",
            "-1",
            "1.5M",
            "M",
            "1Z",
            "18446744073709551615T",
            "18446744073709551616",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn disk_roundtrip_never_overwrites_and_bounds_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.json");
        let m = manifest(100, 32, 1);
        m.save(&path).unwrap();
        assert_eq!(Manifest::load(&path).unwrap(), m);
        assert!(m.save(&path).is_err());
        assert_eq!(Manifest::load(&path).unwrap(), m);
        let huge = directory.path().join("huge.json");
        File::create(&huge)
            .unwrap()
            .set_len(MAX_MANIFEST_BYTES + 1)
            .unwrap();
        assert!(Manifest::load(&huge).is_err());
        assert!(Manifest::load(directory.path()).is_err());
    }

    #[test]
    fn serde_rejects_unknown_and_duplicate_fields() {
        let mut value = serde_json::to_value(manifest(100, 32, 1)).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("surprise".to_owned(), true.into());
        assert!(serde_json::from_value::<Manifest>(value).is_err());
        let json = serde_json::to_string(&manifest(100, 32, 1)).unwrap();
        let duplicated = json.replacen('{', "{\"version\":1,", 1);
        assert!(serde_json::from_str::<Manifest>(&duplicated).is_err());
    }
}
