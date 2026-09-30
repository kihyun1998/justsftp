//! The session: framing, request/response pairing, and the timeouts.
//!
//! This is the part a later change cannot cheaply revisit. Two of the eight upstream defects
//! (`docs/map/territory/upstream-traps.md`) are here, both on the concurrency path.

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
/// offset(8) + data length prefix(4). **The handle's own bytes are not in here** — the server
/// chooses that length per file, so it is added by the caller that holds the handle
/// ([`crate::WriteFile::max_chunk`]).
///
/// ⚠️ **This counts the 4-byte length prefix and `READ_OVERHEAD` does not — do not "fix" the
/// asymmetry.** They bound different things. The check this feeds is
/// `req.encode(id).len() > max_outbound_packet`, and `encode` returns the framed packet, prefix
/// included (`protocol.rs`'s encoding test pins a 1-byte handle and 2 data bytes at 28 total:
/// 25 + 1 + 2). `READ_OVERHEAD` bounds a *requested length* against the reply that comes back.
pub(crate) const WRITE_OVERHEAD: usize = 25;

/// The smallest read or write length a server's limits can lower a transfer to. OpenSSH's own
/// client floors at the same number (`sftp-client.c`, after `sftp_get_limits`).
const SMALLEST_TRANSFER_LEN: u64 = 64;

/// Tunables. Every default is a number with a reason attached.
#[derive(Debug, Clone)]
pub struct Config {
    /// How long to wait for a reply **after the request's bytes have been written**.
    ///
    /// ⚠️ Do not compare this against `russh-sftp`'s 10 seconds without reading what each measures.
    /// There the clock starts on enqueue, so the budget is shared between waiting for the socket and
    /// waiting for the server. Here it covers server latency and the response transfer only, so a
    /// larger number is also a stricter one.
    pub request_timeout: Duration,
    /// How long to wait for a request's bytes to **reach the stream**.
    ///
    /// ⚠️ **A separate budget, and its absence was a silent infinite hang.** Splitting the write out
    /// of the reply budget is what makes [`Self::request_timeout`] honest (upstream #95) — but an
    /// earlier version split it and then put no clock on the write half at all. A server that
    /// accepts the `sftp` subsystem and then stops reading never reopens the channel window, so the
    /// write pends forever and, because writes are serial, **every** request in flight pends behind
    /// it. No `Timeout`, no `Io`, no `Eof`. That is the same wedged-but-accepted case the handshake
    /// clock exists for, arriving on the other side.
    ///
    /// Deliberately larger than the reply budget: a slow write is ordinary on a congested link,
    /// whereas a server that has gone silent is not.
    pub write_timeout: Duration,
    /// The largest packet this client will accept from the server.
    ///
    /// ⚠️ **There has to be one.** `russh-sftp` passes `u32::MAX` to its reader
    /// (`client/mod.rs:74`) and then does `vec![0; length as usize]` (`utils.rs:21`), so a server —
    /// hostile, or merely broken — can make the client allocate **4 GiB from one packet header**.
    /// Its outbound check (`rawsession.rs:199-203`) does not help: it guards what we send. The
    /// default here is 16x the 256 KiB that is itself the default maximum payload, which leaves room
    /// for a large `READDIR` batch while keeping the number bounded.
    pub max_inbound_packet: usize,
    /// The largest packet this client will **send**.
    ///
    /// ⚠️ This is one of the eight upstream traps — *"read/write chunk size must follow the
    /// server's advertised `max_packet_len`, not a constant"*. It is answered here in two halves, and only one of them
    /// is this crate's. **Refusing** to emit an oversized packet belongs here, because a codec that
    /// silently emits something a strict server will reject is wrong at any policy. **Chunking** a
    /// large transfer into packets that fit does not: how to split, how many to keep in flight, and
    /// whether to re-issue a short read are transfer *policy*, and those are the caller's.
    ///
    /// The default matches `russh-sftp`'s (`client/mod.rs:41-49`). A server that states smaller
    /// limits through `limits@openssh.com` lowers [`Session::max_read_len`] and
    /// [`crate::WriteFile::max_chunk`] below what this ceiling allows, never above
    /// ([`Session::server_limits`]).
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

/// ⚠️ **A synchronous mutex, deliberately.** No critical section here holds the lock across an
/// `.await` — every one of them is an insert, a remove, or a drain followed by a synchronous
/// `oneshot::send` — so an async mutex buys nothing, and it **costs** the one thing this map needs:
/// a `Drop` impl can take a `std::sync::Mutex` guard and cannot take a `tokio::sync::Mutex` one.
/// Without that, a cancelled request leaks its entry until a reply arrives, which against a server
/// that never answers is never. `russh-sftp` reaches the same place from the other direction, with a
/// synchronous `DashMap` (`rawsession.rs:2`).
type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<Response>>>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panic while holding this lock poisons it. The map is plain data and a poisoned map is still
    // a usable map, so recovering beats propagating someone else's panic into every later request.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Removes a request's slot if the caller goes away before the reply lands.
///
/// ⚠️ **Nothing else reclaims it.** `read_loop` removes on a matching reply, and `close()` drains —
/// but a dropped future runs neither, because the sender half was moved into the map and only the
/// receiver dies with the future. Against a server that never answers, the entry is permanent. That
/// matters more than it looks: cancelling a transfer mid-flight is a **supported operation**, not
/// an edge case.
struct Slot {
    pending: Pending,
    id: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Unconditional: if the reply already arrived, `read_loop` removed it and this is a no-op.
        lock(&self.pending).remove(&self.id);
    }
}

/// One packet handed to the writer task, with a channel reporting when it reached the stream.
struct Outbound {
    bytes: Vec<u8>,
    written: oneshot::Sender<std::result::Result<(), Arc<Error>>>,
}

/// A live SFTP conversation over some byte stream.
///
/// ⚠️ **The stream is the only thing this type knows about the world.** It is not generic in its
/// own signature on purpose — a consumer should not have to name the stream type to hold a session —
/// but the bound at [`Session::open`] is exactly `AsyncRead + AsyncWrite + Unpin + Send`, which is
/// what `russh::Channel::into_stream()` yields. That coincidence is the entire relationship between
/// this crate and the SSH library, and it is why neither names the other.
pub struct Session {
    pending: Pending,
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    /// Set by the reader when it stops. See [`Session::request`]'s liveness check.
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
    /// Performs the handshake and starts the reader and writer tasks.
    pub async fn open<S>(stream: S, config: Config) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // ⚠️ **The handshake is on a clock too, and this is the one exchange where forgetting that
        // hangs forever.** A server can accept the `sftp` subsystem channel and then never send
        // `SSH_FXP_VERSION` — the wedged-but-accepted case a caller has to tell apart from a dead
        // link. `russh-sftp` routes its `init()` through the same timed path as every other
        // request (`rawsession.rs:239-246`).
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
        let req = Request::Extended { name: LIMITS_EXTENSION.to_vec(), data: Vec::new() };
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
        // The handshake happens before the stream is split, so it needs no request id and no entry
        // in the pending map. `russh-sftp` instead keys its map on `Option<u32>` and reserves `None`
        // for this one exchange (`rawsession.rs:35`); doing it inline removes the special case
        // rather than encoding it in the key type.
        stream.write_all(&encode_init(VERSION)).await?;
        stream.flush().await?;

        let (ty, body) = read_packet(&mut stream, config.max_inbound_packet).await?;
        if ty != packet::VERSION {
            return Err(Error::UnexpectedReply { expected: "VERSION", got: ty });
        }
        let server = decode_version(&mut Reader::new(&body))?;

        // ⚠️ **This guard is what neither crate on disk has, and it is not pedantry.** The draft's
        // § 10.1 records that the error message and language tag were *added to STATUS in version
        // 3*. Both `russh-sftp` and `openssh-sftp-protocol` read those two fields unconditionally
        // while hard-coding v3, so against a v2 server every status packet is over-read. Refusing
        // here is what lets `Response::decode` read them without a version check.
        if server.version != VERSION {
            return Err(Error::UnsupportedVersion { theirs: server.version, ours: VERSION });
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

    /// How many requests are registered and still waiting for a reply.
    ///
    /// ⚠️ **A diagnostic, and the reason it is public is that an invariant nothing can observe is an
    /// invariant nothing will keep.** A cancelled request reclaims its slot through a `Drop` guard;
    /// with the map private and no accessor, no test could tell a working guard from a missing one,
    /// which is exactly how the leak survived a full adversarial pass. Steady state is zero.
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
    /// A caller reading a whole file loops on this. Splitting is the caller's, not this crate's —
    /// see the field's own note.
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
    /// write**. Returns the slot guard and the two receivers.
    ///
    /// ⚠️ **This is split out of [`Self::request`] on purpose.**
    /// Pipelined file I/O needs a dispatch that returns the reply receiver *without* awaiting
    /// anything, so a caller can keep N requests outstanding — `russh-sftp`'s `send`, exposed as
    /// `write_nowait` and consumed by its `AsyncWrite for File`. Fused into one `async fn`, a
    /// `poll_write` built on top could not return until the **server** answered, which is zero
    /// pipelining. It stays private until a pipelining policy is measured; what it does is keep
    /// the seam cheap to expose.
    #[allow(clippy::type_complexity)]
    fn enqueue(
        &self,
        req: Request,
    ) -> Result<(
        Slot,
        oneshot::Receiver<std::result::Result<(), Arc<Error>>>,
        oneshot::Receiver<Result<Response>>,
    )> {
        // ⚠️ **Liveness before anything else.** The reader ending is the ordinary shape of a peer
        // going away — a half-close on TCP, `SSH_MSG_CHANNEL_EOF` on a channel — and the write half
        // stays writable through it. Without this check a request issued afterwards enqueues
        // happily, is written happily, and then waits the **full reply budget** for an answer that
        // structurally cannot come, reporting `Timeout`, i.e. *"the server is slow"*, about a
        // connection that is already gone. `russh-sftp` closes the same hole with a
        // `CancellationToken` shared by both halves (`client/mod.rs:88-89`, `:105`, `:121`).
        if self.reader_done.load(Ordering::Acquire) {
            return Err(Error::Eof);
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();

        // ① Register **before** writing. A reply cannot arrive for a request that has not been
        //    sent, but it can arrive before this task is scheduled again — so registering after the
        //    write is a race that loses the reply and reports a timeout. `russh-sftp` also registers
        //    first, and additionally `insert`s blindly (`rawsession.rs:206`): on an id collision the
        //    previous sender is dropped and the earlier caller is told "sender dropped". Here a
        //    collision is an error, because after `u32::MAX` requests the counter wraps and silently
        //    answering the wrong caller is worse than failing.
        {
            let mut pending = lock(&self.pending);
            if pending.contains_key(&id) {
                return Err(Error::RequestIdInUse(id));
            }
            pending.insert(id, reply_tx);
        }
        let slot = Slot { pending: Arc::clone(&self.pending), id };

        let bytes = req.encode(id);

        // ② Refuse an oversized packet rather than emitting one a strict server will reject.
        //    `russh-sftp` does the same (`rawsession.rs:199-203`). See `Config::max_outbound_packet`
        //    for why refusing lives here and chunking does not.
        if bytes.len() > self.config.max_outbound_packet {
            return Err(Error::TooLong {
                len: bytes.len() as u64,
                limit: self.config.max_outbound_packet as u64,
            });
        }

        // ③ Hand the whole packet to the writer task in one **synchronous** send.
        //
        //    ⚠️ **This indirection is not ceremony — it is what makes `request()` cancel-safe.** An
        //    earlier draft took a lock on the write half and called `write_all` here, which reads as
        //    simpler and is wrong: `write_all` is not cancel-safe, so a caller dropped inside it (a
        //    `select!`, an outer timeout, an aborted task) releases the lock with **half an SFTP
        //    packet on the wire**. The next request appends its frame directly after it, the server
        //    misreads the length prefix, and every packet from then on is misframed — silently,
        //    until the reader gives up. Both references avoid this the same way, by owning the write
        //    half in a task nobody can cancel (`russh-sftp` `client/mod.rs:110-127`). Cancelling a
        //    transfer part way is a supported operation, so this had to be right here.
        let (written_tx, written_rx) = oneshot::channel();
        let outbound = self.outbound.as_ref().ok_or(Error::Eof)?;
        if outbound.send(Outbound { bytes, written: written_tx }).is_err() {
            return Err(Error::Eof);
        }

        Ok((slot, written_rx, reply_rx))
    }

    /// Sends one request and waits for the reply that carries its id.
    ///
    /// ⚠️ **The order of the steps is the fix for upstream #33 and #95, and neither is recoverable
    /// by a later change.** See `enqueue` for ① to ③.
    pub async fn request(&self, req: Request) -> Result<Response> {
        // The guard lives for the whole call, so every early return below — and a caller dropped at
        // any await point — reclaims the pending slot.
        let (_slot, written_rx, reply_rx) = self.enqueue(req)?;

        // ④ Wait for the bytes to reach the stream, **on a clock**. A failed write is reported
        //    rather than swallowed: `russh-sftp`'s writer task does
        //    `let _ = wr.write_all(&data[..]).await` (`client/mod.rs:119`), so a send that never
        //    happened surfaces only as a reply timeout with no cause attached.
        match tokio::time::timeout(self.config.write_timeout, written_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(cause))) => return Err(Error::SessionEnded { cause }),
            Ok(Err(_)) => return Err(Error::Eof),
            Err(_) => return Err(Error::WriteTimeout),
        }

        // ⑤ **The reply clock starts here — after the bytes are on the stream.** That is upstream
        //    #95: there the timeout begins when `send()` returns, and `send()` only pushes onto an
        //    unbounded channel drained by a different task, so time spent blocked in `write_all` —
        //    an exhausted SSH channel window, a slow link — is charged against the *response*
        //    budget and reported as a server timeout.
        match tokio::time::timeout(self.config.request_timeout, reply_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Err(Error::Eof),
            Err(_) => Err(Error::Timeout),
        }
    }

    /// Ends the session, **draining what is already queued** before stopping the writer.
    ///
    /// ⚠️ **Discarding the queue silently would make a teardown that returns quickly one that has
    /// queued its last packets, not sent them.** An earlier version aborted the writer
    /// first, throwing away every packet still in the channel; the one most likely to be there is
    /// the `SSH_FXP_CLOSE` that `list_dir` queues to release its handle.
    pub async fn close(mut self) {
        // Dropping the sender closes the channel, so the writer finishes the queue and exits on its
        // own. It is not aborted, so no packet is truncated part-way.
        self.outbound = None;
        if let Some(writer) = self.writer.take() {
            // Bounded: a wedged peer must not make teardown wait forever either.
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

/// Hand-written because neither task handle nor the channel is `Debug`. It shows what a reader of a
/// panic message actually needs — which server we are talking to and how much work is outstanding —
/// and deliberately not the stream.
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
        // ⚠️ **The reader is aborted and the writer is not**, and that asymmetry is the point.
        // Dropping the sender closes the channel, so the writer finishes its current packet and the
        // queue and then exits — aborting it here would be the one remaining way to leave half a
        // packet on the wire. The reader only reads, so stopping it mid-call costs nothing.
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

        // ⚠️ **The cause reaches everyone, not just the job that hit it.** An earlier version handed
        // the real `io::Error` to this one caller and synthesised a bare `BrokenPipe` — with no
        // `raw_os_error` — for everything queued behind it, which is indistinguishable in a log from
        // a real one. `Arc` is what makes one error reachable N times without `Error: Clone`.
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
        return Err(Error::TooLong { len: len as u64, limit: max as u64 });
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
    // ⚠️ **The reason the loop ended is carried to the waiters, not discarded.** An earlier draft
    // did `Err(_) => break` and then told everyone `Eof`, which made a `TooLong` refusal — the whole
    // point of the inbound ceiling — indistinguishable from a server hanging up.
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

        // The id is read before the body is decoded, so a packet this client cannot parse fails
        // **that one request** with a real reason instead of killing the session.
        let decoded = Response::decode(ty, &mut r);

        let waiter = lock(&pending).remove(&id);
        match waiter {
            Some(tx) => {
                let _ = tx.send(decoded);
            }
            // A reply nobody is waiting for: the request timed out and deregistered, or the server
            // is answering twice. Dropping it is right — the alternative is to tear down a session
            // over a packet that harms nothing.
            None => continue,
        }
    };

    // Published before the drain, so a request racing this either sees the flag and fails fast or is
    // already in the map and gets the cause below.
    reader_done.store(true, Ordering::Release);

    // ⚠️ **Every waiter gets the real cause, not just one of them.** An earlier version sent it to
    // `waiters.next()` and `Eof` to the rest — and `HashMap::drain` has no order, so *which* caller
    // learned the truth was nondeterministic between runs. `Eof` was also simply false for the cases
    // that matter: on a `TooLong` refusal the stream is not over, this client refused to continue.
    let cause = Arc::new(cause);
    let mut pending = lock(&pending);
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(Error::SessionEnded { cause: Arc::clone(&cause) }));
    }
}
