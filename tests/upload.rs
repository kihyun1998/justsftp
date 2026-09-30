//! Writing a whole file **with someone watching** — the other half of what a transfer needs
//! (the-explorer-opens-two-kinds-of-folder 13).
//!
//! The mirror of `download.rs`, and the same argument for where the loop lives: the handle is
//! closed whichever way the write ends, a server has a finite number of open handles, and a caller
//! that drove `open_file`/`write` itself in order to insert a cancel check would have to reproduce
//! that discipline.
//!
//! # One asymmetry, and it is not cosmetic
//!
//! ⚠️ **There is no short write.** `SSH_FXP_READ` may answer with fewer bytes than asked for, which
//! is why `download.rs` has a whole test about not mistaking that for the end of the file.
//! `SSH_FXP_WRITE` answers `SSH_FXP_STATUS` — it either wrote everything or it failed. So the
//! upload loop has no partial-progress case to get wrong, and the offset advances by exactly what
//! was handed over.
//!
//! # Why the caller is asked for bytes rather than handing them in
//!
//! A transfer's source may be a local file, another server, or something not yet written. The crate
//! owns the loop (for the handle) and asks for the next chunk; what produces the bytes is the
//! caller's.
//! [`Feed`] is three answers rather than an `Option` because "no more bytes" and "stop, the user
//! cancelled" are different outcomes and the caller must not have to encode one as the other.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use justsftp::{Config, Feed, Session};

/// `SSH_FXP_VERSION`, version 3, no extensions.
const VERSION_REPLY: &[u8] = &[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x03];

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> (u8, Vec<u8>) {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.expect("length prefix");
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.expect("packet body");
    (buf[0], buf[1..].to_vec())
}

fn request_id(body: &[u8]) -> u32 {
    u32::from_be_bytes([body[0], body[1], body[2], body[3]])
}

fn framed(body: Vec<u8>) -> Vec<u8> {
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

fn handle_reply(id: u32) -> Vec<u8> {
    let mut body = vec![102u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&string(&[0x00, 0xFF, 0x80, 0x01]));
    framed(body)
}

/// `SSH_FXP_STATUS`. 0 is `OK`, which is the whole of a successful write's answer.
fn status_reply(id: u32, code: u32) -> Vec<u8> {
    let mut body = vec![101u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&code.to_be_bytes());
    body.extend_from_slice(&string(b""));
    body.extend_from_slice(&string(b"en"));
    framed(body)
}

/// A `SSH_FXP_WRITE` body: id(4) · handle(4+n) · offset(8) · data(4+n).
fn write_args(body: &[u8]) -> (u64, Vec<u8>) {
    let handle_len = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let rest = &body[8 + handle_len..];
    let offset = u64::from_be_bytes([
        rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7],
    ]);
    let n = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]) as usize;
    (offset, rest[12..12 + n].to_vec())
}

/// What the fake server observed.
#[derive(Default)]
struct Observed {
    /// `(offset, bytes)` of every `WRITE`, in order.
    writes: Mutex<Vec<(u64, Vec<u8>)>>,
    opened_flags: Mutex<Option<u32>>,
    closed: AtomicBool,
}

impl Observed {
    fn writes(&self) -> Vec<(u64, Vec<u8>)> {
        self.writes.lock().expect("observed writes").clone()
    }
    /// Everything written, in the order the server received it.
    fn assembled(&self) -> Vec<u8> {
        self.writes().into_iter().flat_map(|(_, b)| b).collect()
    }
}

/// A server that answers `OPEN`, acknowledges every `WRITE`, and answers `CLOSE`.
///
/// ⚠️ It keeps serving after the caller stops, for the same reason `download.rs`'s does: a stopped
/// upload sends `CLOSE` and a server that exited would hang the test rather than fail it.
fn spawn_server(mut side: tokio::io::DuplexStream) -> Arc<Observed> {
    let seen = Arc::new(Observed::default());
    let out = Arc::clone(&seen);
    tokio::spawn(async move {
        read_frame(&mut side).await; // INIT
        side.write_all(VERSION_REPLY).await.unwrap();
        loop {
            let Ok(frame) =
                tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut side))
                    .await
            else {
                return;
            };
            let (kind, body) = frame;
            let id = request_id(&body);
            match kind {
                3 => {
                    // OPEN: id(4) · path(4+n) · flags(4) · attrs
                    let path_len =
                        u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
                    let f = &body[8 + path_len..];
                    *seen.opened_flags.lock().expect("flags") =
                        Some(u32::from_be_bytes([f[0], f[1], f[2], f[3]]));
                    side.write_all(&handle_reply(id)).await.unwrap();
                }
                6 => {
                    seen.writes.lock().expect("writes").push(write_args(&body));
                    side.write_all(&status_reply(id, 0)).await.unwrap();
                }
                4 => {
                    seen.closed.store(true, Ordering::SeqCst);
                    side.write_all(&status_reply(id, 0)).await.unwrap();
                }
                other => panic!("unexpected request type {other}"),
            }
        }
    });
    out
}

async fn connect() -> (Session, Arc<Observed>) {
    let (client_side, server_side) = tokio::io::duplex(256 * 1024);
    let seen = spawn_server(server_side);
    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    (session, seen)
}

/// Hands out `chunks` in order, then `Done`.
fn feeder(chunks: Vec<Vec<u8>>) -> impl FnMut(u32) -> Feed {
    let mut it = chunks.into_iter();
    move |_| match it.next() {
        Some(b) => Feed::Bytes(b),
        None => Feed::Done,
    }
}

// ── the pushable writer ─────────────────────────────────────────────────────
//
// `write_file_watched` pulls through a **synchronous** `Feed`, which is enough when the source is a
// local file and not enough when the source is itself async — a remote-to-remote copy has to
// `await` the far side's read inside the loop. `WriteFile` is the same file open, pushed into.
// (The mirror of `ReadFile` in `download.rs`, and it exists for the mirrored reason.)

#[tokio::test]
async fn a_caller_can_push_chunks_one_at_a_time() {
    let (session, seen) = connect().await;

    let mut open = session.write_file(b"/srv/out.txt").await.expect("open");
    open.write(b"hello ").await.expect("write");
    open.write(b"world").await.expect("write");
    assert_eq!(open.written(), 11);
    open.close().await.expect("close");

    assert_eq!(seen.assembled(), b"hello world");
    assert_eq!(
        seen.writes().iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![0, 6],
        "the offset advances by what was handed over"
    );
    assert!(seen.closed.load(Ordering::SeqCst));
    assert_eq!(session.in_flight(), 0);
}

/// ⚠️ The accepted cost, made loud — the same one `ReadFile` carries. `Drop` cannot close, so it
/// refuses to be quiet instead.
#[tokio::test]
#[should_panic(expected = "close")]
async fn dropping_a_writer_without_closing_is_loud() {
    let (session, _seen) = connect().await;
    let open = session.write_file(b"/srv/out.txt").await.expect("open");
    drop(open);
}

/// The degenerate case, first: a caller with nothing to send still opens, writes nothing, and
/// closes. An upload of an empty file is an ordinary thing to ask for.
#[tokio::test]
async fn an_empty_file_writes_nothing_and_still_closes() {
    let (session, seen) = connect().await;

    let upload = session
        .write_file_watched(b"/srv/out.bin", 8, &mut |asked| {
            assert_eq!(asked, 8, "the caller is told how much will fit");
            Feed::Done
        })
        .await
        .expect("upload");

    assert_eq!(upload.bytes, 0);
    assert!(!upload.stopped);
    assert!(seen.writes().is_empty(), "nothing was sent");
    assert!(seen.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_bytes_arrive_in_order_at_advancing_offsets() {
    let (session, seen) = connect().await;
    let mut feed = feeder(vec![b"hello ".to_vec(), b"world".to_vec()]);

    let mut totals = Vec::new();
    let upload = session
        .write_file_watched(b"/srv/out.txt", 8, &mut |asked| {
            let f = feed(asked);
            if let Feed::Bytes(ref b) = f {
                totals.push(b.len());
            }
            f
        })
        .await
        .expect("upload");

    assert_eq!(upload.bytes, 11);
    assert!(!upload.stopped);
    assert_eq!(seen.assembled(), b"hello world");
    // ⚠️ The offset advances by what was **written**, and a write is all-or-error — there is no
    // short write to account for (see this file's header).
    assert_eq!(
        seen.writes().iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![0, 6]
    );
    assert_eq!(totals, vec![6, 5]);
}

/// The file is created and emptied, not appended to. Opening without `TRUNCATE` leaves the tail of
/// a longer previous file behind, which is a corrupt destination that reports success.
#[tokio::test]
async fn the_destination_is_created_and_truncated() {
    let (session, seen) = connect().await;
    let mut feed = feeder(vec![b"x".to_vec()]);
    session
        .write_file_watched(b"/srv/out.txt", 8, &mut |a| feed(a))
        .await
        .expect("upload");

    let flags = seen.opened_flags.lock().expect("flags").expect("open seen");
    assert_eq!(flags & 0x0000_0002, 0x0000_0002, "WRITE");
    assert_eq!(flags & 0x0000_0008, 0x0000_0008, "CREATE");
    assert_eq!(flags & 0x0000_0010, 0x0000_0010, "TRUNCATE");
}

/// `Stop` is not `Done`: the caller cancelled, and the result says so rather than looking like a
/// complete file. The bytes already accepted stay accepted — the crate cannot unwrite them.
#[tokio::test]
async fn stopping_says_so_and_keeps_what_the_server_already_took() {
    let (session, seen) = connect().await;
    let mut sent = 0;
    let upload = session
        .write_file_watched(b"/srv/out.bin", 4, &mut |_| {
            sent += 1;
            if sent <= 2 {
                Feed::Bytes(vec![b'a'; 4])
            } else {
                Feed::Stop
            }
        })
        .await
        .expect("upload");

    assert!(upload.stopped, "a stop is not a completed file");
    assert_eq!(upload.bytes, 8);
    assert_eq!(seen.writes().len(), 2, "the stop stops asking");
    assert!(seen.closed.load(Ordering::SeqCst));
}

/// A server has a finite number of open handles; leaking one per upload exhausts them.
#[tokio::test]
async fn the_handle_is_closed_at_the_end() {
    let (session, seen) = connect().await;
    let mut feed = feeder(vec![b"abc".to_vec()]);
    session
        .write_file_watched(b"/srv/out.txt", 8, &mut |a| feed(a))
        .await
        .expect("upload");

    assert!(seen.closed.load(Ordering::SeqCst));
    // Steady state is zero — every request reclaimed its slot in the pending map.
    assert_eq!(session.in_flight(), 0);
}

/// The caller is told how much fits, so it can read exactly that much from its source rather than
/// guessing and having the crate split it again.
#[tokio::test]
async fn the_caller_is_asked_for_the_length_that_fits() {
    let (session, _seen) = connect().await;
    let mut asked = Vec::new();
    let mut n = 0;
    session
        .write_file_watched(b"/srv/out.bin", 1234, &mut |len| {
            asked.push(len);
            n += 1;
            if n <= 2 {
                Feed::Bytes(vec![0u8; 10])
            } else {
                Feed::Done
            }
        })
        .await
        .expect("upload");

    assert!(asked.iter().all(|a| *a == 1234), "got {asked:?}");
}

// ── How much fits in one packet ──────────────────────────────────────────────
//
// ⚠️ **A caller driving its own loop has no way to know this, and one did the arithmetic wrong.**
// `explorer_transfer` picked 256 KiB — the same number as `max_outbound_packet` — so every full
// chunk overflowed by the header and `Session::write` refused it. Measured: an upload of any file
// larger than one chunk had never worked. The read side never had this bug because
// `max_read_len()` already existed; the write side had no twin.
//
// ⚠️ **The three below do not all measure the same thing, and it took a mutation to see it.** The
// first two ask `max_chunk()` for a number and then check `write()` against that same number, so
// they pin **agreement between the two** — a `write()` whose limit drifted away from what
// `max_chunk()` advertises. They cannot see a `max_chunk()` that is simply wrong: measured, a
// ceiling one byte low leaves both of them green.
//
// The third is the one that pins the **value**, against the encoder's arithmetic rather than a copy
// of the number. Mutating the ceiling by one in either direction reddens it and nothing else.

/// The largest chunk the crate advertises really does fit — and it is not a smaller number that
/// happens to fit.
#[tokio::test]
async fn a_chunk_at_the_advertised_ceiling_is_accepted() {
    let (session, seen) = connect().await;
    let mut w = session.write_file(b"/srv/out.bin").await.expect("open");

    let n = w.max_chunk();
    w.write(&vec![0xAB; n])
        .await
        .expect("a chunk at the ceiling must fit");
    w.close().await.expect("close");

    assert_eq!(
        seen.assembled().len(),
        n,
        "the server did not receive the whole chunk"
    );
}

/// One byte more does not. Without this, an off-by-one that made `max_chunk()` *smaller* would go
/// unnoticed — and "smaller" is the direction a careless fix goes.
#[tokio::test]
async fn one_byte_past_the_ceiling_is_refused() {
    let (session, _seen) = connect().await;
    let mut w = session.write_file(b"/srv/out.bin").await.expect("open");

    let n = w.max_chunk();
    let refused = w.write(&vec![0xAB; n + 1]).await;
    assert!(
        matches!(refused, Err(justsftp::Error::TooLong { .. })),
        "expected TooLong, got {refused:?}"
    );

    // ⚠️ The handle still has to come back. A caller that hits the ceiling and then gives up is
    // exactly the shape that leaked one in a consumer's transfer loop.
    w.close().await.expect("close after a refusal");
}

/// ⚠️ **The ceiling is the *packet*, not the payload, and this is what says so.**
///
/// `max_chunk()` could be written as a plain constant and both tests above would still pass. What
/// they cannot see is whether it tracks the **handle**, which the server chooses per file and may
/// make longer. This one reads the number the crate answers and checks it against the encoder's own
/// arithmetic rather than against a copy of it.
#[tokio::test]
async fn the_ceiling_leaves_room_for_this_file_s_handle() {
    let (session, _seen) = connect().await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");

    // `spawn_server` hands out a 4-byte handle, the same length OpenSSH uses.
    // 25 = length prefix(4) + type(1) + id(4) + handle length prefix(4) + offset(8) + data length
    // prefix(4). `protocol.rs`'s encoding test pins that shape.
    let ceiling = w.max_chunk();
    // The handle comes back even though this test writes nothing — `WriteFile` asserts on it, and
    // that assertion caught this very test forgetting.
    w.close().await.expect("close");

    assert_eq!(
        ceiling,
        Config::default().max_outbound_packet - 25 - 4,
        "the ceiling stopped tracking the header or the handle"
    );
}
