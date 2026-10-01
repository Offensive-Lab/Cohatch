//! Offline streaming HTTP fixture. No test depends on DNS or a public service.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

#[derive(Clone, Debug)]
pub enum Behavior {
    Correct,
    IgnoreRange,
    WrongRange,
    WrongTotal,
    MissingRange,
    WrongLength,
    ExtraChunkedBody,
    ShortChunkedOnce,
    Encoded,
    Delay(Duration),
    DropOnce,
    TruncateOnce,
    StatusOnce(u16),
    ChangedEtag,
    LastModifiedOnly,
    ChangedLastModified,
    NoValidators,
    /// Preserve the first completed chunk while later chunks are in flight.
    SlowAfter(u64),
}

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub range: String,
    pub if_range: Option<String>,
    pub accept_encoding: Option<String>,
    pub at: Instant,
}

struct State {
    behavior: Behavior,
    attempted: usize,
    requests: Vec<Request>,
}

pub struct Server {
    pub url: String,
    pub size: u64,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Server {
    pub async fn start(size: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            behavior: Behavior::Correct,
            attempted: 0,
            requests: Vec::new(),
        }));
        let handler_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            // JoinSet aborts all open connections when Server is dropped.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let state = Arc::clone(&handler_state);
                        connections.spawn(async move {
                            // Rejected responses and cancellation legitimately close sockets.
                            let _ = serve(stream, size, state).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            url: format!("http://{address}/deterministic.bin"),
            size,
            state,
            task,
        }
    }

    pub fn behavior(&self, behavior: Behavior) {
        let mut state = self.state.lock().unwrap();
        state.behavior = behavior;
        state.attempted = 0;
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Deterministic source bytes at any offset, using at most one small buffer.
pub fn fill_bytes(offset: u64, buffer: &mut [u8]) {
    for (i, byte) in buffer.iter_mut().enumerate() {
        *byte = ((offset + i as u64) % 251) as u8;
    }
}

pub fn source_sha256(size: u64) -> String {
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut position = 0;
    while position < size {
        let count = (size - position).min(buffer.len() as u64) as usize;
        fill_bytes(position, &mut buffer[..count]);
        digest.update(&buffer[..count]);
        position += count as u64;
    }
    hex::encode(digest.finalize())
}

async fn serve(mut stream: TcpStream, size: u64, state: Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut headers = Vec::new();
    loop {
        let mut byte = [0];
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut byte)).await,
            Ok(Ok(_))
        ) {
            return Ok(());
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 32768 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&headers);
    let mut lines = text.lines();
    let method = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let range = headers.get("range").cloned().unwrap_or_default();
    let (behavior, attempt) = {
        let mut state = state.lock().unwrap();
        state.requests.push(Request {
            method,
            range: range.clone(),
            if_range: headers.get("if-range").cloned(),
            accept_encoding: headers.get("accept-encoding").cloned(),
            at: Instant::now(),
        });
        let attempt = state.attempted;
        state.attempted += 1;
        (state.behavior.clone(), attempt)
    };
    let Some((start, end)) = range
        .strip_prefix("bytes=")
        .and_then(|range| range.split_once('-'))
        .and_then(|(start, end)| Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?)))
    else {
        stream
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    };
    if start > end || end >= size {
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    match behavior {
        Behavior::Delay(delay) => tokio::time::sleep(delay).await,
        Behavior::DropOnce if attempt == 0 => return Ok(()),
        Behavior::StatusOnce(status) if attempt == 0 => {
            let extra = if status == 429 {
                "Retry-After: 1\r\n"
            } else {
                ""
            };
            stream.write_all(format!("HTTP/1.1 {status} Test Failure\r\nContent-Length: 0\r\n{extra}Connection: close\r\n\r\n").as_bytes()).await?;
            return Ok(());
        }
        _ => {}
    }
    // Give parallel in-flight responses time to observe the shared rate-limit
    // deadline before they finish and their workers request another chunk.
    if matches!(behavior, Behavior::StatusOnce(429)) && attempt > 0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let length = end - start + 1;
    let status = if matches!(behavior, Behavior::IgnoreRange) {
        "200 OK"
    } else {
        "206 Partial Content"
    };
    let range_start = if matches!(behavior, Behavior::WrongRange) {
        start + 1
    } else {
        start
    };
    let range_size = if matches!(behavior, Behavior::WrongTotal) {
        size + 1
    } else {
        size
    };
    let etag = if matches!(behavior, Behavior::ChangedEtag) {
        "ETag: \"cohatch-test-v2\"\r\n"
    } else if matches!(
        behavior,
        Behavior::LastModifiedOnly | Behavior::NoValidators
    ) {
        ""
    } else {
        "ETag: \"cohatch-test-v1\"\r\n"
    };
    let modified = if matches!(behavior, Behavior::NoValidators) {
        ""
    } else if matches!(behavior, Behavior::ChangedLastModified) {
        "Last-Modified: Thu, 02 Jan 2025 00:00:00 GMT\r\n"
    } else {
        "Last-Modified: Wed, 01 Jan 2025 00:00:00 GMT\r\n"
    };
    let encoding = if matches!(behavior, Behavior::Encoded) {
        "Content-Encoding: gzip\r\n"
    } else {
        ""
    };
    let range_header = if matches!(behavior, Behavior::MissingRange) {
        String::new()
    } else {
        format!("Content-Range: bytes {range_start}-{end}/{range_size}\r\n")
    };
    let chunked = matches!(
        behavior,
        Behavior::ExtraChunkedBody | Behavior::ShortChunkedOnce
    );
    let length_header = if chunked {
        "Transfer-Encoding: chunked\r\n".to_owned()
    } else if matches!(behavior, Behavior::WrongLength) {
        format!("Content-Length: {}\r\n", length + 1)
    } else {
        format!("Content-Length: {length}\r\n")
    };
    stream.write_all(format!("HTTP/1.1 {status}\r\n{length_header}{range_header}Accept-Ranges: bytes\r\n{etag}{modified}Content-Type: application/octet-stream\r\n{encoding}Connection: close\r\n\r\n").as_bytes()).await?;
    let send_length = if matches!(behavior, Behavior::ExtraChunkedBody) {
        length + 1
    } else if matches!(
        behavior,
        Behavior::TruncateOnce | Behavior::ShortChunkedOnce
    ) && attempt == 0
    {
        length / 2
    } else {
        length
    };
    let slow = matches!(behavior, Behavior::SlowAfter(threshold) if start >= threshold);
    let mut buffer = vec![0; if slow { 4096 } else { 65536 }];
    let mut sent = 0;
    while sent < send_length {
        let count = (send_length - sent).min(buffer.len() as u64) as usize;
        fill_bytes(start + sent, &mut buffer[..count]);
        if chunked {
            stream
                .write_all(format!("{count:x}\r\n").as_bytes())
                .await?;
        }
        stream.write_all(&buffer[..count]).await?;
        if chunked {
            stream.write_all(b"\r\n").await?;
        }
        sent += count as u64;
        if slow {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    if chunked {
        stream.write_all(b"0\r\n\r\n").await?;
    }
    stream.shutdown().await?;
    Ok(())
}
