//! The HTTP side of the API: routing `/rest/<call>`, reading a GET or a form POST,
//! CORS, and turning an answer into a response. What a call means is in `core::api`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};

use super::body::{empty, full, status};
use super::{media, share, Body, Http};
use elbert_subsonic_core::api::{dispatch, extensions, Params};
use elbert_subsonic_core::auth::authenticate;
use elbert_subsonic_core::backend::Peer;
use elbert_subsonic_core::respond::{self, ApiError, Format, Rendered};

/// More than a big playlist's worth of `songId=` in a form.
const MAX_FORM: usize = 8 * 1024 * 1024;

/// An answer as a response: never cached.
fn reply(r: Rendered) -> Response<Body> {
    let mut res = Response::new(full(r.body));
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(r.content_type),
    );
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

pub async fn handle(
    http: Arc<Http>,
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<Body>, Infallible> {
    let mut res = route(&http, req, peer).await;
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Range"),
    );
    Ok(res)
}

async fn route(http: &Http, req: Request<Incoming>, peer: SocketAddr) -> Response<Body> {
    let srv = &http.srv;
    let caps = srv.backend.capabilities();
    let method = req.method().clone();
    if method == Method::OPTIONS {
        return status(StatusCode::NO_CONTENT);
    }
    let head = method == Method::HEAD;
    if method != Method::GET && method != Method::POST && !head {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = req.uri().path().to_string();
    let Some(call) = path.strip_prefix("/rest/") else {
        let (parts, _) = req.into_parts();
        let req = Request::from_parts(parts, ());
        return match path.as_str() {
            "/" | "/index.html" => media::landing_page(&srv.name()),
            _ => match path.strip_prefix("/share/") {
                Some(rest) => share::share_route(http, &req, rest, head).await,
                None => status(StatusCode::NOT_FOUND),
            },
        };
    };
    let call = call.strip_suffix(".view").unwrap_or(call).to_string();

    let mut params = req.uri().query().map(Params::parse).unwrap_or_default();
    let (parts, body) = req.into_parts();
    let mut json_body: Option<Vec<u8>> = None;
    if method == Method::POST {
        let kind = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let Ok(collected) = Limited::new(body, MAX_FORM).collect().await else {
            return status(StatusCode::PAYLOAD_TOO_LARGE);
        };
        let bytes = collected.to_bytes();
        if kind.starts_with("application/x-www-form-urlencoded") {
            params.extend(Params::parse(&String::from_utf8_lossy(&bytes)));
        } else if kind.contains("json") {
            json_body = Some(bytes.to_vec());
        }
    }
    let params = Params(params);
    let format = Format::from_params(params.get("f"), params.get("callback"));
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    let req = Request::from_parts(parts, ());

    // The one call that must answer without a login.
    if call == "getOpenSubsonicExtensions" {
        return reply(respond::ok(&format, extensions(caps)));
    }

    let who = match authenticate(
        srv,
        &params,
        &Peer {
            ip: Some(peer.ip()),
        },
    )
    .await
    {
        Ok(who) => who,
        Err(e) => return reply(respond::failed(&format, &e)),
    };
    srv.saw_client(peer.ip(), params.get("c").unwrap_or(""));

    // Answers that are bytes rather than a document.
    let bytes = match call.as_str() {
        "stream" => Some(media::stream(http, &req, &params, &who, head).await),
        "download" => Some(media::download(srv, &req, &params, &who, head).await),
        "getCoverArt" => Some(media::cover_art(http, &req, &params, &who, head).await),
        "getAvatar" => Some(Err(ApiError::not_found("Avatar"))),
        "getCaptions" => Some(Err(ApiError::not_found("Captions"))),
        #[cfg(feature = "hls")]
        "hls.m3u8" | "hls" => Some(super::hls::hls(srv, &req, &params, &who).await),
        // This call's failures are HTTP statuses, by the extension's rules.
        #[cfg(feature = "transcode")]
        "getTranscodeStream" if caps.transcoding => {
            return match super::transcode::transcode_stream(srv, &req, &params, &who, head).await {
                Ok(res) => res,
                Err(e) => {
                    let code = if e.0 == respond::ERR_NOT_FOUND {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    let mut res = Response::new(full(e.1));
                    *res.status_mut() = code;
                    res
                }
            };
        }
        _ => None,
    };
    match bytes {
        Some(Ok(res)) => return res,
        Some(Err(e)) => return reply(respond::failed(&format, &e)),
        None => {}
    }

    #[cfg(not(feature = "transcode"))]
    let _ = json_body;
    #[cfg(feature = "transcode")]
    if call == "getTranscodeDecision" && caps.transcoding {
        let answer = super::transcode::decision(
            http,
            &params,
            &who,
            json_body.as_deref(),
            method == Method::POST,
        )
        .await;
        return doc(&format, answer, head);
    }
    let answer = dispatch(srv, &call, &params, &who, &host).await;
    doc(&format, answer, head)
}

/// An answer that is a document, as a response.
fn doc(format: &Format, answer: Result<serde_json::Value, ApiError>, head: bool) -> Response<Body> {
    let mut res = reply(match answer {
        Ok(payload) => respond::ok(format, payload),
        Err(e) => respond::failed(format, &e),
    });
    if head {
        *res.body_mut() = empty();
    }
    res
}
