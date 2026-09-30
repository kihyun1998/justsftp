//! Reading a whole file with someone watching — `read_file_watched` — and pulling it with
//! `ReadFile` (docs/map/territory/file-transfer.md).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use justsftp::{Config, Session, Walk};

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

/// `SSH_FXP_HANDLE` carrying a four-byte handle.
fn handle_reply(id: u32) -> Vec<u8> {
    let mut body = vec![102u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&string(&[0x00, 0xFF, 0x80, 0x01]));
    framed(body)
}

/// `SSH_FXP_DATA` carrying `bytes`.
fn data_reply(id: u32, bytes: &[u8]) -> Vec<u8> {
    let mut body = vec![103u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&string(bytes));
    framed(body)
}

/// `SSH_FXP_STATUS`. Code 1 is `EOF`, which is how a read ends; 0 is `OK`.
fn status_reply(id: u32, code: u32) -> Vec<u8> {
    let mut body = vec![101u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&code.to_be_bytes());
    body.extend_from_slice(&string(b""));
    body.extend_from_slice(&string(b"en"));
    framed(body)
}

/// A `SSH_FXP_READ` body: id(4) · handle(4+n) · offset(8) · len(4).
fn read_args(body: &[u8]) -> (u64, u32) {
    let handle_len = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    let rest = &body[8 + handle_len..];
    let offset = u64::from_be_bytes([
        rest[0], rest[1], rest[2], rest[3], rest[4], rest[5], rest[6], rest[7],
    ]);
    let len = u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]);
    (offset, len)
}

/// What the fake server observed, so a test can assert on the far side as well as the near one.
#[derive(Default)]
struct Observed {
    /// `(offset, len)` of every `READ` that arrived, in order.
    reads: Mutex<Vec<(u64, u32)>>,
    closed: AtomicBool,
}

impl Observed {
    fn reads(&self) -> Vec<(u64, u32)> {
        self.reads.lock().expect("observed reads").clone()
    }
}

/// The byte at file offset `n`, position-dependent so a misplaced chunk shows.
fn byte_at(n: u64) -> u8 {
    (n % 251) as u8
}

/// A server that answers `OPEN`, then hands out `script[i]` bytes for the *i*-th `READ` **starting
/// at whatever offset was asked for**, then `EOF` — and answers `CLOSE` whenever it arrives, after
/// `EOF` too. A script entry smaller than the length requested is a short read.
fn spawn_server(mut side: tokio::io::DuplexStream, script: Vec<usize>) -> Arc<Observed> {
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
                3 => side.write_all(&handle_reply(id)).await.unwrap(),
                5 => {
                    let (offset, len) = read_args(&body);
                    let nth = {
                        let mut reads = seen.reads.lock().expect("observed reads");
                        reads.push((offset, len));
                        reads.len() - 1
                    };
                    let reply = match script.get(nth) {
                        Some(&n) => {
                            let chunk: Vec<u8> =
                                (0..n as u64).map(|i| byte_at(offset + i)).collect();
                            data_reply(id, &chunk)
                        }
                        None => status_reply(id, 1), // EOF
                    };
                    side.write_all(&reply).await.unwrap();
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

async fn connect(script: Vec<usize>) -> (Session, Arc<Observed>) {
    let (client_side, server_side) = tokio::io::duplex(256 * 1024);
    let seen = spawn_server(server_side, script);
    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    (session, seen)
}

/// The file the script above describes, assembled locally for comparison.
fn expected(total: u64) -> Vec<u8> {
    (0..total).map(byte_at).collect()
}

// ── the pull-able reader ────────────────────────────────────────────────────

#[tokio::test]
async fn a_caller_can_pull_the_file_one_chunk_at_a_time() {
    let (session, seen) = connect(vec![8, 8, 4]).await;

    let mut file = Vec::new();
    let mut open = session.read_file(b"/etc/nginx.conf").await.expect("open");
    while let Some(chunk) = open.next(8).await.expect("read") {
        file.extend_from_slice(&chunk);
    }
    open.close().await.expect("close");

    assert_eq!(file, expected(20));
    assert!(seen.closed.load(Ordering::SeqCst));
    assert_eq!(session.in_flight(), 0);
}

/// `Ok(None)` is the end; a short `Ok(Some(..))` is not.
#[tokio::test]
async fn a_short_pull_is_not_the_end_of_the_file() {
    let (session, _seen) = connect(vec![4, 8, 8]).await;

    let mut total = 0;
    let mut open = session.read_file(b"/etc/nginx.conf").await.expect("open");
    while let Some(chunk) = open.next(8).await.expect("read") {
        total += chunk.len();
    }
    open.close().await.expect("close");

    assert_eq!(total, 20, "a short chunk is not the end");
}

/// The offset is the reader's to track, and it advances by what arrived.
#[tokio::test]
async fn the_pulled_offsets_advance_by_what_arrived() {
    let (session, seen) = connect(vec![4, 8, 8]).await;

    let mut open = session.read_file(b"/etc/nginx.conf").await.expect("open");
    while open.next(8).await.expect("read").is_some() {}
    open.close().await.expect("close");

    let offsets: Vec<u64> = seen.reads().iter().map(|(o, _)| *o).collect();
    assert_eq!(offsets, vec![0, 4, 12, 20]);
}

/// Dropping a `ReadFile` without `close` panics in a debug build.
#[tokio::test]
#[should_panic(expected = "close")]
async fn dropping_without_closing_is_loud() {
    let (session, _seen) = connect(vec![8]).await;
    let open = session.read_file(b"/etc/nginx.conf").await.expect("open");
    drop(open);
}

#[tokio::test]
async fn a_whole_file_arrives_in_order_and_the_totals_are_cumulative() {
    let (session, _seen) = connect(vec![8, 8, 4]).await;

    let mut file = Vec::new();
    let mut totals = Vec::new();
    let download = session
        .read_file_watched(b"/etc/nginx.conf", 8, &mut |chunk, so_far| {
            file.extend_from_slice(chunk);
            totals.push(so_far);
            Walk::Continue
        })
        .await
        .expect("download");

    assert_eq!(file, expected(20));
    // The running total, not the chunk size.
    assert_eq!(totals, vec![8, 16, 20]);
    assert_eq!(download.bytes, 20);
    assert!(!download.stopped);
}

/// A short read is not the end of the file; only `Ok(None)` is.
#[tokio::test]
async fn a_short_read_is_not_the_end_of_the_file() {
    // The first chunk comes back at half the requested length, and there is more after it.
    let (session, _seen) = connect(vec![4, 8, 8]).await;

    let mut file = Vec::new();
    let download = session
        .read_file_watched(b"/etc/nginx.conf", 8, &mut |chunk, _| {
            file.extend_from_slice(chunk);
            Walk::Continue
        })
        .await
        .expect("download");

    assert_eq!(download.bytes, 20, "a short chunk is not the end");
    assert_eq!(file, expected(20));
}

/// Asserted on the far side: each `READ` asks from where the last one's data ended.
#[tokio::test]
async fn the_offset_advances_by_what_arrived_not_by_what_was_asked() {
    let (session, seen) = connect(vec![4, 8, 8]).await;

    session
        .read_file_watched(b"/etc/nginx.conf", 8, &mut |_, _| Walk::Continue)
        .await
        .expect("download");

    let offsets: Vec<u64> = seen.reads().iter().map(|(o, _)| *o).collect();
    assert_eq!(offsets, vec![0, 4, 12, 20]);
}

/// The chunk length on the wire is the one the caller passed, not `max_read_len()`.
#[tokio::test]
async fn the_requested_length_is_the_callers() {
    let (session, seen) = connect(vec![3, 3]).await;

    session
        .read_file_watched(b"/etc/nginx.conf", 3, &mut |_, _| Walk::Continue)
        .await
        .expect("download");

    assert!(
        seen.reads().iter().all(|(_, len)| *len == 3),
        "every READ asked for the caller's length, got {:?}",
        seen.reads()
    );
}

#[tokio::test]
async fn stopping_keeps_what_arrived_and_says_it_is_a_prefix() {
    let (session, _seen) = connect(vec![8, 8, 8, 8, 8]).await;

    let mut file = Vec::new();
    let download = session
        .read_file_watched(b"/var/log/big", 8, &mut |chunk, so_far| {
            file.extend_from_slice(chunk);
            if so_far >= 16 {
                Walk::Stop
            } else {
                Walk::Continue
            }
        })
        .await
        .expect("download");

    assert_eq!(download.bytes, 16);
    // The flag tells "the file is 16 bytes" from "I stopped at 16".
    assert!(download.stopped);
    assert_eq!(file, expected(16));
}

#[tokio::test]
async fn stopping_stops_asking() {
    // No `READ` goes out after the stop.
    let (session, seen) = connect(vec![8; 100]).await;

    session
        .read_file_watched(b"/var/log/big", 8, &mut |_, so_far| {
            if so_far >= 16 {
                Walk::Stop
            } else {
                Walk::Continue
            }
        })
        .await
        .expect("download");

    assert_eq!(seen.reads().len(), 2, "asked twice, then stopped");
}

/// The handle is closed on a stop.
#[tokio::test]
async fn the_handle_is_closed_on_a_stop() {
    let (session, seen) = connect(vec![8; 100]).await;

    session
        .read_file_watched(b"/var/log/big", 8, &mut |_, _| Walk::Stop)
        .await
        .expect("download");

    assert!(seen.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn the_handle_is_closed_at_the_end_of_the_file() {
    let (session, seen) = connect(vec![8, 2]).await;

    session
        .read_file_watched(b"/etc/nginx.conf", 8, &mut |_, _| Walk::Continue)
        .await
        .expect("download");

    assert!(seen.closed.load(Ordering::SeqCst));
    // Every slot reclaimed, including the last `EOF` read.
    assert_eq!(session.in_flight(), 0);
}

/// An empty file is `OPEN` then one `READ` answering `EOF`; the callback is never invoked.
#[tokio::test]
async fn an_empty_file_reads_no_chunks_and_still_closes() {
    let (session, seen) = connect(vec![]).await;

    let mut calls = 0;
    let download = session
        .read_file_watched(b"/etc/empty", 8, &mut |_, _| {
            calls += 1;
            Walk::Continue
        })
        .await
        .expect("download");

    assert_eq!(calls, 0);
    assert_eq!(download.bytes, 0);
    assert!(!download.stopped);
    assert!(seen.closed.load(Ordering::SeqCst));
}
