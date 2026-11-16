//! The hyper adapter for `elbert-subsonic-core`: the pieces that turn the protocol into an HTTP
//! host on a socket. The accept loop, the LAN-peer filter, CORS and form-body handling, files
//! with `Range`, covers and audio, and the optional parts: transcoding, podcasts and HLS.
//!
//! The crate depends on core and on a `tokio::runtime::Handle` it is given, and holds no
//! globals: the app that embeds it owns the runtime and the way it is stopped. It reaches
//! nothing of the app: what only the app can do is a trait it implements
//! ([`sonic::SonicIndex`], core's `Link`) or something core's `Backend` answers.
//!
//! The entry point is [`spawn`]. The crate forbids `unsafe`.
//!
//! # Features
//!
//! All are on by default.
//!
//! - `transcode`: symphonia decoding, the MP3 and Opus encoders, the resampler. `stream` with a
//!   format, a bit-rate limit or an offset, and the `transcoding` and `transcodeOffset`
//!   extensions.
//! - `podcasts`: the feed client and parser; the podcast calls and `getPodcastEpisode`.
//! - `hls`: `hls.m3u8`.
//!
//! Without them `stream` sends the file as it is, and the calls of the missing groups are "no
//! such call".

#![forbid(unsafe_code)]
#![warn(missing_docs)]

#[cfg(feature = "transcode")]
pub mod audio;
pub mod body;
#[cfg(feature = "podcasts")]
pub mod fetch;
pub mod file;
#[cfg(feature = "hls")]
mod hls;
mod http;
pub mod listen;
pub mod media;
pub mod peer;
#[cfg(feature = "podcasts")]
pub mod podcast;
mod share;
pub mod sonic;
#[cfg(feature = "transcode")]
mod transcode;

#[cfg(feature = "transcode")]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "transcode")]
use std::sync::Mutex;

use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use body::{empty, full, status, Body};
pub use file::serve_file;
pub use listen::bind;
pub use peer::is_local_peer;

#[cfg(feature = "transcode")]
use elbert_subsonic_core::util::Bounded;
use elbert_subsonic_core::Server;

/// What one running host holds beside the library: the caches of what only this side
/// of the protocol reads.
pub struct Http {
    /// The server this host answers for.
    pub srv: Arc<Server>,
    #[cfg(feature = "transcode")]
    pub(crate) probes: Mutex<Bounded<PathBuf, (u64, audio::Probe)>>,
}

/// Serves `srv` on `port` (any free port when it is taken) on the runtime behind `rt`
/// until `stop` says so. `on_bound` gets the port once the socket is listening. The
/// returned handle ends with the server.
pub fn spawn(
    rt: &Handle,
    srv: Arc<Server>,
    port: u16,
    stop: watch::Receiver<bool>,
    on_bound: impl FnOnce(u16) + Send + 'static,
) -> JoinHandle<Result<(), String>> {
    rt.spawn(serve(srv, port, stop, on_bound))
}

async fn serve(
    srv: Arc<Server>,
    port: u16,
    stop: watch::Receiver<bool>,
    on_bound: impl FnOnce(u16),
) -> Result<(), String> {
    let listener = bind(port).await?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    on_bound(port);

    #[cfg(feature = "podcasts")]
    podcast::keep_fresh(srv.clone(), stop.clone());

    let http = Arc::new(Http {
        srv,
        #[cfg(feature = "transcode")]
        probes: Mutex::new(Bounded::new(256)),
    });
    let mut shutdown = stop.clone();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                // Plain HTTP with a password in it: this network only.
                if !is_local_peer(peer.ip()) {
                    continue;
                }
                let _ = stream.set_nodelay(true);
                let http = http.clone();
                let mut stop = stop.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| http::handle(http.clone(), req, peer));
                    let conn = hyper::server::conn::http1::Builder::new()
                        .keep_alive(true)
                        .serve_connection(TokioIo::new(stream), service);
                    tokio::select! {
                        _ = conn => {}
                        _ = stop.changed() => {}
                    }
                });
            }
            _ = shutdown.changed() => break,
        }
    }
    Ok(())
}
