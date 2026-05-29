//! TCP-level failover proxy for R-N16.
//!
//! Models the production scenario where a single hostname
//! (e.g. `zec.rocks:443`) resolves to a different IP between connection
//! attempts because of DNS round-robin, a load-balancer rotation, or a
//! failover event. From Argos's point of view, the `lightwalletd_url` string
//! stays the same; the TCP peer behind it is different on the retry.
//!
//! The proxy binds a `TcpListener` on a random loopback port and forwards
//! each incoming connection's bytes to one of `N` configured upstreams.
//! The selection rule is a per-connection counter: connection 1 goes to
//! `upstreams[0]`, connection 2 goes to `upstreams[1]`, and so on (saturating
//! at `upstreams[last]` for every subsequent connection). This gives the
//! test deterministic control over which backend serves the retry without
//! orchestration from the test body.
//!
//! Bytes are piped raw — no h2 parsing, no TLS termination. The proxy is
//! transparent at the wire level, so anything the upstreams accept (clear
//! HTTP/2 over loopback in our case) passes through unmodified.
//!
//! Gated on `argos-network` to match the rest of the test fixtures.

#![cfg(feature = "argos-network")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A running failover proxy.
///
/// Listens on `127.0.0.1:NNNNN` (random ephemeral port). Dropping the value
/// aborts the listener task and frees the port.
pub struct TcpFailoverProxy {
    /// Loopback URL the proxy is reachable at (`http://127.0.0.1:N`). Pass
    /// this to Argos's `lightwalletd_url` config.
    pub url: String,
    _handle: tokio::task::JoinHandle<()>,
}

/// Bind a failover proxy on a random loopback port. `upstreams` are the
/// targets, in the order they should be tried: connection `i` (1-indexed)
/// goes to `upstreams[min(i - 1, upstreams.len() - 1)]`.
///
/// Each upstream string must be the *host:port* portion only (no scheme).
/// Pass e.g. `"127.0.0.1:9067"`, not `"http://127.0.0.1:9067"`. The
/// FakeLightwalletd fixture's `.url` field is `http://127.0.0.1:N`; trim
/// the `http://` prefix before passing it in.
///
/// # Panics
/// Panics if `upstreams` is empty — a proxy with no targets has nothing to
/// forward to.
pub async fn serve_tcp_failover_proxy(
    upstreams: Vec<String>,
) -> std::io::Result<TcpFailoverProxy> {
    assert!(
        !upstreams.is_empty(),
        "TcpFailoverProxy requires at least one upstream"
    );

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr: SocketAddr = listener.local_addr()?;
    let url = format!("http://{local_addr}");

    let counter = Arc::new(AtomicUsize::new(0));
    let upstreams = Arc::new(upstreams);

    let handle = tokio::spawn(async move {
        loop {
            let Ok((client_stream, _peer)) = listener.accept().await else {
                return;
            };
            // 1-based connection index, capped at upstreams.len() - 1 so
            // every accept past `upstreams.len()` keeps going to the last
            // configured target — the "fully failed over" steady state.
            let idx = counter.fetch_add(1, Ordering::SeqCst);
            let target_idx = idx.min(upstreams.len() - 1);
            let target = upstreams[target_idx].clone();
            tokio::spawn(async move {
                forward(client_stream, &target).await;
            });
        }
    });

    Ok(TcpFailoverProxy {
        url,
        _handle: handle,
    })
}

/// Connect to `target`, then bidirectionally splice bytes between
/// `client_stream` and the upstream until either side closes. Errors are
/// swallowed silently: the proxy is a test fixture, and an upstream connect
/// failure or a mid-stream EOF is a *valid* outcome that the test asserts
/// against — it should not propagate as a panic.
async fn forward(client_stream: TcpStream, target: &str) {
    let Ok(upstream_stream) = TcpStream::connect(target).await else {
        return;
    };

    let (mut cr, mut cw) = tokio::io::split(client_stream);
    let (mut ur, mut uw) = tokio::io::split(upstream_stream);

    let to_upstream = async move {
        // 16 KiB matches tokio::io::copy's internal buffer size and is
        // comfortably larger than typical h2 frame payloads.
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if uw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = uw.shutdown().await;
    };

    let to_client = async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match ur.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = cw.shutdown().await;
    };

    // Each direction terminates the moment its side closes; we don't need
    // both to finish before returning.
    tokio::join!(to_upstream, to_client);
}
