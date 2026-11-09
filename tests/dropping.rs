//! Letting go of an open file or walk without finishing: each path sends one `CLOSE`, and the session
//! goes on (docs/map/invariant/every-handle-is-closed.md).
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use justsftp::{Config, Error, Feed, OpenFlags, Session, Walk};

/// `SSH_FXP_VERSION`, version 3, no extensions.
const VERSION_REPLY: &[u8] = &[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x03];

const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const OPENDIR: u8 = 11;
const READDIR: u8 = 12;
const STAT: u8 = 17;

/// A `HANDLE` reply's body, and a `CLOSE` request's: the string `[0xAB]`.
const HANDLE_BODY: &[u8] = &[0, 0, 0, 1, 0xAB];

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
    /// One packet: its type, its id and the rest of its body.
    async fn packet(&mut self) -> (u8, u32, Vec<u8>) {
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).await.expect("length prefix");
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        self.0.read_exact(&mut buf).await.expect("packet body");
        let id = u32::from_be_bytes(buf[1..5].try_into().unwrap());
        (buf[0], id, buf[5..].to_vec())
    }

    async fn frame(&mut self) -> (u8, u32) {
        let (kind, id, _) = self.packet().await;
        (kind, id)
    }

    /// Replies to `id` with the 1-byte handle `HANDLE_BODY` carries.
    async fn answer_handle(&mut self, id: u32) {
        let mut out = vec![0, 0, 0, 10, 102];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(HANDLE_BODY);
        self.0.write_all(&out).await.unwrap();
    }

    /// Replies to `id` with a `STATUS` of the given code.
    async fn answer_status(&mut self, id: u32, code: u8) {
        let mut out = vec![0, 0, 0, 17, 101];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, code, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.0.write_all(&out).await.unwrap();
    }

    /// Answers an `OPEN` or `OPENDIR` with a 1-byte handle.
    async fn open(&mut self, kind: u8) {
        let (got, id) = self.frame().await;
        assert_eq!(got, kind);
        self.answer_handle(id).await;
    }

    /// Takes packets until nothing arrives for a second of virtual time, answering each `CLOSE`, and
    /// returns their kinds and bodies in order.
    async fn settle_packets(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut packets = Vec::new();
        while let Ok((kind, id, body)) =
            tokio::time::timeout(Duration::from_secs(1), self.packet()).await
        {
            packets.push((kind, body));
            if kind == CLOSE {
                self.answer_status(id, 0).await;
            }
        }
        packets
    }

    /// Takes frames until nothing arrives for a second of virtual time, answering each `CLOSE`, and
    /// returns their kinds in order.
    async fn settle(&mut self) -> Vec<u8> {
        let packets = self.settle_packets().await;
        packets.into_iter().map(|(kind, _)| kind).collect()
    }

    /// Answers one `STAT`, showing the session still pairs replies after what came before it, and
    /// returns its id.
    async fn answer_stat(&mut self) -> u32 {
        let (kind, id) = self.frame().await;
        assert_eq!(kind, STAT);
        let mut out = vec![0, 0, 0, 9, 105];
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        self.0.write_all(&out).await.unwrap();
        id
    }
}

async fn connect() -> (Session, Far) {
    connect_with(256 * 1024, Config::default()).await
}

async fn connect_with(pipe: usize, config: Config) -> (Session, Far) {
    let (client_side, server_side) = tokio::io::duplex(pipe);
    let mut far = Far(server_side);
    let handshake = async {
        far.frame().await;
        far.0.write_all(VERSION_REPLY).await.unwrap();
    };
    let (session, ()) = tokio::join!(Session::open(client_side, config), handshake);
    (session.expect("handshake"), far)
}

fn closes(kinds: &[u8]) -> usize {
    kinds.iter().filter(|k| **k == CLOSE).count()
}

/// The session still answers, and nothing is left registered.
async fn still_works(session: &Session, far: &mut Far) {
    let (stat, _) = tokio::join!(session.stat(b"/still"), far.answer_stat());
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

// ── an open nobody is left to answer ──

/// Exactly one `CLOSE` reaches the server, carrying the handle the reply brought.
async fn the_handle_is_closed_once(far: &mut Far) {
    let packets = far.settle_packets().await;
    assert_eq!(packets, [(CLOSE, HANDLE_BODY.to_vec())]);
}

#[tokio::test(start_paused = true)]
async fn an_open_dropped_before_its_reply_closes_the_handle() {
    let (session, mut far) = connect().await;
    let open = tokio::time::timeout(LET_GO, session.read_file(b"/f")).await;
    assert!(open.is_err(), "nothing was answered");
    far.open(OPEN).await;
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn an_opendir_dropped_before_its_reply_closes_the_handle() {
    let (session, mut far) = connect().await;
    let open = tokio::time::timeout(LET_GO, session.open_dir(b"/d")).await;
    assert!(open.is_err(), "nothing was answered");
    far.open(OPENDIR).await;
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn an_open_that_timed_out_closes_the_handle_of_its_late_reply() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(256 * 1024, config).await;
    let open = session.open_file(b"/f", OpenFlags::READ).await;
    assert!(matches!(open, Err(Error::Timeout)), "{open:?}");
    far.open(OPEN).await;
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn an_opendir_that_timed_out_closes_the_handle_of_its_late_reply() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(256 * 1024, config).await;
    let open = session.open_dir(b"/d").await;
    assert!(matches!(open, Err(Error::Timeout)), "{open:?}");
    far.open(OPENDIR).await;
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn an_opendir_whose_write_timed_out_closes_the_handle_of_its_late_reply() {
    let config = Config {
        write_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(8, config).await;
    let open = session.open_dir(&[b'x'; 2000]).await;
    assert!(matches!(open, Err(Error::WriteTimeout)), "{open:?}");
    far.open(OPENDIR).await;
    the_handle_is_closed_once(&mut far).await;
}

/// The packet outlives the 8-byte pipe's write budget, and the server still gets it whole.
#[tokio::test(start_paused = true)]
async fn an_open_whose_write_timed_out_closes_the_handle_of_its_late_reply() {
    let config = Config {
        write_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(8, config).await;
    let open = session.open_file(&[b'x'; 2000], OpenFlags::READ).await;
    assert!(matches!(open, Err(Error::WriteTimeout)), "{open:?}");
    far.open(OPEN).await;
    the_handle_is_closed_once(&mut far).await;
}

/// The reader has handed the reply over, and the caller goes away without reading it.
#[tokio::test(start_paused = true)]
async fn a_handle_delivered_but_never_read_is_closed() {
    let (session, mut far) = connect().await;
    let mut open = Box::pin(session.open_file(b"/f", OpenFlags::READ));
    assert!(tokio::time::timeout(LET_GO, &mut open).await.is_err());
    far.open(OPEN).await;
    while session.in_flight() > 0 {
        tokio::task::yield_now().await;
    }
    drop(open);
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

/// A second reply to an abandoned open finds nothing left to close.
#[tokio::test(start_paused = true)]
async fn an_abandoned_open_is_closed_once_when_answered_twice() {
    let (session, mut far) = connect().await;
    let open = tokio::time::timeout(LET_GO, session.open_file(b"/f", OpenFlags::READ)).await;
    assert!(open.is_err(), "nothing was answered");
    let (_, id) = far.frame().await;
    far.answer_handle(id).await;
    far.answer_handle(id).await;
    the_handle_is_closed_once(&mut far).await;
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_open_the_server_refused_sends_no_close() {
    let (session, mut far) = connect().await;
    let open = tokio::time::timeout(LET_GO, session.open_file(b"/f", OpenFlags::READ)).await;
    assert!(open.is_err(), "nothing was answered");
    let (_, id) = far.frame().await;
    far.answer_status(id, 4).await;
    assert_eq!(far.settle().await, [0u8; 0]);
    still_works(&session, &mut far).await;
}

/// A repeat of a reply already handed to its caller must not close the handle in the caller's hands.
#[tokio::test(start_paused = true)]
async fn a_repeated_handle_for_a_completed_open_sends_no_close() {
    let (session, mut far) = connect().await;
    let server = async {
        let (_, id) = far.frame().await;
        far.answer_handle(id).await;
        id
    };
    let (open, id) = tokio::join!(session.open_file(b"/f", OpenFlags::READ), server);
    open.unwrap();
    far.answer_handle(id).await;
    assert_eq!(far.settle().await, [0u8; 0]);
    still_works(&session, &mut far).await;
}

#[tokio::test(start_paused = true)]
async fn a_handle_nobody_asked_for_sends_no_close() {
    let (session, mut far) = connect().await;
    far.answer_handle(999).await;
    assert_eq!(far.settle().await, [0u8; 0]);
    still_works(&session, &mut far).await;
}

/// Only an open is remembered: a `HANDLE` answering anything else closes nothing.
#[tokio::test(start_paused = true)]
async fn a_handle_answering_a_dropped_stat_sends_no_close() {
    let (session, mut far) = connect().await;
    assert!(tokio::time::timeout(LET_GO, session.stat(b"/s"))
        .await
        .is_err());
    let (kind, id) = far.frame().await;
    assert_eq!(kind, STAT);
    far.answer_handle(id).await;
    assert_eq!(far.settle().await, [0u8; 0]);
    still_works(&session, &mut far).await;
}

/// An open refused before it was sent is not waiting for any reply.
#[tokio::test(start_paused = true)]
async fn an_open_refused_for_length_is_not_remembered() {
    let (session, mut far) = connect().await;
    let open = session
        .open_file(&vec![b'x'; 300_000], OpenFlags::READ)
        .await;
    assert!(matches!(open, Err(Error::TooLong { .. })), "{open:?}");
    let (_, id) = tokio::join!(session.stat(b"/s"), far.answer_stat());
    far.answer_handle(id - 1).await;
    assert_eq!(far.settle().await, [0u8; 0]);
    still_works(&session, &mut far).await;
}

/// The reader's way of queueing a `CLOSE` does not hold the writer's queue open.
#[tokio::test(start_paused = true)]
async fn closing_the_session_does_not_wait_for_the_write_budget() {
    let (session, mut far) = connect().await;
    let open = tokio::time::timeout(LET_GO, session.open_file(b"/f", OpenFlags::READ)).await;
    assert!(open.is_err(), "nothing was answered");
    far.open(OPEN).await;
    the_handle_is_closed_once(&mut far).await;

    let closing = tokio::time::Instant::now();
    session.close().await;
    assert!(
        closing.elapsed() < Duration::from_secs(1),
        "took {:?}",
        closing.elapsed()
    );
}

// ── `close` itself ──

/// An open answered and closed normally sends one `CLOSE`, with nothing left to close it again.
#[tokio::test(start_paused = true)]
async fn a_file_opened_and_closed_normally_sends_one_close() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open(OPEN));
    let (closed, packets) = tokio::join!(r.unwrap().close(), far.settle());
    closed.unwrap();
    assert_eq!(packets, [CLOSE]);
    still_works(&session, &mut far).await;
}

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
