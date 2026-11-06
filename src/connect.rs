//! Connection setup shared by every worker of a run.
//!
//! The target host is resolved and the TLS client configuration is built once,
//! before the run starts, so neither DNS lookups nor loading the platform's
//! root certificates are counted in handshake latency. Each handshake then
//! opens a TCP connection with [`connect_tcp`], which races the resolved
//! addresses so a host that resolves to an unreachable address family first
//! does not stall every connection. The address that answered is tried first
//! by later connections, so only the first few pay for the race.
//!
//! Each connection's read buffer is sized from the run's connection count
//! ([`read_buffer_size`]). The WebSocket library fills a read buffer's whole
//! capacity with zeros before every read, so a buffer is resident in full,
//! and each read of a small frame first clears the whole buffer. Its 128 KiB
//! default would take 1.3 GB at 10,000 connections and spends more CPU
//! clearing buffers than handling small messages. 16 KiB, one TLS record,
//! is as fast as larger buffers for large messages.

use crate::error::ErrorCategory;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use url::{Host, Url};

/// How long a connection attempt runs before the next address is tried in
/// parallel, as recommended by RFC 8305 (Happy Eyeballs v2).
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Memory the read buffers of all of a run's connections start with.
const READ_BUFFER_BUDGET: usize = 16 * 1024 * 1024;
/// The largest read buffer, used by runs of up to 1,024 connections and by
/// every connection of the mock server: one TLS record. Larger buffers read
/// large messages no faster and make every read of a small one slower.
pub(crate) const MAX_READ_BUFFER: usize = 16 * 1024;
/// The smallest read buffer: one page, enough for typical messages in one
/// read. A larger message grows the buffer as it arrives.
const MIN_READ_BUFFER: usize = 4 * 1024;

/// The initial read buffer of each connection of a run that opens up to
/// `connections` at once: an equal share of [`READ_BUFFER_BUDGET`], between
/// 4 KiB and 16 KiB, rounded down to a power of two. Sizes that are not a
/// whole number of pages waste the partly used pages around each buffer:
/// 4,000 connections with 4,194-byte buffers used 13 MB more than with
/// 4 KiB ones.
pub(crate) fn read_buffer_size(connections: usize) -> usize {
    let share = (READ_BUFFER_BUDGET / connections.max(1)).clamp(MIN_READ_BUFFER, MAX_READ_BUFFER);
    1 << share.ilog2()
}

pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Where and how every worker of one run connects.
pub(crate) struct ConnectTarget {
    /// The target's addresses with address families interleaved, or the
    /// category of the failure to resolve them.
    addrs: Result<Arc<[SocketAddr]>, ErrorCategory>,
    /// Index in `addrs` of the address that most recently accepted a
    /// connection; later connections try it first.
    preferred: AtomicUsize,
    connector: Connector,
    /// WebSocket settings of every connection: the library defaults with a
    /// read buffer sized for the run.
    ws_config: WebSocketConfig,
}

impl ConnectTarget {
    /// Resolves `url`, allowing it `timeout`, and prepares the TLS
    /// configuration for `wss://` targets, for a run that opens up to
    /// `connections` connections at once.
    ///
    /// A resolution failure is not returned as an error: it is kept and
    /// reported by every worker as a connection failure, the same way a
    /// per-connection failure would be.
    pub(crate) async fn prepare(url: &Url, timeout: Duration, connections: usize) -> Self {
        let connector = if url.scheme() == "wss" {
            rustls_connector()
        } else {
            Connector::Plain
        };
        let addrs = tokio::time::timeout(timeout, resolve(url))
            .await
            .unwrap_or(Err(ErrorCategory::Timeout));
        Self {
            addrs,
            preferred: AtomicUsize::new(0),
            connector,
            ws_config: WebSocketConfig::default().read_buffer_size(read_buffer_size(connections)),
        }
    }

    /// Opens the TCP connection and completes the TLS and WebSocket
    /// handshakes for `request`.
    pub(crate) async fn connect(&self, request: Request) -> Result<WsStream, ConnectError> {
        let addrs = self
            .addrs
            .as_ref()
            .map_err(|cat| ConnectError::Static(*cat))?;
        let preferred = self.preferred.load(Ordering::Relaxed).min(addrs.len() - 1);
        let order: Vec<SocketAddr> = std::iter::once(addrs[preferred])
            .chain(
                addrs
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != preferred)
                    .map(|(_, addr)| *addr),
            )
            .collect();
        let tcp = connect_tcp(&order)
            .await
            .map_err(|err| ConnectError::Tungstenite(Box::new(err.into())))?;
        if let Ok(peer) = tcp.peer_addr()
            && let Some(i) = addrs.iter().position(|addr| *addr == peer)
        {
            self.preferred.store(i, Ordering::Relaxed);
        }
        let (ws_stream, _) = tokio_tungstenite::client_async_tls_with_config(
            request,
            tcp,
            Some(self.ws_config),
            Some(self.connector.clone()),
        )
        .await
        .map_err(|err| ConnectError::Tungstenite(Box::new(err)))?;
        Ok(ws_stream)
    }
}

/// Why a connection could not be opened.
pub(crate) enum ConnectError {
    /// A failure that every retry would repeat, such as a host that did not
    /// resolve before the run started.
    Static(ErrorCategory),
    /// A failure of this attempt, boxed because `tungstenite::Error` is
    /// large and this is returned on every connection attempt.
    Tungstenite(Box<tokio_tungstenite::tungstenite::Error>),
}

async fn resolve(url: &Url) -> Result<Arc<[SocketAddr]>, ErrorCategory> {
    let port = url.port_or_known_default().ok_or(ErrorCategory::Other)?;
    let addrs: Vec<SocketAddr> = match url.host() {
        Some(Host::Ipv4(ip)) => vec![SocketAddr::from((ip, port))],
        Some(Host::Ipv6(ip)) => vec![SocketAddr::from((ip, port))],
        Some(Host::Domain(domain)) => tokio::net::lookup_host((domain, port))
            .await
            .map_err(|_| ErrorCategory::DnsResolution)?
            .collect(),
        None => return Err(ErrorCategory::Other),
    };
    if addrs.is_empty() {
        return Err(ErrorCategory::DnsResolution);
    }
    Ok(interleave_families(&addrs).into())
}

/// Builds a rustls client configuration that trusts the platform's root
/// certificates. `SSL_CERT_FILE` and `SSL_CERT_DIR` override the platform
/// store, as documented by `rustls-native-certs`.
pub(crate) fn rustls_client_config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// The TLS connector every `wss://` handshake of a run shares.
fn rustls_connector() -> Connector {
    Connector::Rustls(Arc::new(rustls_client_config()))
}

/// Connects to the first address that accepts, trying them in the given
/// order. Each attempt gets
/// [`ATTEMPT_DELAY`] before the next one starts alongside it; a failed attempt
/// starts the next one at once. Returns the last error when every address
/// fails.
pub(crate) async fn connect_tcp(addrs: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut queue = addrs.iter().copied();
    let mut pending = FuturesUnordered::new();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no address to connect to");

    match queue.next() {
        Some(addr) => pending.push(TcpStream::connect(addr)),
        None => return Err(last_err),
    }

    loop {
        tokio::select! {
            Some(result) = pending.next() => match result {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    last_err = err;
                    match queue.next() {
                        Some(addr) => pending.push(TcpStream::connect(addr)),
                        None if pending.is_empty() => return Err(last_err),
                        None => {}
                    }
                }
            },
            () = tokio::time::sleep(ATTEMPT_DELAY), if queue.len() > 0 => {
                if let Some(addr) = queue.next() {
                    pending.push(TcpStream::connect(addr));
                }
            }
        }
    }
}

/// Reorders `addrs` so address families alternate, starting with the family
/// of the first address, and otherwise keeps the resolver's order.
fn interleave_families(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else {
        return Vec::new();
    };
    let (primary, secondary): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs
        .iter()
        .copied()
        .partition(|addr| addr.is_ipv6() == first.is_ipv6());
    let mut ordered = Vec::with_capacity(addrs.len());
    let (mut primary, mut secondary) = (primary.into_iter(), secondary.into_iter());
    loop {
        match (primary.next(), secondary.next()) {
            (None, None) => return ordered,
            (a, b) => ordered.extend(a.into_iter().chain(b)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn read_buffers_share_a_fixed_budget_within_bounds() {
        assert_eq!(read_buffer_size(0), MAX_READ_BUFFER);
        assert_eq!(read_buffer_size(50), MAX_READ_BUFFER);
        assert_eq!(read_buffer_size(1024), MAX_READ_BUFFER);
        assert_eq!(read_buffer_size(1025), 8 * 1024);
        assert_eq!(read_buffer_size(2048), 8 * 1024);
        assert_eq!(read_buffer_size(3000), MIN_READ_BUFFER);
        assert_eq!(read_buffer_size(4096), MIN_READ_BUFFER);
        assert_eq!(read_buffer_size(100_000), MIN_READ_BUFFER);
    }

    #[test]
    fn families_alternate_starting_with_the_first() {
        let input = [
            addr("[::1]:80"),
            addr("[::2]:80"),
            addr("127.0.0.1:80"),
            addr("[::3]:80"),
            addr("127.0.0.2:80"),
        ];
        assert_eq!(
            interleave_families(&input),
            [
                addr("[::1]:80"),
                addr("127.0.0.1:80"),
                addr("[::2]:80"),
                addr("127.0.0.2:80"),
                addr("[::3]:80"),
            ]
        );
        assert!(interleave_families(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_refused_address_falls_through_to_the_next() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let closed = {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let stream = connect_tcp(&[closed, open]).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
    }

    #[tokio::test]
    async fn a_stalled_address_does_not_block_the_next() {
        // 192.0.2.0/24 is reserved for documentation and never answers, so
        // the first attempt stalls and the second starts after the delay.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            connect_tcp(&[addr("192.0.2.1:9"), open]),
        )
        .await
        .expect("the reachable address was not tried")
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), open);
    }

    #[tokio::test]
    async fn the_address_that_answered_is_tried_first_next_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let target = ConnectTarget {
            addrs: Ok(vec![addr("192.0.2.1:9"), open].into()),
            preferred: AtomicUsize::new(0),
            connector: Connector::Plain,
            ws_config: WebSocketConfig::default(),
        };
        let request = "ws://127.0.0.1/".into_client_request().unwrap();
        // The server never upgrades, so only the TCP step can be observed.
        let _ = tokio::time::timeout(Duration::from_millis(600), target.connect(request)).await;
        assert_eq!(target.preferred.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn every_address_failing_returns_the_last_error() {
        let closed = {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            probe.local_addr().unwrap()
        };
        let err = connect_tcp(&[closed]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn ip_literals_skip_dns_and_bad_hosts_report_dns() {
        let url = Url::parse("ws://[::1]:9001/ws").unwrap();
        assert_eq!(&*resolve(&url).await.unwrap(), [addr("[::1]:9001")]);
        let url = Url::parse("wss://127.0.0.1/").unwrap();
        assert_eq!(&*resolve(&url).await.unwrap(), [addr("127.0.0.1:443")]);
        let url = Url::parse("ws://does-not-exist.invalid/").unwrap();
        assert_eq!(
            resolve(&url).await.unwrap_err(),
            ErrorCategory::DnsResolution
        );
    }
}
