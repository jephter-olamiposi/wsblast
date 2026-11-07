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
//! With source addresses (`--source-ip`), each connection binds the next
//! source of the target address's family in turn, so every source has its
//! own range of ephemeral ports and a run can open more connections to one
//! target than one address allows.
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
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
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

pub(crate) type WsStream = WebSocketStream<Transport>;

/// A client connection's bytes: TCP or TLS, under the permessage-deflate
/// layer when the run uses `--compression`. The plain stream stays inline,
/// so uncompressed connections pay no allocation or indirection for it; the
/// layer, much larger, is boxed.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Transport {
    Plain(MaybeTlsStream<TcpStream>),
    #[cfg(feature = "compression")]
    Deflate(Box<crate::deflate::DeflateStream<MaybeTlsStream<TcpStream>>>),
}

macro_rules! each_transport {
    ($self:ident, $inner:ident => $body:expr) => {
        match $self.get_mut() {
            Transport::Plain($inner) => {
                let $inner = Pin::new($inner);
                $body
            }
            #[cfg(feature = "compression")]
            Transport::Deflate($inner) => {
                let $inner = Pin::new($inner.as_mut());
                $body
            }
        }
    };
}

impl AsyncRead for Transport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        each_transport!(self, inner => inner.poll_read(cx, buf))
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        each_transport!(self, inner => inner.poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_transport!(self, inner => inner.poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        each_transport!(self, inner => inner.poll_shutdown(cx))
    }
}

/// Where and how every worker of one run connects.
pub(crate) struct ConnectTarget {
    /// The target's addresses with address families interleaved, or the
    /// category of the failure to resolve them.
    addrs: Result<Arc<[SocketAddr]>, ErrorCategory>,
    /// Index in `addrs` of the address that most recently accepted a
    /// connection; later connections try it first.
    preferred: AtomicUsize,
    /// The TLS client configuration of a `wss://` target.
    tls: Option<Arc<rustls::ClientConfig>>,
    /// WebSocket settings of every connection: the library defaults with a
    /// read buffer sized for the run.
    ws_config: WebSocketConfig,
    /// The local addresses connections bind, if any.
    sources: SourceIps,
    /// What `--compression` counted, when the run offers it.
    #[cfg(feature = "compression")]
    compression: Option<Arc<crate::deflate::CompressionCounters>>,
}

/// The local addresses connections bind, by family, taken in turn.
#[derive(Default)]
struct SourceIps {
    v4: Box<[IpAddr]>,
    v6: Box<[IpAddr]>,
    /// Connections that took a source so far.
    next: AtomicUsize,
}

impl SourceIps {
    fn new(ips: &[IpAddr]) -> Self {
        let (v4, v6): (Vec<IpAddr>, Vec<IpAddr>) = ips.iter().partition(|ip| ip.is_ipv4());
        Self {
            v4: v4.into(),
            v6: v6.into(),
            next: AtomicUsize::new(0),
        }
    }

    fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    /// Whether a connection to `addr` can bind one of the sources.
    fn serves(&self, addr: &SocketAddr) -> bool {
        self.is_empty() || !self.of_family(addr).is_empty()
    }

    fn of_family(&self, addr: &SocketAddr) -> &[IpAddr] {
        if addr.is_ipv4() { &self.v4 } else { &self.v6 }
    }

    /// The source connection `turn` binds for `addr`; `None` without
    /// sources.
    fn for_turn(&self, addr: &SocketAddr, turn: usize) -> Option<IpAddr> {
        let family = self.of_family(addr);
        (!family.is_empty()).then(|| family[turn % family.len()])
    }
}

impl ConnectTarget {
    /// Resolves `url`, allowing it `timeout`, and prepares the TLS
    /// configuration for `wss://` targets, for a run that opens up to
    /// `connections` connections at once.
    ///
    /// A resolution failure is not returned as an error: it is kept and
    /// reported by every worker as a connection failure, the same way a
    /// per-connection failure would be.
    ///
    /// With `source_ips`, only the target's addresses of a family that has
    /// a source are used; when none has, every connection fails with
    /// `tcp_connect`, and a warning says why.
    pub(crate) async fn prepare(
        url: &Url,
        timeout: Duration,
        connections: usize,
        source_ips: &[IpAddr],
        compression: bool,
    ) -> Self {
        let tls = (url.scheme() == "wss").then(|| Arc::new(rustls_client_config()));
        #[cfg(not(feature = "compression"))]
        let _ = compression;
        let sources = SourceIps::new(source_ips);
        let addrs = tokio::time::timeout(timeout, resolve(url))
            .await
            .unwrap_or(Err(ErrorCategory::Timeout))
            .and_then(|addrs| {
                let usable: Vec<SocketAddr> =
                    addrs.iter().copied().filter(|a| sources.serves(a)).collect();
                if usable.is_empty() {
                    let family = if addrs.iter().all(SocketAddr::is_ipv6) { "IPv6" } else { "IPv4" };
                    eprintln!(
                        "{} {} has only {family} addresses and no --source-ip is {family}, so every connection fails.",
                        colored::Colorize::yellow(colored::Colorize::bold("Warning:")),
                        url.host_str().unwrap_or_default()
                    );
                    return Err(ErrorCategory::TcpConnect);
                }
                Ok(usable.into())
            });
        Self {
            addrs,
            preferred: AtomicUsize::new(0),
            tls,
            ws_config: WebSocketConfig::default().read_buffer_size(read_buffer_size(connections)),
            sources,
            #[cfg(feature = "compression")]
            compression: compression.then(Arc::default),
        }
    }

    /// What `--compression` counted so far, when the run offers it.
    #[cfg(feature = "compression")]
    pub(crate) fn compression_counts(&self) -> Option<crate::metrics::CompressionCounts> {
        self.compression
            .as_ref()
            .map(|counters| counters.snapshot())
    }

    /// Opens the TCP connection and completes the TLS and WebSocket
    /// handshakes for `request`.
    pub(crate) async fn connect(&self, request: Request) -> Result<WsStream, ConnectError> {
        let addrs = self
            .addrs
            .as_ref()
            .map_err(|cat| ConnectError::Static(*cat))?;
        let preferred = self.preferred.load(Ordering::Relaxed).min(addrs.len() - 1);
        let turn = if self.sources.is_empty() {
            0
        } else {
            self.sources.next.fetch_add(1, Ordering::Relaxed)
        };
        let order: Vec<(SocketAddr, Option<IpAddr>)> = std::iter::once(addrs[preferred])
            .chain(
                addrs
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != preferred)
                    .map(|(_, addr)| *addr),
            )
            .map(|addr| (addr, self.sources.for_turn(&addr, turn)))
            .collect();
        let tcp = connect_tcp(&order)
            .await
            .map_err(|err| ConnectError::Tungstenite(Box::new(err.into())))?;
        if let Ok(peer) = tcp.peer_addr()
            && let Some(i) = addrs.iter().position(|addr| *addr == peer)
        {
            self.preferred.store(i, Ordering::Relaxed);
        }
        let stream = match &self.tls {
            None => MaybeTlsStream::Plain(tcp),
            Some(config) => {
                // As tokio-tungstenite names the server: the request's host,
                // an IPv6 literal without its brackets.
                let host = request.uri().host().unwrap_or_default();
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let name =
                    rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
                        ConnectError::Tungstenite(Box::new(
                            tokio_tungstenite::tungstenite::error::TlsError::InvalidDnsName.into(),
                        ))
                    })?;
                let tls = tokio_rustls::TlsConnector::from(Arc::clone(config))
                    .connect(name, tcp)
                    .await
                    .map_err(|err| ConnectError::Tungstenite(Box::new(err.into())))?;
                MaybeTlsStream::Rustls(tls)
            }
        };
        #[cfg(feature = "compression")]
        let transport = match &self.compression {
            Some(counters) => Transport::Deflate(Box::new(crate::deflate::DeflateStream::new(
                stream,
                crate::deflate::Role::Client,
                Arc::clone(counters),
            ))),
            None => Transport::Plain(stream),
        };
        #[cfg(not(feature = "compression"))]
        let transport = Transport::Plain(stream);
        let (ws_stream, _) =
            tokio_tungstenite::client_async_with_config(request, transport, Some(self.ws_config))
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

/// Connects to the first address that accepts, trying them in the given
/// order, each from its source address when it has one. Each attempt gets
/// [`ATTEMPT_DELAY`] before the next one starts alongside it; a failed attempt
/// starts the next one at once. Returns the last error when every address
/// fails.
pub(crate) async fn connect_tcp(addrs: &[(SocketAddr, Option<IpAddr>)]) -> io::Result<TcpStream> {
    let mut queue = addrs.iter().copied();
    let mut pending = FuturesUnordered::new();
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no address to connect to");

    match queue.next() {
        Some(addr) => pending.push(open_tcp(addr)),
        None => return Err(last_err),
    }

    loop {
        tokio::select! {
            Some(result) = pending.next() => match result {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    last_err = err;
                    match queue.next() {
                        Some(addr) => pending.push(open_tcp(addr)),
                        None if pending.is_empty() => return Err(last_err),
                        None => {}
                    }
                }
            },
            () = tokio::time::sleep(ATTEMPT_DELAY), if queue.len() > 0 => {
                if let Some(addr) = queue.next() {
                    pending.push(open_tcp(addr));
                }
            }
        }
    }
}

/// Opens a TCP connection to `target.0`, bound to the source address
/// `target.1` when there is one. Binding port 0 lets the system pick a free
/// port of that address.
async fn open_tcp((addr, source): (SocketAddr, Option<IpAddr>)) -> io::Result<TcpStream> {
    let Some(ip) = source else {
        return TcpStream::connect(addr).await;
    };
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.bind(SocketAddr::new(ip, 0))?;
    socket.connect(addr).await
}

/// Checks that `ip` can be a source address: an address of this machine
/// that is not unspecified (`0.0.0.0` or `::`).
pub(crate) fn check_source_ip(ip: IpAddr) -> Result<(), String> {
    if ip.is_unspecified() {
        return Err(format!(
            "--source-ip {ip} is unspecified; give an address of this machine, such as 10.0.0.5"
        ));
    }
    std::net::UdpSocket::bind(SocketAddr::new(ip, 0))
        .map(drop)
        .map_err(|e| format!("--source-ip {ip} is not usable on this machine: {e}"))
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
        let stream = connect_tcp(&[(closed, None), (open, None)]).await.unwrap();
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
            connect_tcp(&[(addr("192.0.2.1:9"), None), (open, None)]),
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
            tls: None,
            ws_config: WebSocketConfig::default(),
            sources: SourceIps::default(),
            #[cfg(feature = "compression")]
            compression: None,
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
        let err = connect_tcp(&[(closed, None)]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test]
    async fn connections_bind_their_source_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let stream = connect_tcp(&[(open, Some(local))]).await.unwrap();
        assert_eq!(stream.local_addr().unwrap().ip(), local);
        let (accepted, peer) = listener.accept().await.unwrap();
        drop(accepted);
        assert_eq!(peer.ip(), local);
    }

    /// Linux routes all of 127.0.0.0/8 to the loopback interface, so
    /// 127.0.0.2 is an address of every Linux machine.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn connections_alternate_between_source_addresses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!("ws://{}/", listener.local_addr().unwrap())).unwrap();
        let sources: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap(), "127.0.0.2".parse().unwrap()];
        let target = Arc::new(
            ConnectTarget::prepare(&url, Duration::from_secs(1), 4, &sources, false).await,
        );
        let mut peers = Vec::new();
        for _ in 0..4 {
            let target = Arc::clone(&target);
            // The listener never upgrades, so only the TCP step completes.
            let attempt = tokio::spawn(async move {
                let request = "ws://127.0.0.1/".into_client_request().unwrap();
                let _ = target.connect(request).await;
            });
            let (stream, peer) = listener.accept().await.unwrap();
            peers.push(peer.ip());
            drop(stream);
            let _ = attempt.await;
        }
        assert_eq!(peers, [sources[0], sources[1], sources[0], sources[1]]);
    }

    #[test]
    fn sources_are_taken_in_turn_within_the_target_family() {
        let ips: Vec<IpAddr> = ["10.0.0.1", "::1", "10.0.0.2"]
            .iter()
            .map(|ip| ip.parse().unwrap())
            .collect();
        let sources = SourceIps::new(&ips);
        let v4 = addr("192.0.2.1:80");
        let v6 = addr("[2001:db8::1]:80");
        let turns: Vec<Option<IpAddr>> = (0..3).map(|turn| sources.for_turn(&v4, turn)).collect();
        assert_eq!(turns, [Some(ips[0]), Some(ips[2]), Some(ips[0])]);
        assert_eq!(sources.for_turn(&v6, 5), Some(ips[1]));
        let only_v4 = SourceIps::new(&ips[..1]);
        assert!(only_v4.serves(&v4));
        assert!(!only_v4.serves(&v6));
        assert!(SourceIps::default().serves(&v6));
    }

    #[tokio::test]
    async fn a_target_without_a_source_of_its_family_fails_every_connection() {
        let url = Url::parse("ws://127.0.0.1:9/").unwrap();
        let target = ConnectTarget::prepare(
            &url,
            Duration::from_secs(1),
            1,
            &["::1".parse().unwrap()],
            false,
        )
        .await;
        assert_eq!(target.addrs.unwrap_err(), ErrorCategory::TcpConnect);
    }

    #[test]
    fn source_addresses_must_belong_to_this_machine() {
        assert!(check_source_ip("127.0.0.1".parse().unwrap()).is_ok());
        let unspecified = check_source_ip("0.0.0.0".parse().unwrap()).unwrap_err();
        assert!(unspecified.contains("unspecified"), "{unspecified}");
        // 192.0.2.0/24 is reserved for documentation, so no machine has it.
        let foreign = check_source_ip("192.0.2.77".parse().unwrap()).unwrap_err();
        assert!(
            foreign.contains("is not usable on this machine"),
            "{foreign}"
        );
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
