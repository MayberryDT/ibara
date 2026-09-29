//! JSON over HTTP/1.1 on Unix sockets: `POST /v1` with a JSON body.
//!
//! This is the framing the TypeScript controller and its Node clients use, so
//! `ibarad` stays wire-compatible with every caller that still exists during
//! the step-2 cutover (setup, viewer and lifecycle scripts). It is deliberately
//! small: one request per connection, `Content-Length` bodies, `connection:
//! close` replies. The client also accepts chunked replies.

use crate::error::{IbaraError, Result};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const MAX_BODY: usize = 24 * 1024 * 1024;
const MAX_HEAD: usize = 16 * 1024;

/// A parsed request.
#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// The bearer token from `authorization: Bearer …`, if any.
    pub bearer: Option<String>,
    pub body: Vec<u8>,
}

async fn read_head<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<(String, Vec<(String, String)>)> {
    let mut first = String::new();
    let mut total = 0usize;
    let n = r.read_line(&mut first).await?;
    if n == 0 {
        return Err(IbaraError::new("SESSION_UNAVAILABLE", "connection closed before a request", true));
    }
    total += n;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        total += n;
        if total > MAX_HEAD {
            return Err(IbaraError::new("INVALID_ARGUMENT", "request head too large", true));
        }
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((first.trim_end().to_string(), headers))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Read one request from a server-side stream.
pub async fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut reader = BufReader::new(stream);
    let (line, headers) = read_head(&mut reader).await?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let len: usize = header(&headers, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(IbaraError::new("INVALID_ARGUMENT", "Request too large.", true));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let bearer = header(&headers, "authorization").map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string());
    Ok(Request { method, path, bearer, body })
}

/// Write a JSON reply and close.
pub async fn write_response(stream: &mut UnixStream, status: u16, body: &Value) -> Result<()> {
    let text = serde_json::to_vec(body).map_err(|e| crate::error::internal(e.to_string()))?;
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        400 => "Bad Request",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        text.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&text).await?;
    stream.shutdown().await.ok();
    Ok(())
}

/// How a failure to connect to the socket starts: nothing was sent.
const NOT_REACHED: &str = "controller socket unavailable";

/// Whether `post` failed before sending anything: the socket was missing, or
/// nothing listened on it (a daemon that is stopped or starting).
pub fn not_reached(err: &IbaraError) -> bool {
    err.code == "SESSION_UNAVAILABLE" && err.message.starts_with(NOT_REACHED)
}

/// Send one `POST /v1` and return the parsed JSON reply.
pub async fn post(socket: &Path, bearer: Option<&str>, body: &Value, timeout: Duration) -> Result<Value> {
    let fut = async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|e| IbaraError::new("SESSION_UNAVAILABLE", format!("{NOT_REACHED}: {e}"), true))?;
        let payload = serde_json::to_vec(body).map_err(|e| crate::error::internal(e.to_string()))?;
        if payload.len() > MAX_BODY {
            return Err(IbaraError::new("INVALID_ARGUMENT", "Request exceeds transport limit.", true));
        }
        let mut head = format!(
            "POST /v1 HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            payload.len()
        );
        if let Some(token) = bearer {
            head.push_str(&format!("authorization: Bearer {token}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&payload).await?;
        let mut reader = BufReader::new(stream);
        let (_status, headers) = read_head(&mut reader).await?;
        let body = if header(&headers, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
            read_chunked(&mut reader).await?
        } else if let Some(len) = header(&headers, "content-length").and_then(|v| v.parse::<usize>().ok()) {
            if len > MAX_BODY {
                return Err(crate::error::internal("Response exceeds transport limit."));
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            buf
        } else {
            let mut buf = Vec::new();
            reader.take(MAX_BODY as u64 + 1).read_to_end(&mut buf).await?;
            buf
        };
        serde_json::from_slice(&body).map_err(|_| IbaraError::new("SESSION_UNAVAILABLE", "Invalid controller response.", false))
    };
    tokio::time::timeout(timeout, fut).await.map_err(|_| {
        IbaraError::new("TIMEOUT", "Controller response timed out; reconcile before retrying an effect.", false)
    })?
}

async fn read_chunked<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        r.read_line(&mut size_line).await?;
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .map_err(|_| crate::error::internal("bad chunk size"))?;
        if size == 0 {
            let mut end = String::new();
            r.read_line(&mut end).await?;
            return Ok(out);
        }
        if out.len() + size > MAX_BODY {
            return Err(crate::error::internal("Response exceeds transport limit."));
        }
        let start = out.len();
        out.resize(start + size, 0);
        r.read_exact(&mut out[start..]).await?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf).await?;
    }
}
