//! Writing a whole file with someone watching — `write_file_watched` — and pushing it with
//! `WriteFile`, and how much fits in one write (docs/map/territory/file-transfer.md,
//! docs/map/territory/transfer-lengths.md).
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

/// A server that answers `OPEN`, acknowledges every `WRITE`, and answers `CLOSE`, after a stop too.
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

/// Dropping a `WriteFile` without `close` panics in a debug build.
#[tokio::test]
#[should_panic(expected = "close")]
async fn dropping_a_writer_without_closing_is_loud() {
    let (session, _seen) = connect().await;
    let open = session.write_file(b"/srv/out.txt").await.expect("open");
    drop(open);
}

/// A caller with nothing to send still opens, writes nothing, and closes.
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
    // The offset advances by what was written; a write is all or an error.
    assert_eq!(
        seen.writes().iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![0, 6]
    );
    assert_eq!(totals, vec![6, 5]);
}

/// The file is opened with `WRITE | CREATE | TRUNCATE`.
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

/// `Stop` is not `Done`: the result says it stopped, and counts what the server already took.
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

/// The handle is closed at the end.
#[tokio::test]
async fn the_handle_is_closed_at_the_end() {
    let (session, seen) = connect().await;
    let mut feed = feeder(vec![b"abc".to_vec()]);
    session
        .write_file_watched(b"/srv/out.txt", 8, &mut |a| feed(a))
        .await
        .expect("upload");

    assert!(seen.closed.load(Ordering::SeqCst));
    // Every slot reclaimed.
    assert_eq!(session.in_flight(), 0);
}

/// `next` is asked for the `chunk_len` the caller passed.
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
// The first two pin agreement between `max_chunk()` and `write()`; the third pins the value
// (docs/map/territory/verification.md).

/// A chunk of exactly `max_chunk()` bytes is accepted.
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

/// One byte more is refused.
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

    // The handle still comes back after a refusal.
    w.close().await.expect("close after a refusal");
}

/// `max_chunk()` is the outbound ceiling less the write header and this file's handle, checked
/// against the encoder's arithmetic.
#[tokio::test]
async fn the_ceiling_leaves_room_for_this_file_s_handle() {
    let (session, _seen) = connect().await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");

    // A 4-byte handle, as OpenSSH's. 25 = length prefix(4) + type(1) + id(4) + handle length
    // prefix(4) + offset(8) + data length prefix(4).
    let ceiling = w.max_chunk();
    // Closed although nothing was written; `WriteFile` asserts on it.
    w.close().await.expect("close");

    assert_eq!(
        ceiling,
        Config::default().max_outbound_packet - 25 - 4,
        "the ceiling stopped tracking the header or the handle"
    );
}
