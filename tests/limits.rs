//! Asking the server for its limits — `limits@openssh.com` (docs/map/territory/transfer-lengths.md).
//!
//! The fake server here advertises the extension in `SSH_FXP_VERSION` and answers the request with
//! whatever four numbers a test gives it. Everything is observed from outside: what the public API
//! reports, and what the server actually received.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use justsftp::{Config, Session};

const LIMITS: &[u8] = b"limits@openssh.com";

/// The four `uint64` fields of the reply, in wire order (OpenSSH `PROTOCOL` § 4.8).
#[derive(Clone, Copy)]
struct Limits {
    packet: u64,
    read: u64,
    write: u64,
    handles: u64,
}

/// How the fake server answers the `limits@` request.
#[derive(Clone)]
enum Answer {
    Reply(Limits),
    /// `SSH_FXP_STATUS` with `OP_UNSUPPORTED`.
    Status,
    /// An `EXTENDED_REPLY` whose body stops part-way through the second field.
    Truncated,
    /// Never answers.
    Silent,
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> (u8, Vec<u8>) {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.expect("length prefix");
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.expect("packet body");
    (buf[0], buf[1..].to_vec())
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

fn version_reply(advertise: bool) -> Vec<u8> {
    let mut body = vec![2u8];
    body.extend_from_slice(&3u32.to_be_bytes());
    if advertise {
        body.extend_from_slice(&string(LIMITS));
        body.extend_from_slice(&string(b"1"));
    }
    framed(body)
}

fn status_reply(id: u32, code: u32) -> Vec<u8> {
    let mut body = vec![101u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&code.to_be_bytes());
    body.extend_from_slice(&string(b""));
    body.extend_from_slice(&string(b"en"));
    framed(body)
}

fn handle_reply(id: u32) -> Vec<u8> {
    let mut body = vec![102u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&string(&[0x00, 0xFF, 0x80, 0x01]));
    framed(body)
}

fn request_id(body: &[u8]) -> u32 {
    u32::from_be_bytes([body[0], body[1], body[2], body[3]])
}

/// Every packet the server received after `INIT`, as `(type, body)`.
type Seen = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

/// Answers `limits@` as told, `OPEN` with a 4-byte handle, `WRITE`/`CLOSE` with `OK`, and `READ`
/// with `EOF`. Keeps serving so a caller's `CLOSE` never hangs the test.
fn spawn_server(mut side: tokio::io::DuplexStream, advertise: bool, answer: Answer) -> Seen {
    let seen: Seen = Arc::default();
    let out = Arc::clone(&seen);
    tokio::spawn(async move {
        read_frame(&mut side).await; // INIT
        side.write_all(&version_reply(advertise)).await.unwrap();
        loop {
            let Ok((kind, body)) =
                tokio::time::timeout(Duration::from_secs(5), read_frame(&mut side)).await
            else {
                return;
            };
            seen.lock().unwrap().push((kind, body.clone()));
            let id = request_id(&body);
            let reply = match (kind, &answer) {
                (200, Answer::Reply(l)) => {
                    let mut b = vec![201u8];
                    b.extend_from_slice(&id.to_be_bytes());
                    for v in [l.packet, l.read, l.write, l.handles] {
                        b.extend_from_slice(&v.to_be_bytes());
                    }
                    framed(b)
                }
                (200, Answer::Status) => status_reply(id, 8),
                (200, Answer::Truncated) => {
                    let mut b = vec![201u8];
                    b.extend_from_slice(&id.to_be_bytes());
                    b.extend_from_slice(&262_144u64.to_be_bytes());
                    b.extend_from_slice(&[0, 0, 0]);
                    framed(b)
                }
                (200, Answer::Silent) => continue,
                (3, _) => handle_reply(id),
                (5, _) => status_reply(id, 1),
                (4 | 6, _) => status_reply(id, 0),
                (other, _) => panic!("unexpected request type {other}"),
            };
            side.write_all(&reply).await.unwrap();
        }
    });
    out
}

fn config() -> Config {
    Config {
        request_timeout: Duration::from_secs(2),
        ..Config::default()
    }
}

async fn connect(advertise: bool, answer: Answer) -> (Session, Seen) {
    let (client_side, server_side) = tokio::io::duplex(1024 * 1024);
    let seen = spawn_server(server_side, advertise, answer);
    let session = Session::open(client_side, config())
        .await
        .expect("handshake");
    (session, seen)
}

fn limits(packet: u64, read: u64, write: u64) -> Answer {
    Answer::Reply(Limits {
        packet,
        read,
        write,
        handles: 0,
    })
}

/// The chunk a default session hands out for a 4-byte handle: 262,144 − 25 − 4.
const DEFAULT_CHUNK: usize = 262_115;
/// The read length a default session hands out: 262,144 − 9.
const DEFAULT_READ: u32 = 262_135;

#[tokio::test]
async fn a_smaller_write_length_lowers_the_chunk() {
    let (session, _seen) = connect(true, limits(0, 0, 40_000)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    assert_eq!(chunk, 40_000);
}

/// Data lengths of every `SSH_FXP_WRITE` the server received.
fn write_lens(seen: &Seen) -> Vec<usize> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(kind, _)| *kind == 6)
        .map(|(_, body)| {
            // id(4) · handle(4+4) · offset(8) · data length(4)
            u32::from_be_bytes([body[20], body[21], body[22], body[23]]) as usize
        })
        .collect()
}

#[tokio::test]
async fn a_smaller_packet_length_alone_lowers_the_chunk_by_the_write_header() {
    let (session, _seen) = connect(true, limits(50_000, 0, 0)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    // 50,000 − 25 (write header) − 4 (this server's handle)
    assert_eq!(chunk, 49_971);
}

#[tokio::test]
async fn an_upload_in_advertised_chunks_never_sends_a_write_past_the_server_s_length() {
    let (session, seen) = connect(true, limits(0, 0, 40_000)).await;
    let payload = vec![0x5A; 100_000];

    let mut w = session.write_file(b"/srv/out.bin").await.expect("open");
    for piece in payload.chunks(w.max_chunk()) {
        w.write(piece).await.expect("write");
    }
    w.close().await.expect("close");

    assert_eq!(write_lens(&seen), vec![40_000, 40_000, 20_000]);
}

#[tokio::test]
async fn a_write_past_the_server_s_length_is_refused_before_it_is_sent() {
    let (session, seen) = connect(true, limits(0, 0, 40_000)).await;
    let mut w = session.write_file(b"/srv/out.bin").await.expect("open");

    let refused = w.write(&vec![0u8; 40_001]).await;
    w.close().await.expect("close");

    assert!(
        matches!(
            refused,
            Err(justsftp::Error::TooLong {
                len: 40_001,
                limit: 40_000
            })
        ),
        "expected TooLong, got {refused:?}"
    );
    assert!(
        write_lens(&seen).is_empty(),
        "the oversized write reached the server"
    );
}

#[tokio::test]
async fn a_smaller_read_length_lowers_the_read_length() {
    let (session, _seen) = connect(true, limits(0, 30_000, 0)).await;
    assert_eq!(session.max_read_len(), 30_000);
}

#[tokio::test]
async fn larger_limits_leave_both_lengths_where_they_were() {
    let (session, _seen) = connect(true, limits(1 << 30, 1 << 30, 1 << 30)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    assert_eq!(chunk, DEFAULT_CHUNK);
    assert_eq!(session.max_read_len(), DEFAULT_READ);
}

#[tokio::test]
async fn a_zero_field_is_no_limit_rather_than_a_limit_of_zero() {
    let (session, _seen) = connect(true, limits(0, 0, 0)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    assert_eq!(chunk, DEFAULT_CHUNK);
    assert_eq!(session.max_read_len(), DEFAULT_READ);
}

#[tokio::test]
async fn a_tiny_limit_still_lets_a_transfer_make_progress() {
    let (session, _seen) = connect(true, limits(20, 10, 10)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    assert_eq!(chunk, 64);
    assert_eq!(session.max_read_len(), 64);
}

#[tokio::test]
async fn the_server_s_answer_is_reported_as_it_was_stated() {
    let answer = Limits {
        packet: 34_000,
        read: 32_768,
        write: 32_000,
        handles: 1_019,
    };
    let (session, _seen) = connect(true, Answer::Reply(answer)).await;

    let got = session.server_limits().expect("limits were answered");
    assert_eq!(
        (
            got.max_packet_len,
            got.max_read_len,
            got.max_write_len,
            got.max_open_handles
        ),
        (34_000, 32_768, 32_000, 1_019)
    );
}

#[tokio::test]
async fn a_server_that_does_not_advertise_it_is_never_asked() {
    let (session, seen) = connect(false, limits(0, 1, 1)).await;
    // One ordinary round trip, so a request sent at open would already be in `seen`.
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    let kinds: Vec<u8> = seen.lock().unwrap().iter().map(|(k, _)| *k).collect();
    assert_eq!(kinds, vec![3, 4], "only OPEN and CLOSE were sent");
    assert_eq!(chunk, DEFAULT_CHUNK);
    assert_eq!(session.max_read_len(), DEFAULT_READ);
    assert!(session.server_limits().is_none());
}

#[tokio::test]
async fn the_request_names_the_extension_and_carries_nothing_else() {
    let (session, seen) = connect(true, limits(0, 0, 0)).await;
    drop(session);

    let sent = seen.lock().unwrap();
    let (kind, body) = sent.first().expect("a request was sent");
    assert_eq!(*kind, 200);
    // id(4) · name length(4) · name
    assert_eq!(&body[4..8], &[0, 0, 0, 18]);
    assert_eq!(&body[8..], LIMITS);
}

async fn assert_falls_back(answer: Answer) {
    let (session, _seen) = connect(true, answer).await;
    let w = session
        .write_file(b"/srv/out.bin")
        .await
        .expect("the session is still usable");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    assert!(session.server_limits().is_none());
    assert_eq!(chunk, DEFAULT_CHUNK);
    assert_eq!(session.max_read_len(), DEFAULT_READ);
}

#[tokio::test]
async fn a_status_answer_falls_back_to_the_defaults() {
    assert_falls_back(Answer::Status).await;
}

#[tokio::test]
async fn a_truncated_answer_falls_back_to_the_defaults() {
    assert_falls_back(Answer::Truncated).await;
}

#[tokio::test]
async fn no_answer_falls_back_to_the_defaults_once_the_request_times_out() {
    assert_falls_back(Answer::Silent).await;
}

#[tokio::test]
async fn the_tighter_of_the_packet_and_write_lengths_wins() {
    let (session, _seen) = connect(true, limits(50_000, 0, 200_000)).await;
    let w = session.write_file(b"/srv/out.bin").await.expect("open");
    let chunk = w.max_chunk();
    w.close().await.expect("close");

    // The packet bound (50,000 − 25 − 4), not the write length.
    assert_eq!(chunk, 49_971);
}

/// A server that advertises the extension and then hangs up has a dead link, not missing limits.
#[tokio::test]
async fn a_link_that_dies_during_the_query_fails_the_open() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        read_frame(&mut server_side).await; // INIT
        server_side.write_all(&version_reply(true)).await.unwrap();
        read_frame(&mut server_side).await; // the limits request
        drop(server_side);
    });

    let opened = Session::open(client_side, config()).await;
    assert!(
        opened.is_err(),
        "the open reported success over a closed link"
    );
}
