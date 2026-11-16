//! The public share pages: `/share/<id>` and the audio and covers it links to.

use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};

use super::body::{full, status};
use super::{media, serve_file, Body, Http};
use elbert_subsonic_core::backend::CoverRef;
use elbert_subsonic_core::util::{audio_mime, percent_decode, percent_encode};
use elbert_subsonic_core::wall_ms;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/share/<id>` (the page), `/share/<id>/a/<song>` and `/share/<id>/c/<song>`.
/// No login: holding the id is what a share is. A song not in the share is
/// not found, so an id opens only what was shared.
pub(super) async fn share_route(
    http: &Http,
    req: &Request<()>,
    rest: &str,
    head: bool,
) -> Response<Body> {
    let srv = &http.srv;
    let mut parts = rest.splitn(3, '/');
    let id = parts.next().unwrap_or("");
    let kind = parts.next();
    let song = parts.next().map(percent_decode);
    let share = srv.state.lock().unwrap().shares.get(id).cloned();
    let Some(share) = share.filter(|s| s.expires_ms.is_none_or(|when| when > wall_ms())) else {
        return status(StatusCode::NOT_FOUND);
    };
    // A share opens the library of the account that made it.
    let Ok(Some(owner)) = srv.who(&share.user).await else {
        return status(StatusCode::NOT_FOUND);
    };
    let Ok(cat) = srv.catalog(&owner).await else {
        return status(StatusCode::NOT_FOUND);
    };
    match (kind, song) {
        (None, _) => {
            if !head {
                {
                    let mut state = srv.state.lock().unwrap();
                    if let Some(stored) = state.shares.get_mut(id) {
                        stored.visits += 1;
                        stored.last_visited_ms = Some(wall_ms());
                    }
                }
                srv.save_soon();
            }
            let title = if share.description.is_empty() {
                format!("Shared by {}", share.user)
            } else {
                share.description.clone()
            };
            let items: String = share
                .songs
                .iter()
                .filter_map(|s| cat.song(s))
                .map(|s| {
                    let url = percent_encode(&s.id);
                    let cover = if cat.album_cover_of(s).is_some() {
                        format!(r#"<img src="/share/{id}/c/{url}" alt="" loading="lazy">"#)
                    } else {
                        String::new()
                    };
                    format!(
                        r#"<li>{cover}<div><b>{}</b><br><small>{} · {}</small><audio controls preload="none" src="/share/{id}/a/{url}"></audio></div></li>"#,
                        escape(&s.title),
                        escape(&s.artist),
                        escape(&s.album),
                    )
                })
                .collect();
            let page = SHARE_PAGE
                .replace("{{title}}", &escape(&title))
                .replace("{{items}}", &items);
            let mut res = Response::new(full(page));
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            res.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        (Some(kind), Some(song)) if share.songs.contains(&song) => {
            let Some(found) = cat.song(&song) else {
                return status(StatusCode::NOT_FOUND);
            };
            match kind {
                "a" => serve_file(req, &found.path, audio_mime(&found.path), false, head).await,
                "c" => match cat
                    .album_cover_of(found)
                    .and_then(|c| cat.covers.get(&c).cloned())
                {
                    Some(cover) => {
                        let cover = CoverRef(cover.to_string());
                        media::cover(srv, req, &owner, &cover, Some(160), head)
                            .await
                            .unwrap_or_else(|_| status(StatusCode::NOT_FOUND))
                    }
                    None => status(StatusCode::NOT_FOUND),
                },
                _ => status(StatusCode::NOT_FOUND),
            }
        }
        _ => status(StatusCode::NOT_FOUND),
    }
}

const SHARE_PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{{title}}</title>
<style>
:root{color-scheme:light dark;font:16px/1.4 system-ui,sans-serif}
body{margin:0 auto;padding:16px;max-width:40rem}h1{font-size:1.4rem}ul{list-style:none;padding:0}
li{display:flex;gap:12px;margin:0 0 16px}img{width:72px;height:72px;border-radius:10px;object-fit:cover}
li>div{flex:1;min-width:0}audio{width:100%;margin-top:6px}
</style></head><body><h1>{{title}}</h1><ul>{{items}}</ul></body></html>"#;
