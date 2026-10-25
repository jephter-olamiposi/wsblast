//! `--capture`: a JSONL record of every data frame and connection event.
//!
//! Workers never touch the file. Each session wraps its socket in a
//! [`CaptureStream`] that copies frames into records (payloads are
//! reference-counted, so this does not copy their bytes) and hands them to
//! one writer thread over a bounded channel with `try_send`. The writer
//! formats the lines and owns the buffered file. When the channel is full or
//! the record limit is reached the record is dropped and counted, so a slow
//! disk can never stall a worker. A run without `--capture` creates none of
//! this. See METHODOLOGY.md, section 11.

use crate::metrics::CaptureSummary;
use futures_util::{Sink, Stream};
use std::borrow::Cow;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::protocol::frame::{CloseFrame, Utf8Bytes};

/// Version written in the header line's `wsblast_capture` field.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// Records the writer may lag behind before new ones are dropped.
const QUEUE_CAPACITY: usize = 64 * 1024;

/// How long the writer sleeps when it finds the queue empty. The queue
/// holds this interval's records up to 32 million records per second, so
/// the sleep itself does not cause drops; formatting speed is the limit.
const WRITER_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Default for `--capture-limit`.
pub(crate) const DEFAULT_LIMIT: u64 = 100_000;

/// Where to capture and how many records to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CaptureConfig {
    pub(crate) path: PathBuf,
    /// Maximum records to write; 0 means unlimited.
    pub(crate) limit: u64,
}

/// Direction of a recorded frame, from the client's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Sent,
    Recv,
}

impl Dir {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Recv => "recv",
        }
    }
}

/// What a record holds. Payloads share the frame's buffer.
#[derive(Debug)]
enum Event {
    Open,
    Text(Utf8Bytes),
    Binary(bytes::Bytes),
    Close(Option<CloseFrame>),
}

#[derive(Debug)]
struct Record {
    t_us: u64,
    worker: usize,
    dir: Dir,
    event: Event,
}

enum Command {
    Record(Record),
    /// Sent once by [`RunningCapture::finish`]; the writer stops reading.
    Finish,
}

/// The capture file, opened before the run so a bad path fails early.
#[derive(Debug)]
pub(crate) struct CaptureFile {
    file: File,
    path: PathBuf,
    limit: u64,
    target: String,
}

impl CaptureFile {
    /// Creates (or truncates) the capture file.
    pub(crate) fn create(config: &CaptureConfig, target: &str) -> std::io::Result<Self> {
        let file = File::create(&config.path).map_err(|err| {
            std::io::Error::new(
                err.kind(),
                format!(
                    "cannot create capture file '{}': {err}",
                    config.path.display()
                ),
            )
        })?;
        Ok(Self {
            file,
            path: config.path.clone(),
            limit: config.limit,
            target: target.to_string(),
        })
    }

    /// Starts the writer thread. Record timestamps count from `start`.
    pub(crate) fn start(self, start: Instant) -> std::io::Result<RunningCapture> {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let header = serde_json::json!({
            "wsblast_capture": FORMAT_VERSION,
            "target": self.target,
            "started": crate::clock::rfc3339_utc(SystemTime::now()),
        });
        let file = self.file;
        let writer = std::thread::Builder::new()
            .name("wsblast-capture".into())
            .spawn(move || write_records(file, &header, &receiver))?;
        Ok(RunningCapture {
            handle: CaptureHandle(Arc::new(Shared {
                sender,
                start,
                limit: if self.limit == 0 {
                    u64::MAX
                } else {
                    self.limit
                },
                claimed: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            })),
            writer,
            path: self.path,
            limit: self.limit,
        })
    }
}

/// A capture in progress: hand out [`CaptureHandle`]s, then
/// [`finish`](Self::finish) once every worker has stopped.
#[derive(Debug)]
pub(crate) struct RunningCapture {
    handle: CaptureHandle,
    writer: std::thread::JoinHandle<WriterOutcome>,
    path: PathBuf,
    limit: u64,
}

impl RunningCapture {
    pub(crate) fn handle(&self) -> CaptureHandle {
        self.handle.clone()
    }

    /// Writes out every queued record, flushes and closes the file, and
    /// returns the totals. A write error is reported on stderr; the records
    /// it lost are counted as dropped.
    pub(crate) async fn finish(self) -> CaptureSummary {
        let Self {
            handle,
            writer,
            path,
            limit,
        } = self;
        let outcome = tokio::task::spawn_blocking(move || {
            // Blocks only while the writer drains a full queue. The writer
            // stops at this marker even if a stray handle is still alive.
            let _ = handle.0.sender.send(Command::Finish);
            let outcome = writer.join().unwrap_or_else(|_| WriterOutcome {
                written: 0,
                lost: 0,
                error: Some(std::io::Error::other("capture writer thread panicked")),
            });
            (outcome, handle.0.dropped.load(Ordering::Relaxed))
        })
        .await;
        let (outcome, dropped) = outcome.unwrap_or_else(|err| {
            (
                WriterOutcome {
                    written: 0,
                    lost: 0,
                    error: Some(std::io::Error::other(err.to_string())),
                },
                0,
            )
        });
        if let Some(err) = &outcome.error {
            eprintln!(
                "{} writing capture file '{}' failed after {} records: {err}",
                colored::Colorize::yellow(colored::Colorize::bold("Warning:")),
                path.display(),
                outcome.written
            );
        }
        CaptureSummary {
            path: path.display().to_string(),
            frames_written: outcome.written,
            frames_dropped: dropped + outcome.lost,
            limit,
        }
    }
}

#[derive(Debug)]
struct Shared {
    sender: SyncSender<Command>,
    start: Instant,
    /// Effective limit; `u64::MAX` when unlimited.
    limit: u64,
    /// Records queued so far, counted only when a limit is set.
    claimed: AtomicU64,
    dropped: AtomicU64,
}

/// A cloneable handle workers record through.
#[derive(Debug, Clone)]
pub(crate) struct CaptureHandle(Arc<Shared>);

impl CaptureHandle {
    /// Queues a record, or counts it in `dropped` when it cannot be queued.
    fn record(&self, worker: usize, dir: Dir, event: Event, dropped: &mut u64) {
        let shared = &*self.0;
        if shared.limit != u64::MAX {
            // Over the limit, a load is enough; under it, claim a slot so
            // racing workers never queue more than the limit.
            if shared.claimed.load(Ordering::Relaxed) >= shared.limit
                || shared.claimed.fetch_add(1, Ordering::Relaxed) >= shared.limit
            {
                *dropped += 1;
                return;
            }
        }
        let record = Record {
            t_us: u64::try_from(shared.start.elapsed().as_micros()).unwrap_or(u64::MAX),
            worker,
            dir,
            event,
        };
        match shared.sender.try_send(Command::Record(record)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                if shared.limit != u64::MAX {
                    shared.claimed.fetch_sub(1, Ordering::Relaxed);
                }
                *dropped += 1;
            }
        }
    }
}

/// A WebSocket stream that records every text, binary and Close frame
/// passing through it, plus an open event when it is created.
///
/// A sent frame is recorded when the sink accepts it, a received one when
/// the stream yields it. Pings and pongs are not recorded.
pub(crate) struct CaptureStream<S> {
    inner: S,
    capture: CaptureHandle,
    worker: usize,
    /// Records this session dropped, added to the shared total on drop so
    /// workers do not contend for one counter.
    dropped: u64,
}

impl<S> CaptureStream<S> {
    /// Wraps a freshly opened connection and records its open event.
    pub(crate) fn open(inner: S, capture: CaptureHandle, worker: usize) -> Self {
        let mut stream = Self {
            inner,
            capture,
            worker,
            dropped: 0,
        };
        stream
            .capture
            .record(worker, Dir::Recv, Event::Open, &mut stream.dropped);
        stream
    }

    fn observe(&mut self, dir: Dir, msg: &Message) {
        let event = match msg {
            Message::Text(text) => Event::Text(text.clone()),
            Message::Binary(bytes) => Event::Binary(bytes.clone()),
            Message::Close(frame) => Event::Close(frame.clone()),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return,
        };
        self.capture
            .record(self.worker, dir, event, &mut self.dropped);
    }
}

impl<S> Drop for CaptureStream<S> {
    fn drop(&mut self) {
        if self.dropped > 0 {
            self.capture
                .0
                .dropped
                .fetch_add(self.dropped, Ordering::Relaxed);
        }
    }
}

impl<S> Stream for CaptureStream<S>
where
    S: Stream<Item = Result<Message, TungsteniteError>> + Unpin,
{
    type Item = Result<Message, TungsteniteError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(msg))) = &poll {
            this.observe(Dir::Recv, msg);
        }
        poll
    }
}

impl<S> Sink<Message> for CaptureStream<S>
where
    S: Sink<Message, Error = TungsteniteError> + Unpin,
{
    type Error = TungsteniteError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let this = &mut *self;
        // Refcounted payloads make keeping a copy for the record cheap.
        let copy = matches!(
            item,
            Message::Text(_) | Message::Binary(_) | Message::Close(_)
        )
        .then(|| item.clone());
        Pin::new(&mut this.inner).start_send(item)?;
        if let Some(copy) = copy {
            this.observe(Dir::Sent, &copy);
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// What the writer thread did.
#[derive(Debug)]
struct WriterOutcome {
    written: u64,
    /// Records received after a write failed.
    lost: u64,
    error: Option<std::io::Error>,
}

/// One line of the capture file.
#[derive(serde::Serialize)]
struct Line<'a> {
    t_us: u64,
    worker: usize,
    dir: &'static str,
    kind: &'static str,
    len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Cow<'a, str>>,
}

impl<'a> Line<'a> {
    fn new(record: &'a Record) -> Self {
        let (kind, len, code, data) = match &record.event {
            Event::Open => ("open", 0, None, None),
            Event::Text(text) => ("text", text.len(), None, Some(Cow::Borrowed(text.as_str()))),
            Event::Binary(bytes) => (
                "binary",
                bytes.len(),
                None,
                Some(Cow::Owned(crate::base64::encode(bytes))),
            ),
            Event::Close(None) => ("close", 0, None, None),
            Event::Close(Some(frame)) => (
                "close",
                2 + frame.reason.len(),
                Some(u16::from(frame.code)),
                Some(Cow::Borrowed(frame.reason.as_str())),
            ),
        };
        Self {
            t_us: record.t_us,
            worker: record.worker,
            dir: record.dir.as_str(),
            kind,
            len,
            code,
            data,
        }
    }
}

/// The writer thread: the header, then one line per record until
/// [`Command::Finish`] arrives or every sender is gone.
fn write_records(
    file: File,
    header: &serde_json::Value,
    receiver: &Receiver<Command>,
) -> WriterOutcome {
    let mut out = BufWriter::with_capacity(256 * 1024, file);
    let mut outcome = WriterOutcome {
        written: 0,
        lost: 0,
        error: None,
    };
    if let Err(err) = write_line(&mut out, header) {
        outcome.error = Some(err);
    }
    loop {
        // Polling instead of blocking in `recv` means the queue never has a
        // sleeping receiver to wake, so a worker's `try_send` is a plain
        // enqueue and never a futex wake-up system call.
        let record = match receiver.try_recv() {
            Ok(Command::Record(record)) => record,
            Ok(Command::Finish) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {
                std::thread::sleep(WRITER_POLL_INTERVAL);
                continue;
            }
        };
        if outcome.error.is_some() {
            outcome.lost += 1;
            continue;
        }
        match write_line(&mut out, &Line::new(&record)) {
            Ok(()) => outcome.written += 1,
            Err(err) => {
                outcome.lost += 1;
                outcome.error = Some(err);
            }
        }
    }
    if outcome.error.is_none()
        && let Err(err) = out.flush()
    {
        outcome.error = Some(err);
    }
    outcome
}

fn write_line(out: &mut impl Write, value: &impl serde::Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(std::io::Error::other)?;
    out.write_all(b"\n")
}

/// Records an agent formats and sends to its coordinator at a time.
#[cfg(feature = "distributed")]
const FORWARD_BATCH: usize = 1_024;

#[cfg(feature = "distributed")]
use crate::distributed::WireRecord;

/// The record as it travels to a distributed coordinator.
#[cfg(feature = "distributed")]
fn wire_record(record: &Record) -> WireRecord {
    let line = Line::new(record);
    WireRecord {
        t_us: line.t_us,
        worker: line.worker,
        dir: line.dir.to_string(),
        kind: line.kind.to_string(),
        len: line.len,
        code: line.code,
        data: line.data.map(Cow::into_owned),
    }
}

/// A distributed agent's capture: the same bounded queue, limit and drop
/// counting as a capture file, with a thread that formats the records and
/// hands them in batches to the control channel instead of a file.
///
/// The thread waits when the control channel is behind, never the workers:
/// the queue then fills and further records are dropped and counted, as
/// they are when a disk is too slow.
#[cfg(feature = "distributed")]
#[derive(Debug)]
pub(crate) struct ForwardingCapture {
    handle: CaptureHandle,
    thread: std::thread::JoinHandle<u64>,
}

#[cfg(feature = "distributed")]
impl ForwardingCapture {
    /// Starts forwarding. Record timestamps count from `start`; `limit` is
    /// the number of records this agent may still queue, `None` for no
    /// limit.
    pub(crate) fn start(
        start: Instant,
        limit: Option<u64>,
        out: tokio::sync::mpsc::Sender<Vec<WireRecord>>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let thread = std::thread::Builder::new()
            .name("wsblast-capture-forward".into())
            .spawn(move || forward_records(&receiver, &out))?;
        Ok(Self {
            handle: CaptureHandle(Arc::new(Shared {
                sender,
                start,
                limit: limit.unwrap_or(u64::MAX),
                claimed: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            })),
            thread,
        })
    }

    pub(crate) fn handle(&self) -> CaptureHandle {
        self.handle.clone()
    }

    /// Forwards every queued record and returns the number of records
    /// dropped. Call it once every worker has stopped, while the batches are
    /// still being read.
    pub(crate) async fn finish(self) -> u64 {
        let Self { handle, thread } = self;
        tokio::task::spawn_blocking(move || {
            let _ = handle.0.sender.send(Command::Finish);
            let lost = thread.join().unwrap_or(0);
            handle.0.dropped.load(Ordering::Relaxed) + lost
        })
        .await
        .unwrap_or(0)
    }
}

/// The forwarding thread: batches of formatted records until
/// [`Command::Finish`] arrives or every sender is gone. Returns the records
/// it could not hand over because the control channel was closed.
#[cfg(feature = "distributed")]
fn forward_records(
    receiver: &Receiver<Command>,
    out: &tokio::sync::mpsc::Sender<Vec<WireRecord>>,
) -> u64 {
    let mut lost = 0u64;
    let mut batch: Vec<WireRecord> = Vec::with_capacity(FORWARD_BATCH);
    let mut send = |batch: &mut Vec<WireRecord>| {
        if batch.is_empty() {
            return;
        }
        let records = std::mem::replace(batch, Vec::with_capacity(FORWARD_BATCH));
        let count = records.len() as u64;
        if out.blocking_send(records).is_err() {
            lost += count;
        }
    };
    loop {
        match receiver.try_recv() {
            Ok(Command::Record(record)) => {
                batch.push(wire_record(&record));
                if batch.len() >= FORWARD_BATCH {
                    send(&mut batch);
                }
            }
            Ok(Command::Finish) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {
                send(&mut batch);
                std::thread::sleep(WRITER_POLL_INTERVAL);
            }
        }
    }
    send(&mut batch);
    lost
}

/// Records of one agent for the coordinator's capture file.
#[cfg(feature = "distributed")]
#[derive(Debug)]
pub(crate) struct MergedBatch {
    /// The agent's position in `--agents`, from 0.
    pub(crate) agent: usize,
    /// Added to every record's `t_us`: when the phase started on the
    /// coordinator, counted from the start of the capture.
    pub(crate) offset_us: u64,
    pub(crate) records: Vec<WireRecord>,
}

/// One line of a distributed capture file.
#[cfg(feature = "distributed")]
#[derive(serde::Serialize)]
struct MergedLine<'a> {
    t_us: u64,
    agent: usize,
    worker: usize,
    dir: &'a str,
    kind: &'a str,
    len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<&'a str>,
}

/// What the coordinator's writer did.
#[cfg(feature = "distributed")]
#[derive(Debug)]
struct MergedOutcome {
    writer: WriterOutcome,
    /// Records that arrived after the limit was reached.
    over_limit: u64,
}

/// A distributed coordinator's capture file: one writer thread writes the
/// records every agent sends, with the agent added to each line, and keeps
/// the limit for the whole run.
#[cfg(feature = "distributed")]
#[derive(Debug)]
pub(crate) struct MergedCapture {
    sender: tokio::sync::mpsc::Sender<MergedBatch>,
    thread: std::thread::JoinHandle<MergedOutcome>,
    path: PathBuf,
    limit: u64,
    /// Records handed to the writer so far.
    queued: Arc<AtomicU64>,
}

#[cfg(feature = "distributed")]
impl CaptureFile {
    /// Starts the coordinator's writer thread. The header line names the
    /// agents, in `--agents` order, which the `agent` field of every record
    /// indexes.
    pub(crate) fn start_merged(self, agents: &[String]) -> std::io::Result<MergedCapture> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<MergedBatch>(64);
        let header = serde_json::json!({
            "wsblast_capture": FORMAT_VERSION,
            "target": self.target,
            "started": crate::clock::rfc3339_utc(SystemTime::now()),
            "agents": agents,
        });
        let file = self.file;
        let limit = if self.limit == 0 {
            u64::MAX
        } else {
            self.limit
        };
        let thread = std::thread::Builder::new()
            .name("wsblast-capture".into())
            .spawn(move || {
                let mut out = BufWriter::with_capacity(256 * 1024, file);
                let mut writer = WriterOutcome {
                    written: 0,
                    lost: 0,
                    error: write_line(&mut out, &header).err(),
                };
                let mut over_limit = 0u64;
                while let Some(batch) = receiver.blocking_recv() {
                    for record in &batch.records {
                        if writer.written + writer.lost >= limit {
                            over_limit += 1;
                            continue;
                        }
                        if writer.error.is_some() {
                            writer.lost += 1;
                            continue;
                        }
                        let line = MergedLine {
                            t_us: record.t_us.saturating_add(batch.offset_us),
                            agent: batch.agent,
                            worker: record.worker,
                            dir: &record.dir,
                            kind: &record.kind,
                            len: record.len,
                            code: record.code,
                            data: record.data.as_deref(),
                        };
                        match write_line(&mut out, &line) {
                            Ok(()) => writer.written += 1,
                            Err(err) => {
                                writer.lost += 1;
                                writer.error = Some(err);
                            }
                        }
                    }
                }
                if writer.error.is_none()
                    && let Err(err) = out.flush()
                {
                    writer.error = Some(err);
                }
                MergedOutcome { writer, over_limit }
            })?;
        Ok(MergedCapture {
            sender,
            thread,
            path: self.path,
            limit: self.limit,
            queued: Arc::new(AtomicU64::new(0)),
        })
    }
}

#[cfg(feature = "distributed")]
impl MergedCapture {
    /// Where the agents' records go, and the count of records sent there.
    pub(crate) fn sink(&self) -> (tokio::sync::mpsc::Sender<MergedBatch>, Arc<AtomicU64>) {
        (self.sender.clone(), Arc::clone(&self.queued))
    }

    /// Records the agents may still send before the limit is reached;
    /// `None` without a limit.
    pub(crate) fn remaining(&self) -> Option<u64> {
        (self.limit != 0).then(|| {
            self.limit
                .saturating_sub(self.queued.load(Ordering::Relaxed))
        })
    }

    /// Writes out every record received, closes the file and returns the
    /// totals; `agents_dropped` is what the agents reported dropping. Call
    /// it once every agent's control task has ended.
    pub(crate) async fn finish(self, agents_dropped: u64) -> CaptureSummary {
        let Self {
            sender,
            thread,
            path,
            limit,
            ..
        } = self;
        drop(sender);
        let outcome = tokio::task::spawn_blocking(move || thread.join())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| MergedOutcome {
                writer: WriterOutcome {
                    written: 0,
                    lost: 0,
                    error: Some(std::io::Error::other("capture writer thread panicked")),
                },
                over_limit: 0,
            });
        if let Some(err) = &outcome.writer.error {
            eprintln!(
                "{} writing capture file '{}' failed after {} records: {err}",
                colored::Colorize::yellow(colored::Colorize::bold("Warning:")),
                path.display(),
                outcome.writer.written
            );
        }
        CaptureSummary {
            path: path.display().to_string(),
            frames_written: outcome.writer.written,
            frames_dropped: agents_dropped + outcome.writer.lost + outcome.over_limit,
            limit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(capacity: usize, limit: u64) -> (CaptureHandle, Receiver<Command>) {
        let (sender, receiver) = sync_channel(capacity);
        let handle = CaptureHandle(Arc::new(Shared {
            sender,
            start: Instant::now(),
            limit,
            claimed: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }));
        (handle, receiver)
    }

    #[test]
    fn full_queue_drops_without_using_up_the_limit() {
        let (handle, receiver) = shared(1, 2);
        let mut dropped = 0;
        handle.record(0, Dir::Sent, Event::Open, &mut dropped);
        handle.record(0, Dir::Sent, Event::Open, &mut dropped);
        assert_eq!(dropped, 1, "the second record finds the queue full");

        assert!(matches!(receiver.try_recv(), Ok(Command::Record(_))));
        handle.record(0, Dir::Sent, Event::Open, &mut dropped);
        assert_eq!(dropped, 1, "the dropped record did not use a slot");

        assert!(matches!(receiver.try_recv(), Ok(Command::Record(_))));
        handle.record(0, Dir::Sent, Event::Open, &mut dropped);
        assert_eq!(dropped, 2, "the limit of 2 is reached");
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn unlimited_capture_drops_only_on_a_full_queue() {
        let (handle, receiver) = shared(4, u64::MAX);
        let mut dropped = 0;
        for _ in 0..6 {
            handle.record(1, Dir::Recv, Event::Open, &mut dropped);
        }
        assert_eq!(dropped, 2);
        assert_eq!(receiver.try_iter().count(), 4);
    }

    #[test]
    fn lines_carry_kind_length_and_data() {
        let record = |event| Record {
            t_us: 7,
            worker: 3,
            dir: Dir::Sent,
            event,
        };
        let json = |event| serde_json::to_string(&Line::new(&record(event))).unwrap();
        assert_eq!(
            json(Event::Text(Utf8Bytes::from_static("hi \"x\""))),
            r#"{"t_us":7,"worker":3,"dir":"sent","kind":"text","len":6,"data":"hi \"x\""}"#
        );
        assert_eq!(
            json(Event::Binary(bytes::Bytes::from_static(&[0, 255]))),
            r#"{"t_us":7,"worker":3,"dir":"sent","kind":"binary","len":2,"data":"AP8="}"#
        );
        assert_eq!(
            json(Event::Open),
            r#"{"t_us":7,"worker":3,"dir":"sent","kind":"open","len":0}"#
        );
        assert_eq!(
            json(Event::Close(Some(CloseFrame {
                code: 1000.into(),
                reason: Utf8Bytes::from_static("bye"),
            }))),
            r#"{"t_us":7,"worker":3,"dir":"sent","kind":"close","len":5,"code":1000,"data":"bye"}"#
        );
    }
}
