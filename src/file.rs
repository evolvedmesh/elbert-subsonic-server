//! Serving a file from disk with `Range` and `ETag`, the way both hosts do.

use std::io::SeekFrom;
use std::path::Path;

use bytes::Bytes;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::body::{empty, status, Body};
use elbert_subsonic_core::util::{parse_range, RangeAsk};

pub(crate) const CHUNK: usize = 64 * 1024;

pub(crate) fn range_header<B>(req: &Request<B>) -> Option<String> {
    req.headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

pub(crate) fn unsatisfiable(len: u64) -> Response<Body> {
    let mut res = status(StatusCode::RANGE_NOT_SATISFIABLE);
    res.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{len}")).unwrap(),
    );
    res
}

/// A file with ranges and an ETag (length + modification time).
pub async fn serve_file<B: Sync>(
    req: &Request<B>,
    path: &Path,
    mime: &str,
    cacheable: bool,
    head: bool,
) -> Response<Body> {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return status(StatusCode::NOT_FOUND);
    };
    if !meta.is_file() {
        return status(StatusCode::NOT_FOUND);
    }
    let len = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("\"{len:x}-{mtime:x}\"");
    if req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        let mut res = status(StatusCode::NOT_MODIFIED);
        *res.body_mut() = empty();
        res.headers_mut()
            .insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
        return res;
    }
    let (start, end, partial) = if len == 0 {
        (0, 0, false)
    } else {
        match parse_range(range_header(req).as_deref(), len) {
            RangeAsk::Whole => (0, len - 1, false),
            RangeAsk::Part(a, b) => (a, b, true),
            RangeAsk::Unsatisfiable => return unsatisfiable(len),
        }
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    let mut res = Response::new(empty());
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime).unwrap());
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if cacheable {
            "max-age=3600"
        } else {
            "no-cache"
        }),
    );
    if partial {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).unwrap(),
        );
        *res.status_mut() = StatusCode::PARTIAL_CONTENT;
    }
    if head || count == 0 {
        return res;
    }
    let (tx, body) = super::body::channel_body(4);
    let path = path.to_path_buf();
    tokio::spawn(async move {
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        };
        if start > 0 {
            if let Err(e) = file.seek(SeekFrom::Start(start)).await {
                let _ = tx.send(Err(e)).await;
                return;
            }
        }
        let mut left = count;
        while left > 0 {
            let mut buf = vec![0u8; CHUNK.min(left as usize)];
            match file.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    buf.truncate(n);
                    left -= n as u64;
                    if tx.send(Ok(Bytes::from(buf))).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    *res.body_mut() = body;
    res
}
