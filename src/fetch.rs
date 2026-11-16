//! The HTTP client the `podcasts` feature fetches feeds and episodes with: hyper over rustls
//! (ring, so there is no OpenSSL to cross-compile). Self-contained on purpose: this tree
//! must not reach into the app that embeds it.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use rustls::{ClientConfig, RootCertStore};

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

fn crypto() -> Arc<rustls::crypto::CryptoProvider> {
    static P: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider()))
        .clone()
}

/// The trust roots for upstream HTTPS: the system's where it can be read,
/// with Mozilla's set (webpki-roots) underneath, so a self-hosted server with
/// a public certificate works everywhere and one behind a private CA works
/// wherever the system trusts it.
fn roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    // rustls-native-certs doesn't know Android; its system store is a folder
    // of PEM files.
    #[cfg(target_os = "android")]
    if let Ok(dir) = std::fs::read_dir("/system/etc/security/cacerts") {
        use rustls_pki_types::{pem::PemObject, CertificateDer};
        for entry in dir.flatten() {
            if let Ok(cert) = CertificateDer::from_pem_file(entry.path()) {
                let _ = roots.add(cert);
            }
        }
    }
    roots
}

fn client() -> &'static HttpClient {
    static C: OnceLock<HttpClient> = OnceLock::new();
    C.get_or_init(|| {
        let tls = ClientConfig::builder_with_provider(crypto())
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_root_certificates(roots())
            .with_no_client_auth();
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(Duration::from_secs(10)));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .build(https)
    })
}

/// Sends `build`'s request, following up to five redirects (a plugin's stream
/// URL is often a redirect to a CDN). The builder is called once per hop with
/// the URL to request.
pub async fn send_following(
    url: &str,
    build: impl Fn(&str) -> Result<Request<Full<Bytes>>, String>,
) -> Result<Response<hyper::body::Incoming>, String> {
    let mut url = url.to_string();
    for _ in 0..6 {
        let req = build(&url)?;
        let res = tokio::time::timeout(Duration::from_secs(20), client().request(req))
            .await
            .map_err(|_| "the server took too long to answer".to_string())?
            .map_err(|e| e.to_string())?;
        if res.status().is_redirection() {
            if let Some(loc) = res
                .headers()
                .get(hyper::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            {
                url = resolve_url(&url, loc);
                continue;
            }
        }
        return Ok(res);
    }
    Err("too many redirects".into())
}

/// `loc` read against `base`, for the forms a `Location` header takes.
fn resolve_url(base: &str, loc: &str) -> String {
    if loc.contains("://") {
        return loc.to_string();
    }
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let host_end = base[scheme_end..]
        .find('/')
        .map(|i| i + scheme_end)
        .unwrap_or(base.len());
    if let Some(rest) = loc.strip_prefix("//") {
        return format!("{}{}", &base[..scheme_end], rest);
    }
    if loc.starts_with('/') {
        return format!("{}{}", &base[..host_end], loc);
    }
    let path = &base[..base[..base.find('?').unwrap_or(base.len())]
        .rfind('/')
        .map(|i| i + 1)
        .unwrap_or(base.len())];
    format!("{path}{loc}")
}
