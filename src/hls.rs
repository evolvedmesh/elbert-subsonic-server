//! `hls.m3u8` (the `hls` feature): a playlist that points back at `stream`.

use std::sync::Arc;

use hyper::header::{self, HeaderValue};
use hyper::{Request, Response};

use super::body::{full, Body};
use super::media::find_file;
use elbert_subsonic_core::api::Params;
use elbert_subsonic_core::auth::Who;
use elbert_subsonic_core::respond::ApiError;
use elbert_subsonic_core::util::percent_encode;
use elbert_subsonic_core::Server;

/// A one-segment playlist of the track: audio has no need of slicing, and a
/// player seeks in it by range. One line per `bitRate` makes a variant playlist.
pub async fn hls(
    srv: &Arc<Server>,
    req: &Request<()>,
    p: &Params,
    who: &Who,
) -> Result<Response<Body>, ApiError> {
    who.require(who.account.stream, "Streaming")?;
    let id = p.need("id")?;
    let media = find_file(srv, id, who).await?;
    let query: String =
        p.0.iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "u" | "t" | "s" | "p" | "v" | "c" | "apiKey" | "id"
                )
            })
            .map(|(k, v)| format!("{k}={}", percent_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
    let rates: Vec<u32> = p
        .all("bitRate")
        .iter()
        .filter_map(|r| r.parse().ok())
        .collect();
    let duration = (media.duration_ms as f64 / 1000.0).ceil() as u64;
    let segment = |rate: Option<u32>| {
        let limit = rate
            .map(|r| format!("&maxBitRate={r}&format=mp3"))
            .unwrap_or_default();
        format!("stream?{query}{limit}")
    };
    let text = if rates.len() > 1 {
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        for rate in rates {
            let wanted: Vec<String> =
                p.0.iter()
                    .filter(|(k, _)| k != "bitRate")
                    .map(|(k, v)| format!("{k}={}", percent_encode(v)))
                    .collect();
            out.push_str(&format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={}\nhls.m3u8?{}&bitRate={rate}\n",
                rate * 1000,
                wanted.join("&")
            ));
        }
        out
    } else {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:{duration}.0,\n{}\n#EXT-X-ENDLIST\n",
            segment(rates.first().copied()),
        )
    };
    let _ = req;
    let mut res = Response::new(full(text));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(res)
}
