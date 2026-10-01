# Manifest format v1

`examples/manifest.json` is a valid illustrative manifest for a hypothetical
100 GiB file split into 1,600 chunks of 64 MiB. Its example.com URL is a placeholder,
not a downloadable test fixture. Use `cohatch create` to probe a real URL and make
its manifest. Copy the resulting manifest unchanged between the two computers.

All byte sizes and indexes are unsigned integers. Chunk indexes start at zero;
node numbers start at one. With two nodes, node 1 owns even indexes and node 2
owns odd indexes. With one node, node 1 owns every index. The final chunk can be
shorter. Inclusive HTTP ranges are derived from `file_size`, `chunk_size`, and
the index; a manifest does not list each chunk separately.

The format marker is `cohatch`, and `version` is `1`. `url` is the original
HTTP(S) URL; `source.final_url` records the URL after redirects. `source.etag`,
`source.last_modified`, and `source.content_type` retain available probe metadata.
`source.accept_ranges` means that a byte-range probe actually succeeded, rather
than merely trusting an `Accept-Ranges` advertisement.

`expected_sha256` is optional and, when supplied, must be a trusted SHA-256 from
an independent source. It is stored as 64 lowercase hexadecimal characters.
At least one strong ETag, Last-Modified date, or trusted expected SHA-256 is
required. Weak ETags can be retained as metadata but cannot independently make
conditional byte-range requests safe.

The download ID is SHA-256 over the UTF-8 prefix `cohatch-manifest-v1` followed
by a zero byte and the compact JSON serialization of this fixed-order array:

```text
[
  format, version, url, filename, file_size, chunk_size, chunk_count,
  nodes, source, expected_sha256
]
```

Within `source`, the canonical field order is `etag`, `last_modified`,
`content_type`, `final_url`, `accept_ranges`; absent optional values serialize
as JSON `null`. Cohatch uses serde_json compact serialization. Formatting and
object key order in the saved manifest do not affect the ID because Cohatch
deserializes and serializes the typed identity before hashing. Changes to its
content, including the original URL, output name, assignment, or identity
metadata, invalidate the ID. Recreate a manifest rather than editing it.

This digest prevents accidental mismatch and groups compatible offline chunks.
It is not a signature: someone who changes the manifest can also recalculate
the ID. Similarly, local chunk receipt hashes detect subsequent corruption;
they do not establish the original content's authenticity. Preserve a trusted
manifest and compare the reconstructed file against a trusted expected SHA-256
when authenticity matters.

Safety limits are a 64 KiB manifest, 16 KiB per URL, 4 KiB per source header,
1,000,000 chunks, 1 GiB per chunk, and a 180-byte portable output filename.
Empty objects are unsupported. v0.1 accepts one or two nodes. URLs must use
HTTP(S) with a host and must not contain credentials, fragments, literal
whitespace, control characters, or backslashes. Unknown and duplicate JSON
fields are rejected.

New filenames are sanitized for Windows and Unix. The manifest loader rejects
unsafe filenames, including path separators, Windows device names, alternate
data streams, and names reserved for Cohatch state. Loading is bounded before
JSON parsing; saving uses a flushed temporary file and refuses to replace an
existing destination. The download directory is expected to be owned by the
user; this is not a security boundary against a hostile local process changing
paths concurrently.

CLI size suffixes distinguish binary and decimal units: `64M` and `64MiB` mean
67,108,864 bytes; `64MB` means 64,000,000 bytes. Bare integers mean bytes. Size
parsing and byte-range arithmetic detect overflow.
