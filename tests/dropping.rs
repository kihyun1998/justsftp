//! Letting go of an open file or walk without finishing: each path sends one `CLOSE`, and the session
//! goes on (docs/map/invariant/every-handle-is-closed.md).
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use justsftp::{Config, Feed, Session, Walk};

/// `SSH_FXP_VERSION`, version 3, no extensions.
const VERSION_REPLY: &[u8] = &[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x03];

const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const STAT: u8 = 17;

/// How long a client here waits before letting go, shorter than `Far::settle`'s.
const LET_GO: Duration = Duration::from_millis(10);

fn depth(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

fn chunk() -> NonZeroU32 {
    NonZeroU32::new(100).unwrap()
}

struct Far(DuplexStream);

impl Far {
    async fn frame(&mut self) -> (u8, u32) {
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).await.expect("length prefix");
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        self.0.read_exact(&mut buf).await.expect("packet body");
        (buf[0], u32::from_be_bytes(buf[1..5].try_into().unwrap()))
    }

    /// Answers an `OPEN` or `OPENDIR` with a 1-byte handle.
    async fn open(&mut self, kind: u8) {
        let (got, id) = self.frame().await;
        assert_eq!(got, kind);
        let mut out = vec![0, 0, 0, 10, 102];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 1, 0xAB]);
        self.0.write_all(&out).await.unwrap();
    }

    /// Takes frames until nothing arrives for a second of virtual time, answering each `CLOSE`, and
    /// returns their kinds in order.
    async fn settle(&mut self) -> Vec<u8> {
        let mut kinds = Vec::new();
        while let Ok((kind, id)) = tokio::time::timeout(Duration::from_secs(1), self.frame()).await
        {
            kinds.push(kind);
            if kind == CLOSE {
                let mut out = vec![0, 0, 0, 17, 101];
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&[0; 12]);
                self.0.write_all(&out).await.unwrap();
            }
        }
        kinds
    }

    /// Answers one `STAT`, showing the session still pairs replies after what came before it.
    async fn answer_stat(&mut self) {
        let (kind, id) = self.frame().await;
        assert_eq!(kind, STAT);
        let mut out = vec![0, 0, 0, 9, 105];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        self.0.write_all(&out).await.unwrap();
    }
}

async fn connect() -> (Session, Far) {
    let (client_side, server_side) = tokio::io::duplex(256 * 1024);
    let mut far = Far(server_side);
    let handshake = async {
        far.frame().await;
        far.0.write_all(VERSION_REPLY).await.unwrap();
    };
    let (session, ()) = tokio::join!(Session::open(client_side, Config::default()), handshake);
    (session.expect("handshake"), far)
}

fn closes(kinds: &[u8]) -> usize {
    kinds.iter().filter(|k| **k == CLOSE).count()
}

/// The session still answers, and nothing is left registered.
async fn still_works(session: &Session, far: &mut Far) {
    let (stat, ()) = tokio::join!(session.stat(b"/still"), far.answer_stat());
    stat.expect("the session goes on");
    assert_eq!(session.in_flight(), 0);
}

// ── values dropped without `close` ──

#[tokio::test(start_paused = true)]
async fn a_dropped_read_file_sends_one_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    drop(r.unwrap());
    assert_eq!(far.settle().await, [CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_write_file_sends_one_close() {
    let (session, mut far) = connect().await;
    let (w, ()) = tokio::join!(session.write_file(b"/f"), far.open(OPEN));
    drop(w.unwrap());
    assert_eq!(far.settle().await, [CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_read_ahead_sends_one_close_after_what_it_queued() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(
        session.read_file_ahead(b"/f", chunk(), depth(3)),
        far.open(OPEN)
    );
    let mut r = r.unwrap();
    let waited = tokio::time::timeout(LET_GO, r.next()).await;
    assert!(waited.is_err(), "nothing was answered");
    drop(r);
    assert_eq!(far.settle().await, [READ, READ, READ, CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_write_behind_sends_one_close_after_what_it_queued() {
    let (session, mut far) = connect().await;
    let (w, ()) = tokio::join!(session.write_file_behind(b"/f", depth(3)), far.open(OPEN));
    let mut w = w.unwrap();
    w.write(&[0; 10]).await.unwrap();
    w.write(&[0; 10]).await.unwrap();
    drop(w);
    assert_eq!(far.settle().await, [WRITE, WRITE, CLOSE]);
    still_works(&session, &mut far).await;
}

// ── futures dropped part-way ──

#[tokio::test(start_paused = true)]
async fn a_dropped_watched_read_sends_one_close() {
    let (session, mut far) = connect().await;
    let mut go_on = |_: &[u8], _: u64| Walk::Continue;
    let client = async {
        let walk = session.read_file_watched(b"/f", 100, &mut go_on);
        assert!(tokio::time::timeout(LET_GO, walk).await.is_err());
    };
    let (_, ()) = tokio::join!(far.open(OPEN), client);
    assert_eq!(far.settle().await, [READ, CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_watched_write_sends_one_close() {
    let (session, mut far) = connect().await;
    let mut feed = |_: u32| Feed::Bytes(vec![0; 10]);
    let client = async {
        let walk = session.write_file_watched(b"/f", 10, &mut feed);
        assert!(tokio::time::timeout(LET_GO, walk).await.is_err());
    };
    let (_, ()) = tokio::join!(far.open(OPEN), client);
    assert_eq!(far.settle().await, [WRITE, CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_watched_read_ahead_sends_one_close() {
    let (session, mut far) = connect().await;
    let mut go_on = |_: &[u8], _: u64| Walk::Continue;
    let client = async {
        let walk = session.read_file_ahead_watched(b"/f", chunk(), depth(2), &mut go_on);
        assert!(tokio::time::timeout(LET_GO, walk).await.is_err());
    };
    let (_, ()) = tokio::join!(far.open(OPEN), client);
    assert_eq!(far.settle().await, [READ, READ, CLOSE]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_directory_walk_sends_one_close() {
    let (session, mut far) = connect().await;
    let mut go_on = |_: &[justsftp::DirEntry], _: usize| Walk::Continue;
    let client = async {
        let walk = session.list_dir_watched(b"/d", &mut go_on);
        assert!(tokio::time::timeout(LET_GO, walk).await.is_err());
    };
    let (_, ()) = tokio::join!(far.open(OPENDIR), client);
    assert_eq!(far.settle().await, [READDIR, CLOSE]);
    still_works(&session, &mut far).await;
}

// ── `close` itself ──

/// A `close` that sent its `CLOSE` and was then dropped does not send a second one.
#[tokio::test(start_paused = true)]
async fn a_close_dropped_after_sending_sends_no_second_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    let waited = tokio::time::timeout(LET_GO, r.unwrap().close()).await;
    assert!(waited.is_err(), "the reply has not been sent");
    assert_eq!(closes(&far.settle().await), 1);
    still_works(&session, &mut far).await;
}

/// A `close` never polled has done nothing, so dropping it is dropping the value.
#[tokio::test(start_paused = true)]
async fn a_close_never_polled_still_sends_one_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    drop(r.unwrap().close());
    assert_eq!(far.settle().await, [CLOSE]);
    still_works(&session, &mut far).await;
}

/// A `ReadAhead::close` dropped while it waits for what is due still gets `CLOSE` out.
#[tokio::test(start_paused = true)]
async fn a_read_ahead_close_dropped_while_waiting_sends_one_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(
        session.read_file_ahead(b"/f", chunk(), depth(2)),
        far.open(OPEN)
    );
    let mut r = r.unwrap();
    assert!(tokio::time::timeout(LET_GO, r.next()).await.is_err());
    assert!(tokio::time::timeout(LET_GO, r.close()).await.is_err());
    assert_eq!(far.settle().await, [READ, READ, CLOSE]);
    still_works(&session, &mut far).await;
}

/// A `WriteBehind::close` dropped while it waits for writes still gets `CLOSE` out.
#[tokio::test(start_paused = true)]
async fn a_write_behind_close_dropped_while_waiting_sends_one_close() {
    let (session, mut far) = connect().await;
    let (w, ()) = tokio::join!(session.write_file_behind(b"/f", depth(2)), far.open(OPEN));
    let mut w = w.unwrap();
    w.write(&[0; 10]).await.unwrap();
    assert!(tokio::time::timeout(LET_GO, w.close()).await.is_err());
    assert_eq!(far.settle().await, [WRITE, CLOSE]);
    still_works(&session, &mut far).await;
}

/// Letting go after the stream has gone is quiet: nothing to send and nothing to panic about.
#[tokio::test(start_paused = true)]
async fn dropping_after_the_stream_ended_is_quiet() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    let r = r.unwrap();
    drop(far);
    tokio::time::sleep(LET_GO).await;
    drop(r);
    assert_eq!(session.in_flight(), 0);
}

/// A drop needs no runtime: it only queues a packet.
#[tokio::test(start_paused = true)]
async fn a_drop_on_a_thread_with_no_runtime_sends_one_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    let r = r.unwrap();
    std::thread::scope(|threads| {
        threads.spawn(move || drop(r));
    });
    assert_eq!(far.settle().await, [CLOSE]);
    still_works(&session, &mut far).await;
}

/// A callback that panics lets go of the file on the way out.
#[tokio::test(start_paused = true)]
async fn a_panicking_callback_still_sends_one_close() {
    let (session, mut far) = connect().await;
    let session = std::sync::Arc::new(session);
    let task = tokio::spawn({
        let session = std::sync::Arc::clone(&session);
        async move {
            let mut panics = |_: &[u8], _: u64| -> Walk { panic!("the caller's own failure") };
            session.read_file_watched(b"/f", 100, &mut panics).await
        }
    });
    far.open(OPEN).await;
    let (kind, id) = far.frame().await;
    assert_eq!(kind, READ);
    let mut out = vec![0, 0, 0, 10, 103];
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 1, 0x5A]);
    far.0.write_all(&out).await.unwrap();
    assert!(task.await.is_err(), "the callback panicked");
    assert_eq!(far.settle().await, [CLOSE]);
    still_works(&session, &mut far).await;
}

/// The session dropped straight after the file: the writer still finishes its queue, `CLOSE` included.
#[tokio::test(start_paused = true)]
async fn a_close_goes_out_when_the_session_is_dropped_next() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    drop(r.unwrap());
    drop(session);
    let (kind, _) = far.frame().await;
    assert_eq!(kind, CLOSE);
}
