//! The session: its lifetime, request/response pairing, and the timeouts
//! (docs/map/territory/session-lifetime.md, docs/map/territory/request-pairing.md).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::protocol::{
    decode_limits, decode_version, encode_init, packet, Request, Response, ServerLimits,
    ServerVersion, LIMITS_EXTENSION, VERSION,
};
use crate::wire::Reader;

/// Read-packet overhead: type(1) + id(4) + data length(4).
const READ_OVERHEAD: u32 = 9;

/// Write-packet overhead: length prefix(4) + type(1) + id(4) + handle length prefix(4) +
/// offset(8) + data length prefix(4), without the handle's own bytes
/// ([`crate::WriteFile::max_chunk`] adds those).
///
/// Counting the length prefix here and not in `READ_OVERHEAD` is deliberate
/// (docs/map/territory/transfer-lengths.md).
pub(crate) const WRITE_OVERHEAD: usize = 25;

/// The smallest read or write length a server's limits can lower a transfer to.
const SMALLEST_TRANSFER_LEN: u64 = 64;

/// Timeouts and packet ceilings for a [`Session`].
#[derive(Debug, Clone)]
pub struct Config {
    /// How long to wait for a reply **after the request's bytes have been written** — server
    /// latency and the response transfer only. Also bounds the whole handshake, its write
    /// included. Default 30 s.
    pub request_timeout: Duration,
    /// How long to wait for a request's bytes to **reach the stream**, a budget of its own. Also
    /// bounds how long [`Session::close`] waits for the queue to drain. Default 60 s.
    pub write_timeout: Duration,
    /// The largest packet this client will accept from the server, checked before anything is
    /// allocated. A larger header ends the session: the handshake fails with [`Error::TooLong`], and
    /// a waiting request gets [`Error::SessionEnded`] whose cause is `TooLong`. Default 4 MiB.
    pub max_inbound_packet: usize,
    /// The largest packet this client will **send**; a larger request is refused as
    /// [`Error::TooLong`], not split. Splitting a transfer is the caller's, by
    /// [`Session::max_read_len`] and [`crate::WriteFile::max_chunk`], which a server's stated
    /// limits can only lower. Default 262,144.
    pub max_outbound_packet: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(60),
            max_inbound_packet: 4 * 1024 * 1024,
            max_outbound_packet: 262_144,
        }
    }
}

/// The waiters by request id, behind a synchronous mutex so `Slot`'s `Drop` can take it
/// (docs/map/territory/request-pairing.md).
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A poisoned map is still a usable map.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Removes a request's slot if the caller goes away before the reply lands; nothing else would
/// (docs/map/territory/request-pairing.md).
struct Slot {
    pending: Pending,
    id: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // A no-op if the reply already arrived.
        lock(&self.pending).remove(&self.id);
    }
}

/// One packet handed to the writer task, with a channel reporting when it reached the stream.
struct Outbound {
    bytes: Vec<u8>,
    written: oneshot::Sender<std::result::Result<(), Arc<Error>>>,
}

/// A live SFTP conversation over some byte stream — any
/// `AsyncRead + AsyncWrite + Unpin + Send + 'static`, such as `russh::Channel::into_stream()`.
pub struct Session {
    pending: Pending,
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    /// Set by the reader when it stops; `enqueue` checks it first.
    reader_done: Arc<AtomicBool>,
    next_id: AtomicU32,
    config: Config,
    server: ServerVersion,
    /// `None` when the server did not advertise `limits@openssh.com` or did not answer it usably.
    limits: Option<ServerLimits>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Session {
    /// Performs the handshake, starts the reader and writer tasks, and asks for the server's
    /// limits if it advertises `limits@openssh.com`. A server offering any version but 3 is refused
    /// with [`Error::UnsupportedVersion`]; one that never answers, with [`Error::Timeout`].
    pub async fn open<S>(stream: S, config: Config) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // The handshake is on a clock too (docs/map/territory/session-lifetime.md).
        let (stream, server) =
            match tokio::time::timeout(config.request_timeout, Self::handshake(stream, &config))
                .await
            {
                Ok(result) => result?,
                Err(_) => return Err(Error::Timeout),
            };

        let (rd, wr) = tokio::io::split(stream);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::unbounded_channel::<Outbound>();

        let reader = tokio::spawn(read_loop(
            rd,
            Arc::clone(&pending),
            Arc::clone(&reader_done),
            config.max_inbound_packet,
        ));
        let writer = tokio::spawn(write_loop(wr, rx));

        let mut session = Self {
            pending,
            outbound: Some(tx),
            reader_done,
            next_id: AtomicU32::new(1),
            config,
            server,
            limits: None,
            reader: Some(reader),
            writer: Some(writer),
        };
        session.limits = session.query_limits().await?;
        Ok(session)
    }

    /// Asks `limits@openssh.com` once, and only of a server that advertised it. A status, a short
    /// body or a reply timeout leaves `None`; a link that is gone fails the open.
    async fn query_limits(&self) -> Result<Option<ServerLimits>> {
        if !self.server.advertises(LIMITS_EXTENSION) {
            return Ok(None);
        }
        let req = Request::Extended {
            name: LIMITS_EXTENSION.to_vec(),
            data: Vec::new(),
        };
        match self.request(req).await {
            Ok(Response::ExtendedReply(body)) => Ok(decode_limits(&body).ok()),
            Err(e @ (Error::Eof | Error::SessionEnded { .. } | Error::WriteTimeout)) => Err(e),
            _ => Ok(None),
        }
    }

    async fn handshake<S>(mut stream: S, config: &Config) -> Result<(S, ServerVersion)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        // Before the split, so it needs no request id and no entry in the pending map.
        stream.write_all(&encode_init(VERSION)).await?;
        stream.flush().await?;

        let (ty, body) = read_packet(&mut stream, config.max_inbound_packet).await?;
        if ty != packet::VERSION {
            return Err(Error::UnexpectedReply {
                expected: "VERSION",
                got: ty,
            });
        }
        let server = decode_version(&mut Reader::new(&body))?;

        // This refusal is what lets `Response::decode` read STATUS's message and language tag
        // unconditionally
        // (docs/map/territory/packets.md).
        if server.version != VERSION {
            return Err(Error::UnsupportedVersion {
                theirs: server.version,
                ours: VERSION,
            });
        }
        Ok((stream, server))
    }

    pub fn server_version(&self) -> &ServerVersion {
        &self.server
    }

    /// What the server answered to `limits@openssh.com`, or `None` if it was not asked or did not
    /// answer usably.
    pub fn server_limits(&self) -> Option<&ServerLimits> {
        self.limits.as_ref()
    }

    /// The server's own bound on an `SSH_FXP_WRITE`'s data for a handle this long, if it stated
    /// one. Never below [`SMALLEST_TRANSFER_LEN`].
    pub(crate) fn server_write_len(&self, handle_len: usize) -> Option<usize> {
        let limits = self.limits.as_ref()?;
        let overhead = (WRITE_OVERHEAD + handle_len) as u64;
        let by_packet = stated(limits.max_packet_len).map(|p| p.saturating_sub(overhead));
        let bound = match (stated(limits.max_write_len), by_packet) {
            (None, None) => return None,
            (Some(a), None) | (None, Some(a)) => a,
            (Some(a), Some(b)) => a.min(b),
        };
        Some(usize::try_from(bound.max(SMALLEST_TRANSFER_LEN)).unwrap_or(usize::MAX))
    }

    /// How many requests are registered and still waiting for a reply. Zero when idle, including
    /// after a cancelled request.
    pub fn in_flight(&self) -> usize {
        lock(&self.pending).len()
    }

    /// This session's settings — read by [`crate::WriteFile::max_chunk`], which has to know the
    /// outbound ceiling but lives in another module.
    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    /// The largest `SSH_FXP_READ` length that will fit inside [`Config::max_outbound_packet`],
    /// lowered to the server's stated read length where that is smaller.
    ///
    /// A caller reading a whole file loops on this.
    pub fn max_read_len(&self) -> u32 {
        let ceiling = u32::try_from(self.config.max_outbound_packet).unwrap_or(u32::MAX);
        let ours = ceiling.saturating_sub(READ_OVERHEAD);
        match self.server_read_len() {
            Some(theirs) => ours.min(theirs),
            None => ours,
        }
    }

    /// The server's own stated read length, if it stated one. Never below
    /// 64 bytes. Unlike [`Self::max_read_len`] this carries none of our ceiling,
    /// for a caller whose read size is set by something else and only needs the server's bound.
    pub fn server_read_len(&self) -> Option<u32> {
        let v = stated(self.limits.as_ref()?.max_read_len)?;
        Some(u32::try_from(v.max(SMALLEST_TRANSFER_LEN)).unwrap_or(u32::MAX))
    }

    /// Registers a waiter and hands the encoded packet to the writer task, **without awaiting the
    /// write**. Returns the slot guard and the two receivers. Split from [`Self::request`] as the
    /// seam a pipelining caller would use (docs/map/territory/request-pairing.md).
    #[allow(clippy::type_complexity)]
    fn enqueue(
        &self,
        req: Request,
    ) -> Result<(
        Slot,
        oneshot::Receiver<std::result::Result<(), Arc<Error>>>,
        oneshot::Receiver<Result<Response>>,
    )> {
        // Liveness first: after a half-close the write half still accepts bytes that no reply
        // will answer (docs/map/territory/request-pairing.md).
        if self.reader_done.load(Ordering::Acquire) {
            return Err(Error::Eof);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();

        // ① Register before writing, and refuse an id still outstanding after the counter wraps.
        {
            let mut pending = lock(&self.pending);
            if pending.contains_key(&id) {
                return Err(Error::RequestIdInUse(id));
            }
            pending.insert(id, reply_tx);
        }
        let slot = Slot {
            pending: Arc::clone(&self.pending),
            id,
        };

        let bytes = req.encode(id);

        // ② Refuse an oversized packet rather than send it.
        if bytes.len() > self.config.max_outbound_packet {
            return Err(Error::TooLong {
                len: bytes.len() as u64,
                limit: self.config.max_outbound_packet as u64,
            });
        }

        // ③ Hand the whole packet to the writer task in one synchronous send. Writing inline here
        //    would not be cancel-safe (docs/map/territory/request-pairing.md).
        let (written_tx, written_rx) = oneshot::channel();
        let outbound = self.outbound.as_ref().ok_or(Error::Eof)?;
        if outbound
            .send(Outbound {
                bytes,
                written: written_tx,
            })
            .is_err()
        {
            return Err(Error::Eof);
        }

        Ok((slot, written_rx, reply_rx))
    }

    /// Sends one request and waits for the reply that carries its id: first for the bytes to reach
    /// the stream ([`Config::write_timeout`], else [`Error::WriteTimeout`]), then for the reply
    /// ([`Config::request_timeout`], else [`Error::Timeout`]). Cancel-safe.
    pub async fn request(&self, req: Request) -> Result<Response> {
        // The slot guard lives for the whole call.
        let (_slot, written_rx, reply_rx) = self.enqueue(req)?;

        // ④ Wait for the bytes to reach the stream, on a clock; a failed write is reported.
        match tokio::time::timeout(self.config.write_timeout, written_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(cause))) => return Err(Error::SessionEnded { cause }),
            Ok(Err(_)) => return Err(Error::Eof),
            Err(_) => return Err(Error::WriteTimeout),
        }

        // ⑤ The reply clock starts here, after the bytes are on the stream.
        match tokio::time::timeout(self.config.request_timeout, reply_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Err(Error::Eof),
            Err(_) => Err(Error::Timeout),
        }
    }

    /// Ends the session, **sending what is already queued** before stopping the writer, and fails
    /// any request still waiting with [`Error::Eof`].
    pub async fn close(mut self) {
        // Dropping the sender lets the writer finish the queue and exit; it is never aborted.
        self.outbound = None;
        if let Some(writer) = self.writer.take() {
            // Bounded by the write budget.
            let _ = tokio::time::timeout(self.config.write_timeout, writer).await;
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        let mut pending = lock(&self.pending);
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(Error::Eof));
        }
    }
}

/// The server version, extension count, next id, requests in flight, and whether the reader has
/// stopped. Not the stream.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("server_version", &self.server.version)
            .field("extensions", &self.server.extensions.len())
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .field("in_flight", &lock(&self.pending).len())
            .field("reader_done", &self.reader_done.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The reader is aborted and the writer is not: aborting the writer could leave half a
        // packet on the wire (docs/map/territory/session-lifetime.md).
        self.outbound = None;
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
    }
}

/// Owns the write half. Nothing can cancel a packet part-way through: the only thing that awaits
/// here is this task, it is never aborted, and it ends when the channel closes.
async fn write_loop<W: AsyncWrite + Unpin>(mut wr: W, mut rx: mpsc::UnboundedReceiver<Outbound>) {
    while let Some(job) = rx.recv().await {
        let result = match wr.write_all(&job.bytes).await {
            Ok(()) => wr.flush().await,
            Err(e) => Err(e),
        };

        let Err(e) = result else {
            let _ = job.written.send(Ok(()));
            continue;
        };

        // The one cause reaches this job and every job queued behind it.
        let cause = Arc::new(Error::Io(e));
        let _ = job.written.send(Err(Arc::clone(&cause)));
        rx.close();
        while let Some(job) = rx.recv().await {
            let _ = job.written.send(Err(Arc::clone(&cause)));
        }
        break;
    }
}

/// Reads one framed packet: `u32 length | u8 type | body`, where the length covers the type byte.
async fn read_packet<R: AsyncRead + Unpin>(rd: &mut R, max: usize) -> Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    rd.read_exact(&mut len_buf).await.map_err(eof_as_eof)?;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len == 0 {
        return Err(Error::Truncated { needed: 1, had: 0 });
    }
    if len > max {
        return Err(Error::TooLong {
            len: len as u64,
            limit: max as u64,
        });
    }

    let mut ty = [0u8; 1];
    rd.read_exact(&mut ty).await.map_err(eof_as_eof)?;

    let mut body = vec![0u8; len - 1];
    rd.read_exact(&mut body).await.map_err(eof_as_eof)?;
    Ok((ty[0], body))
}

/// A limit field, with `0` read as "no limit stated".
fn stated(v: u64) -> Option<u64> {
    (v != 0).then_some(v)
}

fn eof_as_eof(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::Eof
    } else {
        Error::Io(e)
    }
}

/// The response loop. One packet at a time, each handed to whoever is waiting on its id.
async fn read_loop<R: AsyncRead + Unpin>(
    mut rd: R,
    pending: Pending,
    reader_done: Arc<AtomicBool>,
    max: usize,
) {
    // Why the loop ended is carried to the waiters (docs/map/territory/request-pairing.md).
    let cause = loop {
        let (ty, body) = match read_packet(&mut rd, max).await {
            Ok(p) => p,
            Err(e) => break e,
        };

        let mut r = Reader::new(&body);
        let id = match r.u32() {
            Ok(id) => id,
            Err(e) => break e,
        };

        // The id first, so a body that does not decode fails only its own request.
        let decoded = Response::decode(ty, &mut r);

        let waiter = lock(&pending).remove(&id);
        match waiter {
            Some(tx) => {
                let _ = tx.send(decoded);
            }
            // A reply nobody is waiting for is dropped, not fatal.
            None => continue,
        }
    };

    // Published before the drain, so a racing request either fails fast or gets the cause.
    reader_done.store(true, Ordering::Release);

    // Every waiter gets the same cause.
    let cause = Arc::new(cause);
    let mut pending = lock(&pending);
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(Error::SessionEnded {
            cause: Arc::clone(&cause),
        }));
    }
}
