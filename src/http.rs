//! Strict range HTTP transport. No transparent decompression or URL-token logging.
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode, header};
use url::Url;

use crate::{
    Error, Result,
    manifest::{Manifest, Source, sanitize_filename, validate_url},
};

#[derive(Clone, Debug)]
pub struct HttpOptions {
    /// Total request deadline, including the streaming body.
    pub timeout: Duration,
    pub user_agent: String,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(1800),
            user_agent: "Cohatch/0.1.0".into(),
        }
    }
}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
}

#[derive(Debug)]
pub struct Probe {
    pub file_size: u64,
    pub source: Source,
    pub filename: String,
}

impl HttpClient {
    pub fn new(options: HttpOptions) -> Result<Self> {
        if options.timeout.is_zero() || options.timeout > Duration::from_secs(7 * 86400) {
            return Err(Error::InvalidInput(
                "request timeout must be positive and at most 7 days".into(),
            ));
        }
        let client = Client::builder()
            .user_agent(options.user_agent)
            .timeout(options.timeout)
            .connect_timeout(options.timeout.min(Duration::from_secs(15)))
            .read_timeout(options.timeout.min(Duration::from_secs(60)))
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let next = attempt.url();
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                if validate_url(next.as_str()).is_err()
                    || (next.scheme() == "http"
                        && attempt.previous().iter().any(|u| u.scheme() == "https"))
                {
                    return attempt
                        .error("unsafe redirect (credentials, scheme, or HTTPS downgrade)");
                }
                attempt.follow()
            }))
            .build()
            .map_err(network)?;
        Ok(Self { client })
    }

    /// A real one-byte GET is stronger evidence of range support than HEAD or Accept-Ranges.
    pub async fn probe(&self, url: &str) -> Result<Probe> {
        let parsed = checked_url(url)?;
        let response = self
            .client
            .get(parsed)
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, "bytes=0-0")
            .send()
            .await
            .map_err(network)?;
        validate_status(&response, false)?;
        let (start, end, total) = content_range(&response)?;
        if start != 0 || end != 0 {
            return Err(Error::Protocol(
                "probe did not return exactly bytes 0-0".into(),
            ));
        }
        validate_payload_headers(&response, 1)?;
        let source = Source {
            etag: optional_header(&response, header::ETAG)?,
            last_modified: optional_header(&response, header::LAST_MODIFIED)?,
            content_type: optional_header(&response, header::CONTENT_TYPE)?,
            final_url: response.url().to_string(),
            accept_ranges: true,
        };
        // Ignore Content-Disposition entirely: it is not a trustworthy filesystem path.
        let filename = sanitize_filename(
            response
                .url()
                .path_segments()
                .and_then(|mut parts| parts.next_back())
                .filter(|s| !s.is_empty())
                .unwrap_or("download.bin"),
        );
        let mut stream = response.bytes_stream();
        let mut length = 0usize;
        while let Some(data) = stream.next().await {
            length = length.saturating_add(data.map_err(network)?.len());
            if length > 1 {
                return Err(Error::Protocol(
                    "probe body exceeded the requested byte".into(),
                ));
            }
        }
        if length != 1 {
            return Err(Error::Protocol("probe body was incomplete".into()));
        }
        Ok(Probe {
            file_size: total,
            source,
            filename,
        })
    }

    pub async fn range(&self, manifest: &Manifest, index: u64) -> Result<Response> {
        let (start, end) = manifest.range(index)?;
        let response = self.range_bytes(manifest, start, end).await?;
        tracing::debug!(
            chunk = index,
            status = response.status().as_u16(),
            "validated chunk response"
        );
        Ok(response)
    }

    /// The benchmark samples smaller ranges without changing the true object
    /// length or the manifest's bounded chunk layout.
    pub(crate) async fn range_bytes(
        &self,
        manifest: &Manifest,
        start: u64,
        end: u64,
    ) -> Result<Response> {
        manifest.validate()?;
        if start > end || end >= manifest.file_size {
            return Err(Error::InvalidInput(
                "requested sample byte range is outside the object".into(),
            ));
        }
        let validator = validator(&manifest.source);
        if validator.is_none() && manifest.expected_sha256.is_none() {
            return Err(Error::Protocol("source has no strong ETag or usable Last-Modified; supply a trusted --sha256 to protect final reconstruction".into()));
        }
        let mut request = self
            .client
            .get(checked_url(&manifest.url)?)
            .header(header::ACCEPT_ENCODING, "identity")
            .header(header::RANGE, format!("bytes={start}-{end}"));
        if let Some(value) = validator {
            request = request.header(header::IF_RANGE, value);
        }
        let response = request.send().await.map_err(network)?;
        tracing::debug!(
            start,
            end,
            status = response.status().as_u16(),
            "range response"
        );
        validate_status(&response, validator.is_some())?;
        let actual = content_range(&response)?;
        if actual != (start, end, manifest.file_size) {
            return Err(Error::Protocol(format!(
                "Content-Range {actual:?} does not match ({start}, {end}, {})",
                manifest.file_size
            )));
        }
        validate_payload_headers(&response, end - start + 1)?;
        if response.url().as_str() != manifest.source.final_url {
            return Err(Error::SourceChanged(
                "final redirect URL differs from the manifest".into(),
            ));
        }
        for (name, expected) in [
            (header::ETAG, &manifest.source.etag),
            (header::LAST_MODIFIED, &manifest.source.last_modified),
        ] {
            if let Some(expected) = expected
                && optional_header(&response, name.clone())?.as_ref() != Some(expected)
            {
                return Err(Error::SourceChanged(format!(
                    "{name} changed or disappeared"
                )));
            }
        }
        Ok(response)
    }
}

pub fn validator(source: &Source) -> Option<&str> {
    source
        .etag
        .as_deref()
        .filter(|v| v.starts_with('"') && v.ends_with('"'))
        .or_else(|| {
            source
                .last_modified
                .as_deref()
                .filter(|v| httpdate::parse_http_date(v).is_ok())
        })
}

fn checked_url(value: &str) -> Result<Url> {
    validate_url(value)?;
    Url::parse(value).map_err(|_| Error::InvalidInput("invalid HTTP(S) URL".into()))
}

pub(crate) fn network(error: reqwest::Error) -> Error {
    Error::Network(error.without_url())
}

fn optional_header(response: &Response, name: header::HeaderName) -> Result<Option<String>> {
    let mut values = response.headers().get_all(&name).iter();
    let value = values
        .next()
        .map(|v| v.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| Error::Protocol(format!("non-text {name}")))?;
    if values.next().is_some() {
        return Err(Error::Protocol(format!("duplicate {name}")));
    }
    Ok(value)
}

fn validate_status(response: &Response, conditional: bool) -> Result<()> {
    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        return Ok(());
    }
    if status == StatusCode::OK {
        return Err(if conditional {
            Error::SourceChanged(
                "server returned 200 to a conditional range (object changed or Range unsupported)"
                    .into(),
            )
        } else {
            Error::Protocol("server ignored Range (200 OK); safe byte ranges are required".into())
        });
    }
    if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(Error::SourceChanged(format!("server returned {status}")));
    }
    let retry_after = optional_header(response, header::RETRY_AFTER)?
        .as_deref()
        .and_then(parse_retry_after);
    Err(Error::Http {
        status: status.as_u16(),
        message: status
            .canonical_reason()
            .unwrap_or("unexpected status")
            .into(),
        retry_after,
    })
}

fn validate_payload_headers(response: &Response, expected: u64) -> Result<()> {
    if let Some(encoding) = optional_header(response, header::CONTENT_ENCODING)?
        && !encoding.eq_ignore_ascii_case("identity")
    {
        return Err(Error::Protocol(
            "encoded range bodies are unsafe; identity encoding required".into(),
        ));
    }
    if let Some(length) = optional_header(response, header::CONTENT_LENGTH)?
        && decimal(&length) != Some(expected)
    {
        return Err(Error::Protocol(format!(
            "Content-Length does not match requested {expected} bytes"
        )));
    }
    if optional_header(response, header::CONTENT_TYPE)?
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("multipart/"))
    {
        return Err(Error::Protocol("multipart ranges are unsupported".into()));
    }
    Ok(())
}

fn content_range(response: &Response) -> Result<(u64, u64, u64)> {
    let value = optional_header(response, header::CONTENT_RANGE)?
        .ok_or_else(|| Error::Protocol("missing Content-Range".into()))?;
    parse_content_range(&value).ok_or_else(|| Error::Protocol("malformed Content-Range".into()))
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total) = (decimal(start)?, decimal(end)?, decimal(total)?);
    (start <= end && end < total).then_some((start, end, total))
}

fn decimal(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        None
    } else {
        value.parse().ok()
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if value.bytes().all(|b| b.is_ascii_digit()) && !value.is_empty() {
        return value.parse::<u64>().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_range_parser() {
        assert_eq!(parse_content_range("bytes 10-19/100"), Some((10, 19, 100)));
        for bad in [
            "bytes */100",
            "bytes 0-100/100",
            "bytes +1-2/100",
            "bytes 9-2/10",
            "bytes 0-1/*",
            "bytes 0-1/0",
            "bytes 0-1/18446744073709551616",
        ] {
            assert_eq!(parse_content_range(bad), None, "{bad}");
        }
    }
    #[test]
    fn retry_after_seconds_dates_and_invalid() {
        assert_eq!(parse_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("nonsense"), None);
        assert_eq!(parse_retry_after("+7"), None);
    }

    #[test]
    fn validators_prefer_strong_etag_and_fall_back_from_weak() {
        let mut source = Source {
            etag: Some("\"strong\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            content_type: None,
            final_url: "https://example.com/file".into(),
            accept_ranges: true,
        };
        assert_eq!(validator(&source), Some("\"strong\""));
        source.etag = Some("W/\"weak\"".into());
        assert_eq!(validator(&source), source.last_modified.as_deref());
        source.last_modified = None;
        assert_eq!(validator(&source), None);
    }

    #[test]
    fn probe_and_manifest_share_url_validation() {
        for bad in [
            "http:example.com/file",
            "http://example.com/a#b",
            "https://user:secret@example.com/x",
            "https:\\example.com/file",
        ] {
            assert!(checked_url(bad).is_err());
        }
        assert!(checked_url("https://example.com/file?token=allowed").is_ok());
    }

    #[tokio::test]
    async fn network_error_does_not_display_query_tokens() {
        let bad = reqwest::Client::new()
            .get("file:///missing?access_token=secret-query-value")
            .send()
            .await
            .unwrap_err();
        let redacted = network(bad);
        assert!(!redacted.to_string().contains("secret-query-value"));
    }
}
