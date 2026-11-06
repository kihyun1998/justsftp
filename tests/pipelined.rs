//! Keeping several requests outstanding: `ReadAhead` and `WriteBehind`. Each test scripts the far
//! side by hand, so it decides when a reply lands and in what order
//! (docs/map/territory/file-transfer.md).
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};

use justsftp::{Config, Error, Session, StatusCode, Walk};

/// `SSH_FXP_VERSION`, version 3, no extensions.
const VERSION_REPLY: &[u8] = &[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x03];

const OPEN: u8 = 3;
const CLOSE: u8 = 4;
const READ: u8 = 5;
const WRITE: u8 = 6;
const STAT: u8 = 17;

const FX_OK: u32 = 0;
const FX_EOF: u32 = 1;
const FX_FAILURE: u32 = 4;

/// The 1-byte handle every `OPEN` here is answered with: `READ` and `WRITE` bodies keep their
/// offset at a fixed place.
const HANDLE: u8 = 0xAB;

const CHUNK: u32 = 100;

fn chunk() -> NonZeroU32 {
    NonZeroU32::new(CHUNK).unwrap()
}

fn depth(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// The byte at file offset `n`, position-dependent so a misplaced chunk shows.
fn byte_at(n: u64) -> u8 {
    (n % 251) as u8
}

fn bytes_at(offset: u64, len: u32) -> Vec<u8> {
    (0..len as u64).map(|i| byte_at(offset + i)).collect()
}

struct Frame {
    kind: u8,
    id: u32,
    body: Vec<u8>,
}

impl Frame {
    /// `READ`: the offset and length after the id and the 1-byte handle.
    fn read_args(&self) -> (u64, u32) {
        let b = &self.body[9..];
        (
            u64::from_be_bytes(b[..8].try_into().unwrap()),
            u32::from_be_bytes(b[8..12].try_into().unwrap()),
        )
    }

    /// `WRITE`: the offset and data after the id and the 1-byte handle.
    fn write_args(&self) -> (u64, Vec<u8>) {
        let b = &self.body[9..];
        (
            u64::from_be_bytes(b[..8].try_into().unwrap()),
            b[12..].to_vec(),
        )
    }

    /// `OPEN`: the flags word after the path.
    fn open_flags(&self) -> u32 {
        let path_len = u32::from_be_bytes(self.body[4..8].try_into().unwrap()) as usize;
        let at = 8 + path_len;
        u32::from_be_bytes(self.body[at..at + 4].try_into().unwrap())
    }
}

/// The far side, driven line by line from a test.
struct Far<S = DuplexStream>(S);

impl<S: AsyncRead + AsyncWrite + Unpin> Far<S> {
    async fn frame(&mut self) -> Frame {
        let mut len = [0u8; 4];
        self.0.read_exact(&mut len).await.expect("length prefix");
        let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
        self.0.read_exact(&mut buf).await.expect("packet body");
        Frame {
            kind: buf[0],
            id: u32::from_be_bytes(buf[1..5].try_into().unwrap()),
            body: buf[1..].to_vec(),
        }
    }

    async fn frame_of(&mut self, kind: u8) -> Frame {
        let f = self.frame().await;
        assert_eq!(f.kind, kind, "unexpected request type");
        f
    }

    /// Takes `n` `READ`s off the wire without answering any.
    async fn reads(&mut self, n: usize) -> Vec<Frame> {
        let mut out = Vec::new();
        for _ in 0..n {
            out.push(self.frame_of(READ).await);
        }
        out
    }

    /// Nothing else arrives, on virtual time.
    async fn silent(&mut self) {
        let waited = tokio::time::timeout(Duration::from_secs(1), self.frame()).await;
        assert!(
            waited.is_err(),
            "an unexpected request of type {}",
            waited.map(|f| f.kind).unwrap_or(0)
        );
    }

    async fn send(&mut self, ty: u8, id: u32, payload: &[u8]) {
        let mut out = ((payload.len() + 5) as u32).to_be_bytes().to_vec();
        out.push(ty);
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(payload);
        self.0.write_all(&out).await.unwrap();
    }

    async fn status(&mut self, id: u32, code: u32) {
        let mut p = code.to_be_bytes().to_vec();
        p.extend_from_slice(&[0; 8]);
        self.send(101, id, &p).await;
    }

    async fn data(&mut self, id: u32, bytes: &[u8]) {
        let mut p = (bytes.len() as u32).to_be_bytes().to_vec();
        p.extend_from_slice(bytes);
        self.send(103, id, &p).await;
    }

    /// Answers the `READ` with the file's bytes for what it asked.
    async fn answer(&mut self, f: &Frame) {
        let (offset, len) = f.read_args();
        self.data(f.id, &bytes_at(offset, len)).await;
    }

    async fn open(&mut self) {
        let f = self.frame_of(OPEN).await;
        self.send(102, f.id, &[0, 0, 0, 1, HANDLE]).await;
    }

    async fn close(&mut self) {
        let f = self.frame_of(CLOSE).await;
        self.status(f.id, FX_OK).await;
    }
}

async fn connect_with(config: Config, pipe: usize) -> (Session, Far) {
    let (client_side, server_side) = tokio::io::duplex(pipe);
    let mut far = Far(server_side);
    let handshake = async {
        far.frame().await;
        far.0.write_all(VERSION_REPLY).await.unwrap();
    };
    let (session, ()) = tokio::join!(Session::open(client_side, config), handshake);
    (session.expect("handshake"), far)
}

async fn connect() -> (Session, Far) {
    connect_with(Config::default(), 256 * 1024).await
}

// ── reading ──

/// Asking for one chunk puts `depth` requests on the wire, at consecutive offsets of the chunk
/// length, and no more until one is taken.
#[tokio::test(start_paused = true)]
async fn the_first_chunk_puts_depth_requests_on_the_wire() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(3))
            .await
            .unwrap();
        assert_eq!(r.next().await.unwrap().unwrap(), bytes_at(0, CHUNK));
        r
    };
    let server = async {
        far.open().await;
        let reads = far.reads(3).await;
        let asked: Vec<_> = reads.iter().map(Frame::read_args).collect();
        assert_eq!(asked, [(0, CHUNK), (100, CHUNK), (200, CHUNK)]);
        far.silent().await;
        far.answer(&reads[0]).await;
        reads
    };
    let (mut r, reads) = tokio::join!(client, server);

    // Taking one chunk makes room for one more request.
    let more = async {
        let f = far.frame_of(READ).await;
        assert_eq!(f.read_args(), (300, CHUNK));
        far.answer(&reads[1]).await;
        far.answer(&reads[2]).await;
        far.answer(&f).await;
        far.silent().await;
    };
    let (taken, ()) = tokio::join!(r.next(), more);
    assert_eq!(taken.unwrap().unwrap(), bytes_at(100, CHUNK));

    let closing = far.close();
    let (closed, ()) = tokio::join!(r.close(), closing);
    closed.unwrap();
}

/// Replies landing in reverse are handed over in file order.
#[tokio::test(start_paused = true)]
async fn replies_in_reverse_arrive_in_file_order() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(3))
            .await
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(r.next().await.unwrap().unwrap());
        }
        r.close().await.unwrap();
        got
    };
    let server = async {
        far.open().await;
        let reads = far.reads(3).await;
        far.answer(&reads[2]).await;
        far.answer(&reads[1]).await;
        far.answer(&reads[0]).await;
        // Whatever was asked after those stays unanswered; only CLOSE matters.
        loop {
            let f = far.frame().await;
            if f.kind == CLOSE {
                far.status(f.id, FX_OK).await;
                break;
            }
        }
    };
    let (got, ()) = tokio::join!(client, server);
    assert_eq!(
        got,
        [
            bytes_at(0, CHUNK),
            bytes_at(100, CHUNK),
            bytes_at(200, CHUNK)
        ]
    );
}

/// A short reply asks for its remainder at once, with later offsets already outstanding, and
/// nothing is skipped or repeated.
#[tokio::test(start_paused = true)]
async fn a_short_reply_asks_for_the_rest_and_nothing_is_skipped() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let a = r.next().await.unwrap().unwrap();
        assert_eq!(r.offset(), 30);
        let b = r.next().await.unwrap().unwrap();
        assert_eq!(r.offset(), 100);
        let c = r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
        [a, b, c]
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.data(reads[0].id, &bytes_at(0, 30)).await;
        let rest = far.frame_of(READ).await;
        assert_eq!(
            rest.read_args(),
            (30, 70),
            "the remainder of the short reply"
        );
        far.answer(&rest).await;
        far.answer(&reads[1]).await;
        loop {
            let f = far.frame().await;
            if f.kind == CLOSE {
                far.status(f.id, FX_OK).await;
                break;
            }
        }
    };
    let (got, ()) = tokio::join!(client, server);
    assert_eq!(
        got,
        [bytes_at(0, 30), bytes_at(30, 70), bytes_at(100, CHUNK)]
    );
}

/// `EOF` on a later offset does not end the read before the bytes in front of it.
#[tokio::test(start_paused = true)]
async fn eof_on_a_later_offset_waits_for_the_bytes_in_front_of_it() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let first = r.next().await.unwrap().unwrap();
        let end = r.next().await.unwrap();
        assert_eq!(
            session.in_flight(),
            0,
            "what was due past the end is given up"
        );
        r.close().await.unwrap();
        (first, end)
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.status(reads[1].id, FX_EOF).await;
        far.answer(&reads[0]).await;
        let next = far.frame_of(READ).await;
        assert_eq!(next.read_args(), (200, CHUNK));
        far.close().await;
    };
    let ((first, end), ()) = tokio::join!(client, server);
    assert_eq!(first, bytes_at(0, CHUNK));
    assert_eq!(end, None);
}

/// A failure at the front is reported where it happened, and the bytes before it are all
/// delivered. Asking again retries from there, as `ReadFile` does.
#[tokio::test(start_paused = true)]
async fn a_failure_is_reported_in_order_and_asking_again_retries_from_there() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let first = r.next().await.unwrap().unwrap();
        let failed = r.next().await;
        assert!(
            matches!(&failed, Err(Error::Status(s)) if s.code == StatusCode::Failure),
            "{failed:?}"
        );
        assert_eq!(r.offset(), 100);
        let again = r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
        (first, again)
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.answer(&reads[0]).await;
        let third = far.frame_of(READ).await;
        assert_eq!(third.read_args(), (200, CHUNK));
        far.status(reads[1].id, FX_FAILURE).await;
        // The retry starts over from the failed offset, and the old third reply is not used.
        let retry = far.reads(2).await;
        assert_eq!(retry[0].read_args(), (100, CHUNK));
        assert_eq!(retry[1].read_args(), (200, CHUNK));
        far.data(third.id, &[0xEE; CHUNK as usize]).await;
        far.answer(&retry[0]).await;
        far.close().await;
    };
    let ((first, again), ()) = tokio::join!(client, server);
    assert_eq!(first, bytes_at(0, CHUNK));
    assert_eq!(again, bytes_at(100, CHUNK));
}

/// A reply longer than what was asked would overlap the next offset, so it is refused.
#[tokio::test(start_paused = true)]
async fn a_reply_longer_than_asked_is_refused() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let refused = r.next().await;
        assert!(
            matches!(
                refused,
                Err(Error::TooLong {
                    len: 101,
                    limit: 100
                })
            ),
            "{refused:?}"
        );
        assert_eq!(r.offset(), 0);
        r.close().await.unwrap();
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.data(reads[0].id, &bytes_at(0, CHUNK + 1)).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A stop sends no further `READ`: closing takes the replies already due, sends `CLOSE`, and the
/// session goes on.
#[tokio::test(start_paused = true)]
async fn closing_early_takes_what_is_due_and_sends_no_further_read() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
        session.stat(b"/still-alive").await
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.answer(&reads[0]).await;
        far.answer(&reads[1]).await;
        far.close().await;
        let stat = far.frame_of(STAT).await;
        far.send(105, stat.id, &[0, 0, 0, 0]).await;
        far.silent().await;
    };
    let (alive, ()) = tokio::join!(client, server);
    alive.expect("the session survives late replies");
    assert_eq!(session.in_flight(), 0);
}

/// Dropping a pending `next` loses nothing: the same chunk comes from the next call.
#[tokio::test(start_paused = true)]
async fn a_dropped_next_loses_nothing() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(1))
            .await
            .unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(10), r.next()).await;
        assert!(waited.is_err(), "the reply has not been sent yet");
        let got = r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
        got
    };
    let server = async {
        far.open().await;
        let f = far.frame_of(READ).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        far.answer(&f).await;
        loop {
            let f = far.frame().await;
            if f.kind == CLOSE {
                far.status(f.id, FX_OK).await;
                break;
            }
        }
    };
    let (got, ()) = tokio::join!(client, server);
    assert_eq!(got, bytes_at(0, CHUNK));
}

/// Each reply gets its own budget from when it reaches the front: replies 2 s apart against a 3 s
/// budget complete, though the last lands 8 s after it was sent.
#[tokio::test(start_paused = true)]
async fn the_reply_clock_runs_from_the_front_of_the_queue() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(config, 256 * 1024).await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(4))
            .await
            .unwrap();
        for i in 0..4u64 {
            let c = r
                .next()
                .await
                .expect("each reply is within its budget")
                .unwrap();
            assert_eq!(c, bytes_at(i * 100, CHUNK));
        }
        r.close().await.unwrap();
    };
    let server = async {
        far.open().await;
        let reads = far.reads(4).await;
        for f in &reads {
            tokio::time::sleep(Duration::from_secs(2)).await;
            far.answer(f).await;
        }
        loop {
            let f = far.frame().await;
            if f.kind == CLOSE {
                far.status(f.id, FX_OK).await;
                break;
            }
        }
    };
    tokio::join!(client, server);
}

/// A silent front still reports `Timeout`.
#[tokio::test(start_paused = true)]
async fn a_silent_front_times_out() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(config, 256 * 1024).await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let waited = r.next().await;
        assert!(matches!(waited, Err(Error::Timeout)), "{waited:?}");
        r.close().await.unwrap();
    };
    let server = async {
        far.open().await;
        far.reads(2).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

#[tokio::test(start_paused = true)]
async fn reading_ahead_from_an_offset_starts_there() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead_from(b"/f", 1000, chunk(), depth(2))
            .await
            .unwrap();
        assert_eq!(r.offset(), 1000);
        r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        assert_eq!(reads[0].read_args(), (1000, CHUNK));
        assert_eq!(reads[1].read_args(), (1100, CHUNK));
        far.answer(&reads[0]).await;
        loop {
            let f = far.frame().await;
            if f.kind == CLOSE {
                far.status(f.id, FX_OK).await;
                break;
            }
        }
    };
    tokio::join!(client, server);
}

// ── the watched read ──

#[tokio::test(start_paused = true)]
async fn a_watched_read_ahead_reports_totals_and_closes() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut seen = Vec::new();
        let mut totals = Vec::new();
        let d = session
            .read_file_ahead_watched(b"/f", chunk(), depth(2), &mut |c, t| {
                seen.extend_from_slice(c);
                totals.push(t);
                Walk::Continue
            })
            .await
            .unwrap();
        (d, seen, totals)
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.answer(&reads[1]).await;
        far.answer(&reads[0]).await;
        let third = far.frame_of(READ).await;
        far.status(third.id, FX_EOF).await;
        let fourth = far.frame_of(READ).await;
        far.status(fourth.id, FX_EOF).await;
        far.close().await;
    };
    let ((d, seen, totals), ()) = tokio::join!(client, server);
    assert_eq!((d.bytes, d.stopped), (200, false));
    assert_eq!(seen, bytes_at(0, 200));
    assert_eq!(totals, [100, 200]);
}

#[tokio::test(start_paused = true)]
async fn a_stopped_watched_read_ahead_says_so_and_closes() {
    let (session, mut far) = connect().await;

    let mut stop = |_: &[u8], _: u64| Walk::Stop;
    let client = session.read_file_ahead_watched(b"/f", chunk(), depth(2), &mut stop);
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.answer(&reads[0]).await;
        far.close().await;
        far.silent().await;
    };
    let (d, ()) = tokio::join!(client, server);
    let d = d.unwrap();
    assert_eq!((d.bytes, d.stopped), (100, true));
}

// ── writing ──

const SOME: &[u8] = &[0x5A; 100];

/// Writes return with their replies still due, up to `depth`; the next one waits for the oldest.
#[tokio::test(start_paused = true)]
async fn writes_return_up_to_depth_and_the_next_waits_for_the_oldest() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.write(SOME).await.unwrap();
        assert_eq!((w.offset(), w.acknowledged()), (200, 0));
        w.write(SOME).await.unwrap();
        assert_eq!((w.offset(), w.acknowledged()), (300, 100));
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        let b = far.frame_of(WRITE).await;
        assert_eq!(a.write_args(), (0, SOME.to_vec()));
        assert_eq!(b.write_args().0, 100);
        far.silent().await;
        far.status(a.id, FX_OK).await;
        let c = far.frame_of(WRITE).await;
        assert_eq!(c.write_args().0, 200);
        far.status(b.id, FX_OK).await;
        far.status(c.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A later write's reply does not move the acknowledged offset past one still due.
#[tokio::test(start_paused = true)]
async fn acknowledged_stays_contiguous_when_replies_land_out_of_order() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(4)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.write(SOME).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        w.write(SOME).await.unwrap();
        assert_eq!(w.acknowledged(), 0, "the first write is still due");
        tokio::time::sleep(Duration::from_millis(100)).await;
        w.write(SOME).await.unwrap();
        assert_eq!(
            w.acknowledged(),
            300,
            "the first three are in, without waiting"
        );
        w.flush().await.unwrap();
        assert_eq!((w.offset(), w.acknowledged()), (400, 400));
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        let b = far.frame_of(WRITE).await;
        far.status(b.id, FX_OK).await;
        let c = far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        far.status(c.id, FX_OK).await;
        let d = far.frame_of(WRITE).await;
        far.status(d.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// One write fails and the next is accepted: the hole stays behind the resume point, every later
/// call reports the same failure and sends nothing, and the handle is still closed.
#[tokio::test(start_paused = true)]
async fn a_failed_write_keeps_the_resume_point_before_the_hole() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(3)).await.unwrap();
        for _ in 0..3 {
            w.write(SOME).await.unwrap();
        }
        let failed = w.flush().await;
        let is_failure = |r: &justsftp::Result<()>| matches!(r, Err(Error::Status(s)) if s.code == StatusCode::Failure);
        assert!(is_failure(&failed), "{failed:?}");
        assert_eq!(
            w.acknowledged(),
            100,
            "before the failed write, not after the accepted one"
        );
        assert_eq!(session.in_flight(), 0, "the replies still due are given up");

        let again = w.write(SOME).await;
        assert!(is_failure(&again), "{again:?}");
        let flushed = w.flush().await;
        assert!(is_failure(&flushed), "{flushed:?}");
        assert_eq!(w.offset(), 300, "a refused write moved nothing");

        let closed = w.close().await;
        assert_eq!(closed.acknowledged, 100);
        assert!(is_failure(&closed.result), "{closed:?}");
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        let b = far.frame_of(WRITE).await;
        let c = far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        far.status(b.id, FX_FAILURE).await;
        far.close().await;
        far.status(c.id, FX_OK).await;
        far.silent().await;
    };
    tokio::join!(client, server);
}

/// A failure already known stops the next write, with room left in the queue.
#[tokio::test(start_paused = true)]
async fn a_known_failure_stops_the_next_write_before_the_queue_is_full() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(8)).await.unwrap();
        w.write(SOME).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let stopped = w.write(SOME).await;
        assert!(matches!(stopped, Err(Error::Status(_))), "{stopped:?}");
        assert_eq!(w.offset(), 100);
        let _ = w.close().await;
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.status(a.id, FX_FAILURE).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// `CLOSE` goes out only after every write's reply.
#[tokio::test(start_paused = true)]
async fn close_waits_for_the_writes_still_due() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.silent().await;
        far.status(a.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_flush_loses_nothing() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        w.write(SOME).await.unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(10), w.flush()).await;
        assert!(waited.is_err(), "the reply has not been sent yet");
        w.flush().await.unwrap();
        assert_eq!(w.acknowledged(), 100);
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        far.status(a.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A chunk one byte past `max_chunk` is refused without sending or moving anything, and the file
/// goes on. 262,144 less the 25-byte write header and the 1-byte handle.
#[tokio::test(start_paused = true)]
async fn a_chunk_past_the_ceiling_is_refused_and_the_file_goes_on() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        assert_eq!(w.max_chunk(), 262_118);
        let refused = w.write(&vec![0; 262_119]).await;
        assert!(matches!(refused, Err(Error::TooLong { .. })), "{refused:?}");
        assert_eq!(w.offset(), 0);
        w.write(&vec![0; 262_118]).await.unwrap();
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        assert_eq!(a.write_args().1.len(), 262_118);
        far.status(a.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// Each write gets its own budget from when it reaches the front: a server taking 2 s per packet
/// against a 3 s budget completes, though the last is on the wire 8 s after it was queued.
#[tokio::test(start_paused = true)]
async fn the_write_clock_runs_from_the_front_of_the_queue() {
    let config = Config {
        write_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(config, 8).await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(4)).await.unwrap();
        for _ in 0..4 {
            w.write(SOME).await.unwrap();
        }
        w.flush().await.expect("each write is within its budget");
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let f = far.frame_of(WRITE).await;
            far.status(f.id, FX_OK).await;
        }
        far.close().await;
    };
    tokio::join!(client, server);
}

#[tokio::test(start_paused = true)]
async fn a_new_file_is_created_and_truncated() {
    let (session, mut far) = connect().await;

    let client = async {
        let w = session.write_file_behind(b"/f", depth(1)).await.unwrap();
        w.close().await.result.unwrap();
    };
    let server = async {
        let open = far.frame_of(OPEN).await;
        assert_eq!(open.open_flags(), 0x1A, "WRITE | CREATE | TRUNCATE");
        far.send(102, open.id, &[0, 0, 0, 1, HANDLE]).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// Resuming keeps what is there and starts at the offset, which counts as acknowledged.
#[tokio::test(start_paused = true)]
async fn resuming_keeps_the_file_and_starts_at_the_offset() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session
            .write_file_behind_from(b"/f", 500, depth(1))
            .await
            .unwrap();
        assert_eq!((w.offset(), w.acknowledged()), (500, 500));
        w.write(SOME).await.unwrap();
        w.flush().await.unwrap();
        assert_eq!(w.acknowledged(), 600);
        w.close().await.result.unwrap();
    };
    let server = async {
        let open = far.frame_of(OPEN).await;
        assert_eq!(
            open.open_flags(),
            0x0A,
            "WRITE | CREATE, no TRUNCATE and no APPEND"
        );
        far.send(102, open.id, &[0, 0, 0, 1, HANDLE]).await;
        let a = far.frame_of(WRITE).await;
        assert_eq!(a.write_args().0, 500);
        far.status(a.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A `write` dropped while waiting for room queues nothing, so the same data can be written again.
#[tokio::test(start_paused = true)]
async fn a_dropped_write_queues_nothing() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(1)).await.unwrap();
        w.write(SOME).await.unwrap();
        let waited = tokio::time::timeout(Duration::from_millis(10), w.write(SOME)).await;
        assert!(waited.is_err(), "the first write has no reply yet");
        assert_eq!(w.offset(), 100);
        w.write(SOME).await.unwrap();
        assert_eq!(w.offset(), 200);
        w.close().await.result.unwrap();
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        far.status(a.id, FX_OK).await;
        let b = far.frame_of(WRITE).await;
        assert_eq!(b.write_args().0, 100, "once, at the next offset");
        far.status(b.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A write whose bytes cannot reach the stream is reported by the next call, with the real cause.
#[tokio::test(start_paused = true)]
async fn a_write_that_cannot_reach_the_stream_stops_the_next_one() {
    // Two pipes, so the write half fails while the read half stays alive.
    let (client_writes, server_reads) = tokio::io::duplex(64 * 1024);
    let (server_writes, client_reads) = tokio::io::duplex(64 * 1024);
    let mut far = Far(tokio::io::join(server_reads, server_writes));

    let handshake = async {
        far.frame().await;
        far.0.write_all(VERSION_REPLY).await.unwrap();
    };
    let (session, ()) = tokio::join!(
        Session::open(
            tokio::io::join(client_reads, client_writes),
            Config::default()
        ),
        handshake
    );
    let session = session.expect("handshake");

    let (w, ()) = tokio::join!(session.write_file_behind(b"/f", depth(8)), far.open());
    let mut w = w.unwrap();

    // Keeps the reader alive and fails every later write.
    let (reads, writes) = far.0.into_inner();
    drop(reads);

    w.write(SOME).await.unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let stopped = w.write(SOME).await;
    match &stopped {
        Err(Error::SessionEnded { cause }) => {
            assert!(matches!(&**cause, Error::Io(_)), "{cause:?}");
        }
        other => panic!("expected the write's cause, got {other:?}"),
    }
    assert_eq!((w.offset(), w.acknowledged()), (100, 0));
    assert!(w.close().await.result.is_err());
    drop(writes);
}

/// A zero-length reply is passed on as an empty chunk and its range is asked again.
#[tokio::test(start_paused = true)]
async fn a_zero_length_reply_is_an_empty_chunk_and_asked_again() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(2))
            .await
            .unwrap();
        let empty = r.next().await.unwrap().unwrap();
        assert_eq!(r.offset(), 0);
        let full = r.next().await.unwrap().unwrap();
        r.close().await.unwrap();
        (empty, full)
    };
    let server = async {
        far.open().await;
        let reads = far.reads(2).await;
        far.data(reads[0].id, &[]).await;
        let again = far.frame_of(READ).await;
        assert_eq!(again.read_args(), (0, CHUNK));
        far.answer(&again).await;
        far.close().await;
    };
    let ((empty, full), ()) = tokio::join!(client, server);
    assert!(empty.is_empty());
    assert_eq!(full, bytes_at(0, CHUNK));
}

/// The link ending hands over everything already in order, then the cause; asking again says so.
#[tokio::test(start_paused = true)]
async fn the_link_ending_hands_over_what_arrived_first() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut r = session
            .read_file_ahead(b"/f", chunk(), depth(3))
            .await
            .unwrap();
        assert_eq!(r.next().await.unwrap().unwrap(), bytes_at(0, CHUNK));
        assert_eq!(r.next().await.unwrap().unwrap(), bytes_at(100, CHUNK));
        let ended = r.next().await;
        assert!(
            matches!(ended, Err(Error::SessionEnded { .. })),
            "{ended:?}"
        );
        assert_eq!(r.offset(), 200);
        let again = r.next().await;
        assert!(matches!(again, Err(Error::Eof)), "{again:?}");
        assert!(r.close().await.is_err());
    };
    let server = async {
        far.open().await;
        let reads = far.reads(3).await;
        far.answer(&reads[0]).await;
        far.answer(&reads[1]).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        drop(far);
    };
    tokio::join!(client, server);
}

/// A server answers in order, so `CLOSE`'s reply comes after every `READ` already sent. Those replies
/// take the front's clock, so a slow link does not turn a stop into `Timeout`: replies 2 s apart
/// against a 3 s budget, and `CLOSE` answered last.
#[tokio::test(start_paused = true)]
async fn a_stop_on_a_slow_link_still_says_it_stopped() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(config, 256 * 1024).await;

    let mut stop = |_: &[u8], _: u64| Walk::Stop;
    let client = session.read_file_ahead_watched(b"/f", chunk(), depth(4), &mut stop);
    let server = async {
        far.open().await;
        let reads = far.reads(4).await;
        far.answer(&reads[0]).await;
        for f in &reads[1..] {
            tokio::time::sleep(Duration::from_secs(2)).await;
            far.answer(f).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        far.close().await;
    };
    let (d, ()) = tokio::join!(client, server);
    let d = d.expect("a stop is not a timeout");
    assert_eq!((d.bytes, d.stopped), (100, true));
}

/// An empty file: `EOF` at the front, the callback never told, and the handle closed.
#[tokio::test(start_paused = true)]
async fn an_empty_file_reads_no_chunks_ahead_and_still_closes() {
    let (session, mut far) = connect().await;

    let mut told = 0;
    let mut count = |_: &[u8], _: u64| {
        told += 1;
        Walk::Continue
    };
    let client = session.read_file_ahead_watched(b"/f", chunk(), depth(3), &mut count);
    let server = async {
        far.open().await;
        let reads = far.reads(3).await;
        for f in &reads {
            far.status(f.id, FX_EOF).await;
        }
        far.close().await;
    };
    let (d, ()) = tokio::join!(client, server);
    let d = d.unwrap();
    assert_eq!((d.bytes, d.stopped), (0, false));
    assert_eq!(told, 0);
}

/// A failure in the watched loop still closes the handle, and reports the failure.
#[tokio::test(start_paused = true)]
async fn a_failed_watched_read_ahead_still_closes() {
    let (session, mut far) = connect().await;

    let mut go_on = |_: &[u8], _: u64| Walk::Continue;
    let client = session.read_file_ahead_watched(b"/f", chunk(), depth(1), &mut go_on);
    let server = async {
        far.open().await;
        let f = far.frame_of(READ).await;
        far.status(f.id, FX_FAILURE).await;
        far.close().await;
    };
    let (d, ()) = tokio::join!(client, server);
    assert!(matches!(d, Err(Error::Status(_))), "{d:?}");
}

/// A write with no answer in time ends the file, like any failure, and the resume point is kept.
#[tokio::test(start_paused = true)]
async fn a_write_with_no_answer_in_time_ends_the_file() {
    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let (session, mut far) = connect_with(config, 256 * 1024).await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.write(SOME).await.unwrap();
        let flushed = w.flush().await;
        assert!(matches!(flushed, Err(Error::Timeout)), "{flushed:?}");
        assert_eq!(w.acknowledged(), 100);
        let again = w.write(SOME).await;
        assert!(matches!(again, Err(Error::Timeout)), "{again:?}");
        let _ = w.close().await;
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// `EOF` is how a read or a walk ends. It does not answer a write, which must not then count as
/// acknowledged.
#[tokio::test(start_paused = true)]
async fn a_write_answered_with_eof_is_a_failure() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(1)).await.unwrap();
        w.write(SOME).await.unwrap();
        let flushed = w.flush().await;
        assert!(
            matches!(&flushed, Err(Error::Status(s)) if s.code == StatusCode::Eof),
            "{flushed:?}"
        );
        assert_eq!(w.acknowledged(), 0);
        let _ = w.close().await;

        let mut w = session.write_file(b"/g").await.unwrap();
        let wrote = w.write(SOME).await;
        assert!(matches!(wrote, Err(Error::Status(_))), "{wrote:?}");
        assert_eq!(w.written(), 0);
        let _ = w.close().await;
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.status(a.id, FX_EOF).await;
        far.close().await;
        far.open().await;
        let b = far.frame_of(WRITE).await;
        far.status(b.id, FX_EOF).await;
        far.close().await;
    };
    tokio::join!(client, server);
}

/// A panic with a file still open reports that panic: a second one from `Drop` would abort the
/// process and take every other result with it.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "the caller's own failure")]
async fn a_panic_holding_a_read_file_is_not_a_second_panic() {
    let (session, mut far) = connect().await;
    let (r, ()) = tokio::join!(session.read_file(b"/f"), far.open());
    let _open = r.unwrap();
    panic!("the caller's own failure");
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "the caller's own failure")]
async fn a_panic_holding_a_write_file_is_not_a_second_panic() {
    let (session, mut far) = connect().await;
    let (w, ()) = tokio::join!(session.write_file(b"/f"), far.open());
    let _open = w.unwrap();
    panic!("the caller's own failure");
}

/// With no `flush` first, `close` still says where to resume after a failure, and `CLOSE` still goes.
#[tokio::test(start_paused = true)]
async fn close_with_no_flush_reports_the_resume_point_and_the_failure() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(3)).await.unwrap();
        for _ in 0..3 {
            w.write(SOME).await.unwrap();
        }
        w.close().await
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        let b = far.frame_of(WRITE).await;
        let c = far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        far.status(b.id, FX_FAILURE).await;
        far.close().await;
        far.status(c.id, FX_OK).await;
    };
    let (closed, ()) = tokio::join!(client, server);
    assert_eq!(closed.acknowledged, 100, "before the failed write");
    assert!(
        matches!(&closed.result, Err(Error::Status(s)) if s.code == StatusCode::Failure),
        "{closed:?}"
    );
}

/// A clean `close` reports everything acknowledged, with one `CLOSE`.
#[tokio::test(start_paused = true)]
async fn a_clean_close_reports_everything_acknowledged() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(2)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.close().await
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        far.close().await;
        far.silent().await;
    };
    let (closed, ()) = tokio::join!(client, server);
    assert_eq!(closed.acknowledged, 100);
    closed.result.unwrap();
}

/// A `CLOSE` the server refuses, after writes it accepted, is in `result` with the offset intact.
#[tokio::test(start_paused = true)]
async fn a_refused_close_is_reported_with_the_offset_intact() {
    let (session, mut far) = connect().await;

    let client = async {
        let mut w = session.write_file_behind(b"/f", depth(1)).await.unwrap();
        w.write(SOME).await.unwrap();
        w.close().await
    };
    let server = async {
        far.open().await;
        let a = far.frame_of(WRITE).await;
        far.status(a.id, FX_OK).await;
        let c = far.frame_of(CLOSE).await;
        far.status(c.id, FX_FAILURE).await;
    };
    let (closed, ()) = tokio::join!(client, server);
    assert_eq!(closed.acknowledged, 100);
    assert!(matches!(closed.result, Err(Error::Status(_))), "{closed:?}");
}
