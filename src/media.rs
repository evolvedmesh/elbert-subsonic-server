//! The bytes a client asks for: audio, covers and the page the QR code opens. What they are
//! and where they live is the [`Backend`](elbert_subsonic_core::backend::Backend)'s answer
//! ([`Media`]): a file served here with `Range`, bytes, or a redirect.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};

use super::body::{empty, full, status};
use super::{serve_file, Body, Http};
use elbert_subsonic_core::api::Params;
use elbert_subsonic_core::auth::Who;
use elbert_subsonic_core::backend::{CoverRef, Media, MediaRequest, TranscodeFormat};
use elbert_subsonic_core::browse::View;
use elbert_subsonic_core::catalog::Catalog;
use elbert_subsonic_core::respond::ApiError;
use elbert_subsonic_core::Server;

/// A file on disk to send, with what the catalog knows about it. The length, the rate and
/// the transcode request are read by the `transcode` and `hls` features.
#[allow(dead_code)]
pub(super) struct Source {
    pub path: PathBuf,
    pub mime: String,
    pub duration_ms: u64,
    /// Kbit/s, 0 when unknown.
    pub bitrate: u32,
    /// The backend asked for it transcoded: the format and the start, in milliseconds.
    pub forced: Option<(TranscodeFormat, u64)>,
}

/// What a request for audio comes to.
pub(super) enum Located {
    File(Source),
    /// Answered already: a redirect, or bytes the backend holds.
    Reply(Response<Body>),
}

/// Asks the backend for `id`'s audio.
pub(super) async fn find(
    srv: &Arc<Server>,
    id: &str,
    who: &Who,
    download: bool,
) -> Result<Located, ApiError> {
    let media = srv
        .media(
            who,
            MediaRequest {
                id: id.to_string(),
                download,
            },
        )
        .await?;
    let (path, mime, forced) = match media {
        Media::File { path, mime } => (path, mime, None),
        Media::Transcode {
            source,
            format,
            offset_ms,
        } => {
            let mime = elbert_subsonic_core::util::audio_mime(&source).to_string();
            (source, mime, Some((format, offset_ms)))
        }
        Media::Redirect { .. } => return Ok(Located::Reply(redirect_reply(&media))),
        Media::Bytes { data, mime } => {
            return Ok(Located::Reply(bytes_reply(&data, &mime, false, true)))
        }
        Media::NotFound => return Err(ApiError::not_found("Media")),
    };
    // A podcast episode is not in the catalog: it has no length or rate on record.
    let cat = srv.catalog(who).await?;
    let (duration_ms, bitrate) = match cat.song(id) {
        Some(song) => {
            let computed = (song.size * 8).checked_div(song.duration_ms).unwrap_or(0) as u32;
            (
                song.duration_ms,
                if song.bitrate > 0 {
                    song.bitrate
                } else {
                    computed
                },
            )
        }
        None => (0, 0),
    };
    Ok(Located::File(Source {
        path,
        mime,
        duration_ms,
        bitrate,
        forced,
    }))
}

/// `find`, for the calls that need the file itself (a redirect has none to look into).
#[cfg(any(feature = "transcode", feature = "hls"))]
pub(super) async fn find_file(srv: &Arc<Server>, id: &str, who: &Who) -> Result<Source, ApiError> {
    match find(srv, id, who, false).await? {
        Located::File(source) => Ok(source),
        Located::Reply(_) => Err(ApiError(
            elbert_subsonic_core::respond::ERR_GENERIC,
            "This server does not send that as a file".into(),
        )),
    }
}

/// `302` to where the backend says, as `Media::redirect_head` words it.
fn redirect_reply(media: &Media) -> Response<Body> {
    let Some(head) = media.redirect_head() else {
        return status(StatusCode::NOT_FOUND);
    };
    let mut res = status(StatusCode::from_u16(head.status).unwrap_or(StatusCode::FOUND));
    for (name, value) in head.headers {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) else {
            return status(StatusCode::BAD_GATEWAY);
        };
        res.headers_mut().insert(name, value);
    }
    res
}

fn bytes_reply(data: &[u8], mime: &str, head: bool, cacheable: bool) -> Response<Body> {
    let mut res = Response::new(if head {
        empty()
    } else {
        full(Bytes::copy_from_slice(data))
    });
    let h = res.headers_mut();
    if let Ok(mime) = HeaderValue::from_str(mime) {
        h.insert(header::CONTENT_TYPE, mime);
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(data.len()));
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if cacheable {
            "max-age=3600"
        } else {
            "no-cache"
        }),
    );
    res
}

/// `download`: the file, always.
pub async fn download(
    srv: &Arc<Server>,
    req: &Request<()>,
    p: &Params,
    who: &Who,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    who.require(who.account.download, "Downloading")?;
    let media = match find(srv, p.need("id")?, who, true).await? {
        Located::File(media) => media,
        Located::Reply(res) => return Ok(res),
    };
    let res = serve_file(req, &media.path, &media.mime, false, head).await;
    if res.status() == StatusCode::NOT_FOUND {
        return Err(ApiError::not_found("File"));
    }
    Ok(res)
}

/// `stream`. The file as it is, unless the `transcode` feature is there and what was asked
/// (a format, a bit-rate limit, a start) calls for more.
pub async fn stream(
    http: &Http,
    req: &Request<()>,
    p: &Params,
    who: &Who,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    who.require(who.account.stream, "Streaming")?;
    let media = match find(&http.srv, p.need("id")?, who, false).await? {
        Located::File(media) => media,
        Located::Reply(res) => return Ok(res),
    };
    #[cfg(feature = "transcode")]
    if let Some(res) = super::transcode::stream(http, p, who, &media, head).await {
        return Ok(res);
    }
    let res = serve_file(req, &media.path, &media.mime, false, head).await;
    if res.status() == StatusCode::NOT_FOUND {
        return Err(ApiError::not_found("File"));
    }
    Ok(res)
}

/// `getCoverArt`: the picture, scaled by the backend when `size` is given.
pub async fn cover_art(
    http: &Http,
    req: &Request<()>,
    params: &Params,
    who: &Who,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    let srv = &http.srv;
    let cat = srv.catalog(who).await?;
    let notes = srv.notes(who).await?;
    let view = View {
        srv,
        cat: &cat,
        who,
        notes: &notes,
    };
    let id = params.need("id")?;
    let missing = || ApiError::not_found("Cover art");
    #[cfg(feature = "podcasts")]
    let podcast = super::podcast::cover_url(srv, id);
    #[cfg(not(feature = "podcasts"))]
    let podcast: Option<String> = None;
    let reference = match podcast {
        Some(url) => url,
        None => {
            let cover_id = cover_id_for(&view, &cat, id).ok_or_else(missing)?;
            cat.covers.get(&cover_id).ok_or_else(missing)?.to_string()
        }
    };
    let size = params.int("size").filter(|&s| s > 0).map(|s| s as u32);
    cover(srv, req, who, &CoverRef(reference), size, head).await
}

/// Clients ask for covers by `coverArt` id, but also by the id of a song,
/// album, artist or playlist.
fn cover_id_for(view: &View, cat: &Catalog, id: &str) -> Option<Arc<str>> {
    if cat.covers.contains_key(id) {
        return Some(Arc::from(id));
    }
    if let Some(song) = view.visible_song(id) {
        return cat.album_cover_of(song);
    }
    if let Some(album) = cat.album(id) {
        return album.cover.clone();
    }
    if let Some(artist) = cat.artist(id) {
        return artist.cover.clone();
    }
    view.playlist_cover(id)
}

/// The picture `cover` names, as the backend has it: a file (cached by the client for an
/// hour), bytes, or a redirect.
pub async fn cover(
    srv: &Arc<Server>,
    req: &Request<()>,
    who: &Who,
    cover: &CoverRef,
    size: Option<u32>,
    head: bool,
) -> Result<Response<Body>, ApiError> {
    let missing = || ApiError::not_found("Cover art");
    match srv.cover(who, cover, size).await? {
        Media::File { path, mime } => {
            let res = serve_file(req, &path, &mime, true, head).await;
            if res.status() == StatusCode::NOT_FOUND {
                return Err(missing());
            }
            Ok(res)
        }
        Media::Bytes { data, mime } => Ok(bytes_reply(&data, &mime, head, true)),
        media @ Media::Redirect { .. } => Ok(redirect_reply(&media)),
        Media::Transcode { .. } | Media::NotFound => Err(missing()),
    }
}

/// What a phone's camera opens from the host's QR code. The login is in the
/// link's fragment, which a browser never sends: the page reads it itself.
pub fn landing_page(name: &str) -> Response<Body> {
    let escaped = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut res = Response::new(full(LANDING_PAGE.replace("{{name}}", &escaped)));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

const LANDING_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{name}} · Elbert</title>
<style>
:root{color-scheme:light dark;font:16px/1.45 system-ui,sans-serif}
body{display:grid;place-items:center;min-height:100vh;margin:0;padding:16px;box-sizing:border-box}
main{width:100%;max-width:26rem}h1{font-size:1.4rem;margin:0 0 .25rem}p{margin:0 0 1rem;opacity:.8}
label{display:block;font-size:.85rem;margin:.9rem 0 .25rem;opacity:.8}
input{width:100%;box-sizing:border-box;padding:.65rem .75rem;font:inherit;border:1px solid #8888;border-radius:10px;background:transparent;color:inherit}
</style></head><body><main>
<h1>{{name}}</h1>
<p id="lead">Scan the code in Elbert to see how to connect.</p>
<div id="login" hidden>
<label for="a">Server address</label><input id="a" readonly>
<label for="u">Username</label><input id="u" readonly>
<label for="p">Password</label><input id="p" readonly>
<p style="margin-top:1.25rem">In another Elbert: Settings → Subsonic Server → Connect to an Elbert, then scan the code or paste this page's link.</p>
</div>
<script>
var h=new URLSearchParams(location.hash.slice(1)),u=h.get('u'),p=h.get('p');
if(u&&p){document.getElementById('lead').textContent='Type these into any Subsonic app to play this library.';
document.getElementById('login').hidden=false;
document.getElementById('a').value=location.origin;document.getElementById('u').value=u;document.getElementById('p').value=p;
document.querySelectorAll('input').forEach(function(i){i.addEventListener('focus',function(){i.select()})})}
</script></main></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redirect_is_sent_as_302_with_the_url_and_no_store() {
        let media = Media::Redirect {
            url: "https://objects.example/a?sig=1".into(),
            ttl_secs: 60,
        };
        let res = redirect_reply(&media);
        assert_eq!(res.status(), StatusCode::FOUND);
        assert_eq!(
            res.headers()[header::LOCATION],
            "https://objects.example/a?sig=1"
        );
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
    }
}
