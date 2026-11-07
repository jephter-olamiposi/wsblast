//! permessage-deflate (RFC 7692) for `--compression` and `wsblast mock
//! --compression`, as a byte-stream layer under the WebSocket library.
//!
//! tungstenite rejects frames with the RSV1 bit set and knows no
//! extensions, so the layer sits between the (TLS) connection and
//! tungstenite and rewrites frames, leaving tungstenite to see plain
//! WebSocket traffic:
//!
//! - The HTTP handshake passes through unchanged. The layer reads the
//!   `Sec-WebSocket-Extensions` answer from the response (the inbound one
//!   for a client, the outbound one for a server) to learn whether
//!   compression was agreed and with which parameters. Without agreement
//!   the layer passes every byte through, so a server that declines costs
//!   nothing per frame.
//! - Inbound, a data message with RSV1 set is inflated, across its
//!   fragments, and handed on with RSV1 cleared and its new length. A
//!   masked frame (one a client sent) is re-masked with its own key.
//! - Outbound, every data message tungstenite writes (always as one frame)
//!   is deflated, flushed with a sync flush whose trailing `00 00 ff ff` is
//!   removed, and sent with RSV1 set. A client frame is unmasked, deflated
//!   and masked again with the key tungstenite chose.
//! - Control frames pass through untouched.
//!
//! The deflater always uses a 32 KiB window, so wsblast never offers a
//! smaller client window. A server that still asks for one gets
//! uncompressed messages, which RFC 7692 allows for any message.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The extension offer a client sends.
pub(crate) const OFFER: &str = "permessage-deflate";

/// The tail a sync flush ends with, removed from every compressed message.
const SYNC_TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// The largest message the layer inflates, as tungstenite's default
/// `max_message_size`, so a small compressed frame cannot expand into
/// unbounded memory.
const MAX_MESSAGE: usize = 64 << 20;

/// The largest compressed frame the layer buffers before rewriting it.
const MAX_FRAME: usize = 64 << 20;

/// The largest HTTP head the layer waits for.
const MAX_HEAD: usize = 64 << 10;

/// Unwritten output above which a write waits for the connection.
const HIGH_WATER: usize = 256 << 10;

/// Which end of the connection the layer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Client,
    /// `wsblast mock --compression`.
    #[cfg_attr(not(feature = "mock"), allow(dead_code))]
    Server,
}

/// What every compressed connection of a run counted, shared by its
/// connections.
#[derive(Debug, Default)]
pub(crate) struct CompressionCounters {
    /// Handshakes that offered compression.
    pub(crate) offered: AtomicU64,
    /// Handshakes whose server agreed.
    pub(crate) negotiated: AtomicU64,
    /// Data message bytes sent, before compression.
    pub(crate) bytes_sent: AtomicU64,
    /// Data message bytes as sent on the wire.
    pub(crate) wire_bytes_sent: AtomicU64,
    /// Data message bytes received, after decompression.
    pub(crate) bytes_recv: AtomicU64,
    /// Data message bytes as received on the wire.
    pub(crate) wire_bytes_recv: AtomicU64,
}

impl CompressionCounters {
    pub(crate) fn snapshot(&self) -> crate::metrics::CompressionCounts {
        crate::metrics::CompressionCounts {
            offered: self.offered.load(Ordering::Relaxed),
            negotiated: self.negotiated.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            wire_bytes_sent: self.wire_bytes_sent.load(Ordering::Relaxed),
            bytes_recv: self.bytes_recv.load(Ordering::Relaxed),
            wire_bytes_recv: self.wire_bytes_recv.load(Ordering::Relaxed),
        }
    }
}

/// The parameters an agreed extension carries (RFC 7692, section 7.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Params {
    pub(crate) server_no_context_takeover: bool,
    pub(crate) client_no_context_takeover: bool,
    pub(crate) server_max_window_bits: Option<u8>,
    pub(crate) client_max_window_bits: Option<u8>,
}

impl Params {
    /// The first `permessage-deflate` in a `Sec-WebSocket-Extensions`
    /// value, if any. A malformed one is treated as absent.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        value.split(',').find_map(|extension| {
            let mut parts = extension.split(';').map(str::trim);
            if !parts.next()?.eq_ignore_ascii_case(OFFER) {
                return None;
            }
            let mut params = Self::default();
            for part in parts.filter(|p| !p.is_empty()) {
                let (name, value) = match part.split_once('=') {
                    Some((name, value)) => (name.trim(), Some(value.trim().trim_matches('"'))),
                    None => (part, None),
                };
                let bits = || -> Option<Option<u8>> {
                    match value {
                        None => Some(None),
                        Some(v) => v
                            .parse::<u8>()
                            .ok()
                            .filter(|b| (8..=15).contains(b))
                            .map(Some),
                    }
                };
                match name.to_ascii_lowercase().as_str() {
                    "server_no_context_takeover" => params.server_no_context_takeover = true,
                    "client_no_context_takeover" => params.client_no_context_takeover = true,
                    "server_max_window_bits" => params.server_max_window_bits = bits()?,
                    "client_max_window_bits" => params.client_max_window_bits = bits()?,
                    _ => return None,
                }
            }
            Some(params)
        })
    }

    /// Whether this end's deflater may use its 32 KiB window.
    fn can_compress(&self, role: Role) -> bool {
        let bits = match role {
            Role::Client => self.client_max_window_bits,
            Role::Server => self.server_max_window_bits,
        };
        bits.is_none_or(|bits| bits == 15)
    }

    /// Whether this end resets its deflater after every message.
    fn reset_deflater(&self, role: Role) -> bool {
        match role {
            Role::Client => self.client_no_context_takeover,
            Role::Server => self.server_no_context_takeover,
        }
    }

    /// Whether the peer resets its deflater after every message.
    fn reset_inflater(&self, role: Role) -> bool {
        match role {
            Role::Client => self.server_no_context_takeover,
            Role::Server => self.client_no_context_takeover,
        }
    }
}

/// The answer `wsblast mock --compression` gives to a client's
/// `Sec-WebSocket-Extensions`: the extension when the client offered it,
/// with `server_no_context_takeover` when the client asked for it, as RFC
/// 7692 requires, and nothing when the client did not offer it or asked
/// the server for a smaller window than the mock's deflater uses.
#[cfg(feature = "mock")]
pub(crate) fn accept_offer(offer: &str) -> Option<String> {
    let params = Params::parse(offer)?;
    if !params.server_max_window_bits.is_none_or(|bits| bits == 15) {
        return None;
    }
    Some(if params.server_no_context_takeover {
        format!("{OFFER}; server_no_context_takeover")
    } else {
        OFFER.to_string()
    })
}

/// The value of `name` in an HTTP head, matched without case.
fn header<'a>(head: &'a str, name: &str) -> impl Iterator<Item = &'a str> {
    head.split("\r\n").skip(1).filter_map(move |line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The agreed parameters in a response head: `None` unless it is a 101
/// carrying `permessage-deflate`.
fn negotiated(response: &str) -> Option<Params> {
    let status = response.split("\r\n").next()?;
    if status.split_whitespace().nth(1) != Some("101") {
        return None;
    }
    header(response, "sec-websocket-extensions").find_map(Params::parse)
}

/// A frame header, as RFC 6455 section 5.2 lays it out.
#[derive(Debug, Clone, Copy)]
struct Header {
    fin: bool,
    rsv1: bool,
    /// The RSV2 and RSV3 bits, kept as they are.
    rsv23: u8,
    opcode: u8,
    mask: Option<[u8; 4]>,
    /// Header length in bytes.
    len: usize,
    payload: usize,
}

impl Header {
    /// Parses the header at the start of `buf`; `None` until it is complete.
    fn parse(buf: &[u8]) -> Option<Self> {
        let (&b0, rest) = buf.split_first()?;
        let &b1 = rest.first()?;
        let mut len = 2;
        let payload = match b1 & 0x7f {
            126 => {
                let bytes = buf.get(2..4)?;
                len += 2;
                u16::from_be_bytes([bytes[0], bytes[1]]) as usize
            }
            127 => {
                let bytes = buf.get(2..10)?;
                len += 8;
                u64::from_be_bytes(bytes.try_into().ok()?).min(usize::MAX as u64) as usize
            }
            n => n as usize,
        };
        let mask = if b1 & 0x80 != 0 {
            let key = buf.get(len..len + 4)?;
            len += 4;
            Some([key[0], key[1], key[2], key[3]])
        } else {
            None
        };
        Some(Self {
            fin: b0 & 0x80 != 0,
            rsv1: b0 & 0x40 != 0,
            rsv23: b0 & 0x30,
            opcode: b0 & 0x0f,
            mask,
            len,
            payload,
        })
    }

    /// Writes a header with this frame's flags, `rsv1` and `payload` bytes.
    fn write(&self, rsv1: bool, payload: usize, out: &mut Vec<u8>) {
        let fin = if self.fin { 0x80 } else { 0 };
        let rsv1 = if rsv1 { 0x40 } else { 0 };
        out.push(fin | rsv1 | self.rsv23 | self.opcode);
        let mask_bit = if self.mask.is_some() { 0x80 } else { 0 };
        match payload {
            0..=125 => out.push(mask_bit | payload as u8),
            126..=0xffff => {
                out.push(mask_bit | 126);
                out.extend_from_slice(&(payload as u16).to_be_bytes());
            }
            _ => {
                out.push(mask_bit | 127);
                out.extend_from_slice(&(payload as u64).to_be_bytes());
            }
        }
        if let Some(key) = self.mask {
            out.extend_from_slice(&key);
        }
    }
}

/// Applies (or removes) a frame mask.
fn apply_mask(data: &mut [u8], key: [u8; 4]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte ^= key[i & 3];
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// A stream with permessage-deflate applied to its WebSocket frames.
pub(crate) struct DeflateStream<S> {
    inner: S,
    role: Role,
    counters: Arc<CompressionCounters>,
    /// `None` until the response head has passed; then the agreed
    /// parameters, or `None` inside when the server declined.
    agreement: Option<Option<Params>>,
    /// Bytes read from `inner` and not yet rewritten.
    raw_in: Vec<u8>,
    /// Rewritten bytes not yet read by the caller, from `in_pos` on.
    ready_in: Vec<u8>,
    in_pos: usize,
    read_head_done: bool,
    /// Whether an inbound data message being received is compressed.
    inflating: bool,
    /// Its decompressed size so far.
    inflated: usize,
    inflater: Decompress,
    /// Bytes the caller wrote and not yet rewritten.
    raw_out: Vec<u8>,
    /// Rewritten bytes not yet written to `inner`, from `out_pos` on.
    ready_out: Vec<u8>,
    out_pos: usize,
    write_head_done: bool,
    /// Created on the first message it compresses, as it holds a few
    /// hundred KB of state.
    deflater: Option<Box<Compress>>,
    /// Scratch space for a frame's payload.
    scratch: Vec<u8>,
}

impl<S> DeflateStream<S> {
    pub(crate) fn new(inner: S, role: Role, counters: Arc<CompressionCounters>) -> Self {
        Self {
            inner,
            role,
            counters,
            agreement: None,
            raw_in: Vec::new(),
            ready_in: Vec::new(),
            in_pos: 0,
            read_head_done: false,
            inflating: false,
            inflated: 0,
            inflater: Decompress::new(false),
            raw_out: Vec::new(),
            ready_out: Vec::new(),
            out_pos: 0,
            write_head_done: false,
            deflater: None,
            scratch: Vec::new(),
        }
    }

    /// Whether compression was declined, so bytes pass through as they are.
    fn declined(&self) -> bool {
        self.agreement == Some(None)
    }

    /// Ends the HTTP head of one direction once its buffer holds all of it:
    /// moves the head on unchanged, and reads the agreement from it when it
    /// is the response. Returns whether the head ended.
    fn end_head(&mut self, inbound: bool) -> io::Result<bool> {
        let buf = if inbound { &self.raw_in } else { &self.raw_out };
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) else {
            if buf.len() > MAX_HEAD {
                return Err(invalid("the WebSocket handshake is too long"));
            }
            return Ok(false);
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let is_response = inbound == (self.role == Role::Client);
        if is_response {
            let params = negotiated(&head);
            if params.is_some() {
                self.counters.negotiated.fetch_add(1, Ordering::Relaxed);
            }
            self.agreement = Some(params);
        } else if self.role == Role::Client
            && header(&head, "sec-websocket-extensions").any(|v| Params::parse(v).is_some())
        {
            self.counters.offered.fetch_add(1, Ordering::Relaxed);
        }
        let (from, to) = if inbound {
            (&mut self.raw_in, &mut self.ready_in)
        } else {
            (&mut self.raw_out, &mut self.ready_out)
        };
        to.extend(from.drain(..end));
        Ok(true)
    }

    /// Rewrites the complete inbound frames in `raw_in` into `ready_in`.
    /// Frames wait while the agreement is unknown: a server learns it only
    /// from its own response.
    fn process_in(&mut self) -> io::Result<()> {
        if !self.read_head_done {
            if !self.end_head(true)? {
                return Ok(());
            }
            self.read_head_done = true;
        }
        let Some(agreement) = self.agreement else {
            return Ok(());
        };
        let Some(params) = agreement else {
            self.ready_in.append(&mut self.raw_in);
            return Ok(());
        };
        let mut consumed = 0;
        while let Some(header) = Header::parse(&self.raw_in[consumed..]) {
            if header.payload > MAX_FRAME {
                return Err(invalid("a compressed WebSocket frame is too large"));
            }
            let total = header.len + header.payload;
            if self.raw_in.len() - consumed < total {
                break;
            }
            let start = consumed + header.len;
            let end = consumed + total;
            let data = header.opcode < 8;
            if data && header.opcode != 0 {
                // The first frame of a message says whether it is compressed.
                self.inflating = header.rsv1;
                self.inflated = 0;
            }
            if data && self.inflating {
                self.scratch.clear();
                self.scratch.extend_from_slice(&self.raw_in[start..end]);
                if let Some(key) = header.mask {
                    apply_mask(&mut self.scratch, key);
                }
                let mut payload = Vec::new();
                self.inflate(header.fin, &mut payload)?;
                self.counters
                    .wire_bytes_recv
                    .fetch_add(header.payload as u64, Ordering::Relaxed);
                self.counters
                    .bytes_recv
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                if let Some(key) = header.mask {
                    apply_mask(&mut payload, key);
                }
                header.write(false, payload.len(), &mut self.ready_in);
                self.ready_in.extend_from_slice(&payload);
                if header.fin {
                    self.inflating = false;
                    if params.reset_inflater(self.role) {
                        self.inflater.reset(false);
                    }
                }
            } else {
                if data {
                    self.counters
                        .wire_bytes_recv
                        .fetch_add(header.payload as u64, Ordering::Relaxed);
                    self.counters
                        .bytes_recv
                        .fetch_add(header.payload as u64, Ordering::Relaxed);
                }
                self.ready_in.extend_from_slice(&self.raw_in[consumed..end]);
            }
            consumed = end;
        }
        self.raw_in.drain(..consumed);
        Ok(())
    }

    /// Inflates `scratch` into `out`, with the sync tail at the end of a
    /// message.
    fn inflate(&mut self, last: bool, out: &mut Vec<u8>) -> io::Result<()> {
        if last {
            self.scratch.extend_from_slice(&SYNC_TAIL);
        }
        let input = std::mem::take(&mut self.scratch);
        let start_in = self.inflater.total_in();
        loop {
            let consumed = (self.inflater.total_in() - start_in) as usize;
            if out.capacity() - out.len() < 4096 {
                out.reserve((input.len() * 4).clamp(16 << 10, 1 << 20));
            }
            let (before_in, before_out) = (self.inflater.total_in(), self.inflater.total_out());
            let status = self
                .inflater
                .decompress_vec(&input[consumed..], out, FlushDecompress::Sync)
                .map_err(|_| invalid("a compressed WebSocket message does not inflate"))?;
            self.inflated += (self.inflater.total_out() - before_out) as usize;
            if self.inflated > MAX_MESSAGE {
                return Err(invalid(
                    "a compressed WebSocket message inflates past 64 MiB",
                ));
            }
            let done = (self.inflater.total_in() - start_in) as usize == input.len();
            let progress =
                self.inflater.total_in() != before_in || self.inflater.total_out() != before_out;
            if status == Status::StreamEnd || (done && out.len() < out.capacity()) || !progress {
                break;
            }
        }
        self.scratch = input;
        self.scratch.clear();
        Ok(())
    }

    /// Rewrites the complete outbound frames in `raw_out` into `ready_out`.
    fn process_out(&mut self) -> io::Result<()> {
        if !self.write_head_done {
            if !self.end_head(false)? {
                return Ok(());
            }
            self.write_head_done = true;
        }
        let Some(agreement) = self.agreement else {
            return Ok(());
        };
        let Some(params) = agreement else {
            self.ready_out.append(&mut self.raw_out);
            return Ok(());
        };
        let compress = params.can_compress(self.role);
        let mut consumed = 0;
        while let Some(header) = Header::parse(&self.raw_out[consumed..]) {
            let total = header.len + header.payload;
            if self.raw_out.len() - consumed < total {
                break;
            }
            let start = consumed + header.len;
            let end = consumed + total;
            // tungstenite writes every message as one final frame; anything
            // else passes through as it is.
            let message = header.opcode == 1 || header.opcode == 2;
            if compress && message && header.fin && !header.rsv1 {
                self.scratch.clear();
                self.scratch.extend_from_slice(&self.raw_out[start..end]);
                if let Some(key) = header.mask {
                    apply_mask(&mut self.scratch, key);
                }
                let mut payload = self.deflate()?;
                self.counters
                    .bytes_sent
                    .fetch_add(header.payload as u64, Ordering::Relaxed);
                self.counters
                    .wire_bytes_sent
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                if let Some(key) = header.mask {
                    apply_mask(&mut payload, key);
                }
                header.write(true, payload.len(), &mut self.ready_out);
                self.ready_out.extend_from_slice(&payload);
                if params.reset_deflater(self.role)
                    && let Some(deflater) = self.deflater.as_mut()
                {
                    deflater.reset();
                }
            } else {
                if message {
                    self.counters
                        .bytes_sent
                        .fetch_add(header.payload as u64, Ordering::Relaxed);
                    self.counters
                        .wire_bytes_sent
                        .fetch_add(header.payload as u64, Ordering::Relaxed);
                }
                self.ready_out
                    .extend_from_slice(&self.raw_out[consumed..end]);
            }
            consumed = end;
        }
        self.raw_out.drain(..consumed);
        Ok(())
    }

    /// Deflates `scratch` with a sync flush, without the sync tail.
    fn deflate(&mut self) -> io::Result<Vec<u8>> {
        let deflater = self
            .deflater
            .get_or_insert_with(|| Box::new(Compress::new(Compression::fast(), false)));
        let input = &self.scratch;
        let mut out = Vec::with_capacity(input.len() / 2 + 64);
        let start_in = deflater.total_in();
        loop {
            let consumed = (deflater.total_in() - start_in) as usize;
            if out.capacity() - out.len() < 64 {
                out.reserve(input.len().max(1024));
            }
            let (before_in, before_out) = (deflater.total_in(), deflater.total_out());
            deflater
                .compress_vec(&input[consumed..], &mut out, FlushCompress::Sync)
                .map_err(|_| invalid("a WebSocket message does not deflate"))?;
            let done = (deflater.total_in() - start_in) as usize == input.len();
            let progress = deflater.total_in() != before_in || deflater.total_out() != before_out;
            if (done && out.len() < out.capacity()) || !progress {
                break;
            }
        }
        if out.ends_with(&SYNC_TAIL) {
            out.truncate(out.len() - SYNC_TAIL.len());
        }
        Ok(out)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> DeflateStream<S> {
    /// Writes as much of `ready_out` to `inner` as it takes.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.out_pos < self.ready_out.len() {
            let n =
                ready!(Pin::new(&mut self.inner).poll_write(cx, &self.ready_out[self.out_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.out_pos += n;
        }
        self.ready_out.clear();
        self.out_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for DeflateStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if this.in_pos < this.ready_in.len() {
                let n = buf.remaining().min(this.ready_in.len() - this.in_pos);
                buf.put_slice(&this.ready_in[this.in_pos..this.in_pos + n]);
                this.in_pos += n;
                if this.in_pos == this.ready_in.len() {
                    this.ready_in.clear();
                    this.in_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            // Frames that waited for the agreement can go now.
            if !this.raw_in.is_empty() {
                this.process_in()?;
                if !this.ready_in.is_empty() {
                    continue;
                }
            }
            // Declined: read straight into the caller's buffer.
            if this.declined() && this.raw_in.is_empty() {
                return Pin::new(&mut this.inner).poll_read(cx, buf);
            }
            let mut chunk = [0u8; 16 << 10];
            let mut read = ReadBuf::new(&mut chunk);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut read))?;
            if read.filled().is_empty() {
                return Poll::Ready(Ok(()));
            }
            this.raw_in.extend_from_slice(read.filled());
            this.process_in()?;
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for DeflateStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if this.ready_out.len() - this.out_pos > HIGH_WATER {
            ready!(this.poll_drain(cx))?;
        }
        // Declined, with nothing queued: write straight through.
        if this.declined() && this.raw_out.is_empty() && this.out_pos == this.ready_out.len() {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        this.raw_out.extend_from_slice(buf);
        this.process_out()?;
        // Start sending; whatever does not fit goes with the next write or
        // the flush.
        if let Poll::Ready(Err(err)) = this.poll_drain(cx) {
            return Poll::Ready(Err(err));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // A server's frames wait for its own response; a flush pushes on
        // whatever has been rewritten.
        this.process_out()?;
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_and_answers_are_parsed() {
        assert_eq!(Params::parse("permessage-deflate"), Some(Params::default()));
        let p = Params::parse(
            "x-webkit-deflate-frame, permessage-deflate; client_no_context_takeover; server_max_window_bits=10",
        )
        .unwrap();
        assert!(p.client_no_context_takeover && !p.server_no_context_takeover);
        assert_eq!(p.server_max_window_bits, Some(10));
        assert!(p.can_compress(Role::Client));
        assert!(!p.can_compress(Role::Server));
        assert!(Params::parse("permessage-deflate; client_max_window_bits=16").is_none());
        assert!(Params::parse("permessage-deflate; bogus").is_none());
        assert!(Params::parse("x-other").is_none());
        let response = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nSec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover\r\n\r\n";
        assert!(negotiated(response).unwrap().server_no_context_takeover);
        assert!(
            negotiated("HTTP/1.1 400 Bad\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n")
                .is_none()
        );
    }

    #[cfg(feature = "mock")]
    #[test]
    fn the_mock_answers_offers() {
        assert_eq!(accept_offer("permessage-deflate").as_deref(), Some(OFFER));
        assert_eq!(
            accept_offer("permessage-deflate; client_max_window_bits").as_deref(),
            Some(OFFER)
        );
        assert_eq!(
            accept_offer("permessage-deflate; server_no_context_takeover").as_deref(),
            Some("permessage-deflate; server_no_context_takeover")
        );
        assert_eq!(
            accept_offer("permessage-deflate; server_max_window_bits=10"),
            None
        );
        assert_eq!(accept_offer("x-webkit-deflate-frame"), None);
    }

    #[test]
    fn headers_round_trip() {
        for (payload, mask) in [(5usize, None), (300, Some([1, 2, 3, 4])), (70_000, None)] {
            let header = Header {
                fin: true,
                rsv1: false,
                rsv23: 0,
                opcode: 2,
                mask,
                len: 0,
                payload: 0,
            };
            let mut out = Vec::new();
            header.write(true, payload, &mut out);
            let parsed = Header::parse(&out).unwrap();
            assert_eq!(
                (parsed.payload, parsed.mask, parsed.rsv1, parsed.len),
                (payload, mask, true, out.len())
            );
        }
        assert!(Header::parse(&[0x82]).is_none());
        assert!(Header::parse(&[0x82, 126, 1]).is_none());
    }

    /// RFC 7692 section 7.2.3.1: "Hello" compressed without context
    /// takeover is `f2 48 cd c9 c9 07 00`.
    #[test]
    fn the_rfc_example_inflates() {
        let mut stream = DeflateStream::new((), Role::Client, Arc::default());
        stream
            .scratch
            .extend_from_slice(&[0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00]);
        let mut out = Vec::new();
        stream.inflate(true, &mut out).unwrap();
        assert_eq!(out, b"Hello");
    }

    #[test]
    fn deflated_messages_inflate_with_context_takeover() {
        let mut sender = DeflateStream::new((), Role::Client, Arc::default());
        let mut receiver = DeflateStream::new((), Role::Server, Arc::default());
        for text in [
            "Hello",
            "Hello",
            "",
            "a longer message, a longer message, a longer message",
        ] {
            sender.scratch.clear();
            sender.scratch.extend_from_slice(text.as_bytes());
            let compressed = sender.deflate().unwrap();
            receiver.scratch.clear();
            receiver.scratch.extend_from_slice(&compressed);
            let mut out = Vec::new();
            receiver.inflate(true, &mut out).unwrap();
            assert_eq!(out, text.as_bytes());
        }
    }

    #[test]
    fn a_decompression_bomb_is_refused() {
        let mut sender = DeflateStream::new((), Role::Server, Arc::default());
        sender.scratch = vec![0u8; MAX_MESSAGE + 1];
        let compressed = sender.deflate().unwrap();
        assert!(compressed.len() < MAX_MESSAGE / 100);
        let mut receiver = DeflateStream::new((), Role::Client, Arc::default());
        receiver.scratch = compressed;
        let err = receiver.inflate(true, &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("64 MiB"), "{err}");
    }
}
