//! The response body type the host speaks, and the helpers that build one.

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Frame;
use hyper::{Response, StatusCode};
use tokio::sync::mpsc;

/// The body type of every response.
pub type Body = BoxBody<Bytes, std::io::Error>;

/// An empty body.
pub fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// A body holding `data`.
pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

/// An empty response with this status.
pub fn status(code: StatusCode) -> Response<Body> {
    let mut res = Response::new(full(code.canonical_reason().unwrap_or("").to_string()));
    *res.status_mut() = code;
    res
}

/// A body fed from a channel, for bytes produced by a task.
pub fn channel_body(capacity: usize) -> (mpsc::Sender<Result<Bytes, std::io::Error>>, Body) {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(capacity);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item.map(Frame::data), rx))
    });
    (tx, http_body_util::StreamBody::new(stream).boxed())
}
