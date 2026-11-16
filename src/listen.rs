//! Binding the host's listening socket.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::TcpListener;

/// Listens on `port`, or on any free port when that one is taken.
pub async fn bind(port: u16) -> Result<TcpListener, String> {
    let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    if port != 0 {
        if let Ok(l) = TcpListener::bind(SocketAddr::new(any, port)).await {
            return Ok(l);
        }
    }
    TcpListener::bind(SocketAddr::new(any, 0))
        .await
        .map_err(|e| format!("could not listen: {e}"))
}
