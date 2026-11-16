//! The host on loopback over a tiny in-memory `Backend`: login, a listing, a ranged file, a
//! redirect and an unknown call, as a client sees them. The golden responses of the whole
//! protocol stay in the Elbert repository, which is the consumer of this crate.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use elbert_subsonic_core::api::{Answer, Params};
use elbert_subsonic_core::auth::Who;
use elbert_subsonic_core::backend::{
    ArtistInfo, Backend, BackendError, Capabilities, CoverRef, Edit, EditOutcome, Lockout, Lyrics,
    Media, MediaRequest, Peer, TenantId, UserNotes,
};
use elbert_subsonic_core::browse::View;
use elbert_subsonic_core::catalog::{Catalog, Pool, Raw};
use elbert_subsonic_core::respond::ApiError;
use elbert_subsonic_core::state::{Account, State};
use elbert_subsonic_core::{Ask, Boxed, Link, Server, Tell};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use zeroize::Zeroizing;

const AUDIO: &[u8] = b"0123456789abcdefghij";

struct Memory {
    catalog: Arc<Catalog>,
    file: PathBuf,
}

static CAPABILITIES: Capabilities = Capabilities {
    accounts: false,
    shares: false,
    radio: false,
    chat: false,
    podcasts: false,
    sonic: false,
    jukebox: false,
    scan: false,
    transcoding: false,
};

fn amy() -> Who {
    Who {
        name: "amy".into(),
        owner: false,
        account: Account {
            password: "sesame".into(),
            stream: true,
            ..Default::default()
        },
        mask: u64::MAX,
        tenant: TenantId(Arc::from("t1")),
        catalog_version: 1,
    }
}

impl Backend for Memory {
    fn capabilities(&self) -> &Capabilities {
        &CAPABILITIES
    }
    async fn passwords(&self, user: &str) -> Result<Vec<Zeroizing<String>>, BackendError> {
        Ok(if user == "amy" {
            vec![Zeroizing::new("sesame".to_string())]
        } else {
            vec![]
        })
    }
    async fn api_key(&self, _key_sha256: [u8; 32]) -> Result<Option<Who>, BackendError> {
        Ok(None)
    }
    async fn who(&self, user: &str) -> Result<Option<Who>, BackendError> {
        Ok((user == "amy").then(amy))
    }
    async fn auth_failed(
        &self,
        _user: Option<&str>,
        _peer: &Peer,
    ) -> Result<Lockout, BackendError> {
        Ok(Lockout::Open)
    }
    async fn catalog(
        &self,
        _tenant: &TenantId,
        _version: u64,
    ) -> Result<Arc<Catalog>, BackendError> {
        Ok(self.catalog.clone())
    }
    async fn notes(&self, _who: &Who) -> Result<Arc<UserNotes>, BackendError> {
        Ok(Arc::new(UserNotes::default()))
    }
    async fn apply(&self, _who: &Who, _edit: Edit) -> Result<EditOutcome, BackendError> {
        Err(BackendError::Failed("read only".into()))
    }
    async fn media(&self, _who: &Who, req: MediaRequest) -> Result<Media, BackendError> {
        Ok(match req.id.as_str() {
            "s1" => Media::File {
                path: self.file.clone(),
                mime: "audio/mpeg".into(),
            },
            "s2" => Media::Redirect {
                url: "https://objects.example/s2?sig=abc".into(),
                ttl_secs: 60,
            },
            _ => Media::NotFound,
        })
    }
    async fn cover(
        &self,
        _who: &Who,
        _cover: &CoverRef,
        _size: Option<u32>,
    ) -> Result<Media, BackendError> {
        Ok(Media::NotFound)
    }
    async fn lyrics(&self, _who: &Who, _song: &str) -> Result<Option<Lyrics>, BackendError> {
        Ok(None)
    }
    async fn artist_info(
        &self,
        _who: &Who,
        _artist: &str,
        _album: Option<&str>,
    ) -> Result<Option<ArtistInfo>, BackendError> {
        Ok(None)
    }
}

struct NoApp;

impl Link for NoApp {
    fn tell(&self, _event: Tell<'_>) {}
    fn ask<'a>(&'a self, _ask: Ask<'a>, _timeout: Duration) -> Boxed<'a, Option<Vec<u8>>> {
        Box::pin(async { None })
    }
    fn save_soon(&self) {}
    fn host_user(&self) -> String {
        String::new()
    }
    fn data_dir(&self) -> Option<PathBuf> {
        None
    }
    fn call<'a>(
        &'a self,
        _srv: &'a Arc<Server>,
        _view: &'a View<'a>,
        _call: &'a str,
        _p: &'a Params,
    ) -> Boxed<'a, Answer> {
        Box::pin(async { Err(ApiError(0, "none".into())) })
    }
}

struct Host {
    port: u16,
    stop: watch::Sender<bool>,
    _dir: tempfile::TempDir,
}

async fn host() -> Host {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("one.mp3");
    std::fs::write(&file, AUDIO).unwrap();
    let mut pool = Pool::default();
    let raw = ["s1", "s2"]
        .iter()
        .map(|id| Raw {
            id: id.to_string(),
            path: file.to_string_lossy().into_owned(),
            title: pool.get(id),
            artist: pool.get("Alpha"),
            album: pool.get("Album"),
            album_artist: pool.get("Alpha"),
            genre: pool.get("Rock"),
            cover: None,
            isrc: None,
            duration_ms: 3000,
            track: 1,
            disc: 1,
            year: 2020,
            bitrate: 128,
            sample_rate: 44_100,
            size: AUDIO.len() as u64,
            added_ms: 1_700_000_000_000,
        })
        .collect();
    let backend = Arc::new(Memory {
        catalog: Arc::new(Catalog::build(vec!["/".into()], raw, 0)),
        file,
    });
    let srv = Arc::new(Server::new(
        Arc::new(NoApp),
        backend,
        "Test".into(),
        Arc::new(Mutex::new(State::default())),
    ));
    let (stop, stopped) = watch::channel(false);
    let (bound, port) = tokio::sync::oneshot::channel();
    drop(elbert_subsonic_server::spawn(
        &tokio::runtime::Handle::current(),
        srv,
        0,
        stopped,
        move |p| {
            let _ = bound.send(p);
        },
    ));

    Host {
        port: port.await.unwrap(),
        stop,
        _dir: dir,
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

/// One `GET`, read to the end: the status line, the headers (lower-cased) and the body.
async fn get(port: u16, target: &str, range: Option<&str>) -> (u16, String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let range = range.map(|r| format!("Range: {r}\r\n")).unwrap_or_default();
    let request =
        format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n{range}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head, raw[split + 4..].to_vec())
}

const LOGIN: &str = "u=amy&p=sesame&v=1.16.1&c=test&f=json";

#[tokio::test]
async fn logs_in_and_lists() {
    let host = host().await;
    let (status, _, body) = get(host.port, &format!("/rest/ping?{LOGIN}"), None).await;
    assert_eq!(status, 200);
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"status\":\"ok\""), "{body}");

    let (_, _, body) = get(host.port, &format!("/rest/getSong?{LOGIN}&id=s1"), None).await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"id\":\"s1\""), "{body}");

    let (_, _, body) = get(
        host.port,
        "/rest/ping?u=amy&p=wrong&v=1.16.1&c=test&f=json",
        None,
    )
    .await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"code\":40"), "{body}");
}

#[tokio::test]
async fn serves_a_file_with_ranges_and_redirects_to_the_rest() {
    let host = host().await;
    let (status, _, body) = get(host.port, &format!("/rest/stream?{LOGIN}&id=s1"), None).await;
    assert_eq!((status, body.as_slice()), (200, AUDIO));

    let (status, head, body) = get(
        host.port,
        &format!("/rest/stream?{LOGIN}&id=s1"),
        Some("bytes=2-5"),
    )
    .await;
    assert_eq!((status, body.as_slice()), (206, &AUDIO[2..6]));
    assert!(head.contains("content-range: bytes 2-5/20"), "{head}");

    let (status, head, _) = get(host.port, &format!("/rest/stream?{LOGIN}&id=s2"), None).await;
    assert_eq!(status, 302);
    assert!(
        head.contains("location: https://objects.example/s2?sig=abc"),
        "{head}"
    );
    assert!(head.contains("cache-control: no-store"), "{head}");
}

#[tokio::test]
async fn an_unknown_call_is_an_api_error() {
    let host = host().await;
    let (_, _, body) = get(host.port, &format!("/rest/noSuchCall?{LOGIN}"), None).await;
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("\"status\":\"failed\""), "{body}");
}
