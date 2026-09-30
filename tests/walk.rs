//! Walking a directory with someone watching — `list_dir_watched` and `list_dir`
//! (docs/map/territory/listing.md). The replies are built by helpers, since what is graded is the
//! walk's control flow (docs/map/territory/verification.md).
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

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

/// `SSH_FXP_NAME` carrying `count` entries, named `<prefix><n>`, with no attribute flags set.
fn name_reply(id: u32, prefix: &str, count: u32) -> Vec<u8> {
    let mut body = vec![104u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&count.to_be_bytes());
    for i in 0..count {
        let filename = format!("{prefix}{i}");
        let longname = format!("-rw-r--r-- {prefix}{i}");
        body.extend_from_slice(&string(filename.as_bytes()));
        body.extend_from_slice(&string(longname.as_bytes()));
        body.extend_from_slice(&0u32.to_be_bytes()); // attribute flags: none
    }
    framed(body)
}

/// `SSH_FXP_STATUS`. Code 1 is `EOF`, which is how a directory walk ends; 0 is `OK`.
fn status_reply(id: u32, code: u32) -> Vec<u8> {
    let mut body = vec![101u8];
    body.extend_from_slice(&id.to_be_bytes());
    body.extend_from_slice(&code.to_be_bytes());
    body.extend_from_slice(&string(b""));
    body.extend_from_slice(&string(b"en"));
    framed(body)
}

/// What the fake server observed, so a test can assert on the far side as well as the near one.
#[derive(Default)]
struct Observed {
    readdirs: AtomicUsize,
    closed: AtomicBool,
}

/// A server that answers `OPENDIR`, then `batches` full `READDIR` replies of `per_batch` entries
/// named `b<batch>-f<n>`, then `EOF` — and answers `CLOSE` whenever it arrives, after `EOF` too.
fn spawn_server(
    mut side: tokio::io::DuplexStream,
    batches: usize,
    per_batch: u32,
) -> Arc<Observed> {
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
                11 => side.write_all(&handle_reply(id)).await.unwrap(),
                12 => {
                    let n = seen.readdirs.fetch_add(1, Ordering::SeqCst);
                    let reply = if n < batches {
                        name_reply(id, &format!("b{n}-f"), per_batch)
                    } else {
                        status_reply(id, 1) // EOF
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

async fn connect(batches: usize, per_batch: u32) -> (Session, Arc<Observed>) {
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    let seen = spawn_server(server_side, batches, per_batch);
    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    (session, seen)
}

#[tokio::test]
async fn a_watched_walk_reports_after_every_batch_and_the_counts_are_cumulative() {
    let (session, _seen) = connect(3, 4).await;

    let mut seen_counts = Vec::new();
    let listing = session
        .list_dir_watched(b"/var/log", &mut |_, so_far| {
            seen_counts.push(so_far);
            Walk::Continue
        })
        .await
        .expect("listing");

    // Three batches of four; each report is the running total.
    assert_eq!(seen_counts, vec![4, 8, 12]);
    assert_eq!(listing.entries.len(), 12);
    assert!(!listing.stopped);
}

#[tokio::test]
async fn a_watched_walk_hands_on_each_batch_as_it_lands() {
    let (session, _seen) = connect(3, 4).await;

    let mut batches: Vec<Vec<String>> = Vec::new();
    session
        .list_dir_watched(b"/var/log", &mut |batch, _| {
            batches.push(
                batch
                    .iter()
                    .map(|e| String::from_utf8_lossy(&e.filename).into_owned())
                    .collect(),
            );
            Walk::Continue
        })
        .await
        .expect("listing");

    // Each call carries only what that round trip brought, never the running whole.
    let batch = |b: usize| {
        (0..4)
            .map(|i| format!("b{b}-f{i}"))
            .collect::<Vec<String>>()
    };
    assert_eq!(batches, vec![batch(0), batch(1), batch(2)]);
}

#[tokio::test]
async fn stopping_keeps_what_was_read_and_says_it_is_a_prefix() {
    let (session, _seen) = connect(5, 10).await;

    let listing = session
        .list_dir_watched(b"/var/log", &mut |_, so_far| {
            if so_far >= 20 {
                Walk::Stop
            } else {
                Walk::Continue
            }
        })
        .await
        .expect("listing");

    assert_eq!(listing.entries.len(), 20);
    // The flag tells "the folder holds 20" from "I stopped at 20".
    assert!(listing.stopped);
}

#[tokio::test]
async fn stopping_stops_asking() {
    // No `READDIR` goes out after the stop.
    let (session, seen) = connect(50, 10).await;

    session
        .list_dir_watched(b"/var/log", &mut |_, so_far| {
            if so_far >= 20 {
                Walk::Stop
            } else {
                Walk::Continue
            }
        })
        .await
        .expect("listing");

    assert_eq!(
        seen.readdirs.load(Ordering::SeqCst),
        2,
        "asked for more batches after Stop"
    );
}

#[tokio::test]
async fn a_stopped_walk_still_closes_the_handle() {
    // `CLOSE` goes out after a stop (docs/map/invariant/every-handle-is-closed.md).
    let (session, seen) = connect(50, 10).await;

    session
        .list_dir_watched(b"/var/log", &mut |_, _| Walk::Stop)
        .await
        .expect("listing");

    assert!(
        seen.closed.load(Ordering::SeqCst),
        "the handle was left open"
    );
}

#[tokio::test]
async fn an_empty_directory_reports_nothing_and_is_not_a_stop() {
    let (session, _seen) = connect(0, 0).await;

    let mut called = 0usize;
    let listing = session
        .list_dir_watched(b"/empty", &mut |_, _| {
            called += 1;
            Walk::Continue
        })
        .await
        .expect("listing");

    // EOF on the first READDIR: there is no batch, so there is nothing to report.
    assert_eq!(called, 0);
    assert!(listing.entries.is_empty());
    assert!(!listing.stopped);
}

#[tokio::test]
async fn list_dir_is_the_watched_walk_with_nobody_watching() {
    // `list_dir` is the same loop, so the same round trips and the same close.
    let (session, seen) = connect(3, 4).await;

    let entries = session.list_dir(b"/var/log").await.expect("listing");

    assert_eq!(entries.len(), 12);
    assert!(seen.closed.load(Ordering::SeqCst));
}
