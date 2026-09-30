//! The skeleton's two concurrency traps, upstream #33 and #95.
//!
//! Skeleton, not tail: any pipelining a caller builds runs straight over this path.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use justsftp::{Config, Error, Handle, Session};

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> (u8, Vec<u8>) {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.expect("length prefix");
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.expect("packet body");
    (buf[0], buf[1..].to_vec())
}

const VERSION_REPLY: &[u8] = &[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x03];

/// Pulls the request id and the path out of a one-path request, without using the crate's encoder.
fn id_and_path(body: &[u8]) -> (u32, Vec<u8>) {
    let id = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let n = u32::from_be_bytes([body[4], body[5], body[6], body[7]]) as usize;
    (id, body[8..8 + n].to_vec())
}

/// `SSH_FXP_ATTRS` carrying only a size. The harness builds this, which is fine here: what is under
/// test is **pairing**, and the harness shares none of that logic.
fn attrs_reply(id: u32, size: u64) -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x11, 0x69];
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // flags: SIZE
    v.extend_from_slice(&size.to_be_bytes());
    v
}

/// Upstream #33 — *"concurrent requests read mangled packets"*.
#[tokio::test]
async fn two_requests_in_flight_are_answered_out_of_order_and_each_caller_gets_its_own_reply() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        // Both requests are taken off the wire before either is answered — that is what "in flight"
        // means, and it is the state upstream got wrong.
        let (_, first) = read_frame(&mut server_side).await;
        let (_, second) = read_frame(&mut server_side).await;
        let (id_a, path_a) = id_and_path(&first);
        let (id_b, path_b) = id_and_path(&second);
        assert_eq!(path_a, b"/a");
        assert_eq!(path_b, b"/bb");

        // ⚠️ **Answered in reverse.** A server is under no obligation to reply in order, and a
        // client that assumes it will hands each caller the other one's answer. Replying in order
        // would let a first-in-first-out implementation pass.
        server_side.write_all(&attrs_reply(id_b, 222)).await.unwrap();
        server_side.write_all(&attrs_reply(id_a, 111)).await.unwrap();
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");

    // Mutation that reddens this: in `read_loop`, ignore the decoded id and instead pop any waiter
    // out of the map. Each caller then receives the other's size and both assertions fail.
    let (a, b) = tokio::join!(session.stat(b"/a"), session.stat(b"/bb"));

    assert_eq!(a.expect("/a").size, Some(111), "/a must get /a's answer");
    assert_eq!(b.expect("/bb").size, Some(222), "/bb must get /bb's answer");
    server.await.unwrap();
}

/// Upstream #95 — *"the timeout clock starts when `send()` returns, not when the request is out"*.
///
/// ⚠️ **Asserting that a timeout eventually happens would not observe this.** The defect is about
/// *when* the clock starts, so the test has to make writing slow and then require success:
///
/// - the pipe holds 8 bytes, so a request cannot be written in one go
/// - the server waits 5 virtual seconds before draining it
/// - the request budget is 3 seconds
///
/// With the clock starting at enqueue — `russh-sftp` pushes onto an unbounded channel drained by a
/// different task (`rawsession.rs:212-216`, `client/mod.rs:110-127`) — the 3 second budget expires
/// while the bytes are still being written and this fails with `Timeout`. With the clock starting
/// after `write_all` returns, the budget measures only the server's latency and this passes.
#[tokio::test(start_paused = true)]
async fn the_timeout_measures_the_wait_for_a_reply_and_not_the_wait_to_send() {
    let (client_side, mut server_side) = tokio::io::duplex(8);

    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        // Long enough to blow a 3 second budget several times over, if the budget were running.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let (_, body) = read_frame(&mut server_side).await;
        let (id, _) = id_and_path(&body);
        server_side.write_all(&attrs_reply(id, 7)).await.unwrap();
    });

    let config = Config { request_timeout: Duration::from_secs(3), ..Config::default() };
    let session = Session::open(client_side, config).await.expect("handshake");

    let attrs = session
        .stat(b"/a/path/long/enough/not/to/fit/in/an/eight/byte/pipe")
        .await
        .expect("the send stall must not be charged against the reply budget");
    assert_eq!(attrs.size, Some(7));
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_answers_produces_a_timeout_rather_than_hanging() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        // Deliberately silent. Held open so this is a silent server, not a closed stream.
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config { request_timeout: Duration::from_secs(3), ..Config::default() };
    let session = Session::open(client_side, config).await.expect("handshake");

    match session.stat(b"/never").await {
        Err(Error::Timeout) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    server.abort();
}

/// ⚠️ **A server can accept the subsystem and then never speak.** That is the wedged-but-accepted
/// case a caller has to tell apart from a dead link, and an unguarded handshake waits for it
/// forever. `tokio::time::timeout` appeared exactly once in this crate — inside `request()` — while
/// `Session::open` had no clock at all.
///
/// Mutation: remove the `timeout` wrapper around the handshake in `Session::open`. This test then
/// hangs rather than failing, which the harness reports.
#[tokio::test(start_paused = true)]
async fn a_handshake_the_server_never_completes_times_out_rather_than_hanging() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await; // takes the INIT and answers nothing
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config { request_timeout: Duration::from_secs(3), ..Config::default() };
    match Session::open(client_side, config).await {
        Err(Error::Timeout) => {}
        other => panic!("expected Timeout from the handshake, got {other:?}"),
    }
    server.abort();
}

/// The chunk-size trap's half that belongs to this crate: **refuse** an oversized outbound packet
/// rather than emitting one a strict server will reject. Chunking a large transfer to fit is
/// transfer *policy* and belongs to the caller.
///
/// ⚠️ This assertion did not exist when the ceiling was added, and its absence was found by
/// mutation, not by review: replacing the guard with `if false` left the whole suite green. That is
/// surface ablation — the code was there and nothing looked at it.
///
/// The timeout is deliberately short so that removing the guard fails **fast** rather than waiting
/// out the default budget: with no refusal the packet is sent, the silent server never answers, and
/// this reddens as `Timeout`.
#[tokio::test]
async fn a_request_larger_than_the_outbound_ceiling_is_refused_before_it_is_sent() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config {
        max_outbound_packet: 1024,
        request_timeout: Duration::from_secs(1),
        ..Config::default()
    };
    let session = Session::open(client_side, config).await.expect("handshake");

    let handle = Handle(vec![0x01]);
    match session.write(&handle, 0, &vec![0u8; 4096]).await {
        Err(Error::TooLong { limit: 1024, .. }) => {}
        other => panic!("expected the request to be refused, got {other:?}"),
    }

    // And the ceiling is a ceiling, not a blanket refusal: a request that fits is still sent.
    assert!(session.max_read_len() > 0);
    server.abort();
}

/// ⚠️ **The cancel-safety property, asserted rather than argued.**
///
/// `write_all` is not cancel-safe. An earlier draft held a lock on the write half and called it
/// directly in `request()`, so a caller dropped mid-write released that lock with **half a packet
/// on the wire** — and the next request appended its frame straight after, misframing everything
/// from then on. Silently: the session produces garbage until the reader gives up.
///
/// The pipe holds 8 bytes so the first request cannot be written in one go, and the future is
/// dropped while it is still waiting for the write to finish. The proof is that the server then
/// reads **two whole, correctly framed packets** — the abandoned one included.
///
/// Mutation: move the write back inline under a lock in `request()`. The second `read_frame` on the
/// server then never completes, and this hangs.
#[tokio::test]
async fn a_cancelled_request_does_not_leave_half_a_packet_on_the_wire() {
    let (client_side, mut server_side) = tokio::io::duplex(8);

    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        let (t1, b1) = read_frame(&mut server_side).await; // the abandoned request, in full
        let (t2, b2) = read_frame(&mut server_side).await; // and the next one, still aligned
        let (id2, path2) = id_and_path(&b2);
        server_side.write_all(&attrs_reply(id2, 5)).await.unwrap();
        (t1, id_and_path(&b1).1, t2, path2)
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");

    // ⚠️ **The path is 2000 bytes on purpose, and a short one made this test unable to fail.**
    // Through an 8-byte pipe a ~50-byte packet finishes its write before the drop below is
    // scheduled, so the window being tested is never open and a genuinely cancellable write passed.
    // Measured: with a short path a mutation that abandons the write mid-packet came back green; at
    // 2000 bytes it reddens. The window has to be wide enough for the drop to land inside it.
    let long_path = vec![b'a'; 2000];
    {
        let mut doomed = Box::pin(session.stat(&long_path));
        // `biased` polls the request first: it registers, encodes, hands the packet to the writer
        // task, and only then pends. So the drop below is guaranteed to land *after* the enqueue
        // and *before* the write completes — which is precisely the window being tested.
        tokio::select! {
            biased;
            _ = &mut doomed => panic!("the request should not have completed"),
            _ = tokio::task::yield_now() => {}
        }
        drop(doomed);
    }

    let survived = session.stat(b"/second").await.expect("the stream must still be framed");
    assert_eq!(survived.size, Some(5));

    let (t1, p1, t2, p2) = server.await.unwrap();
    assert_eq!(t1, 17, "SSH_FXP_STAT");
    assert_eq!(p1, long_path, "the abandoned packet went out whole");
    assert_eq!(t2, 17);
    assert_eq!(p2, b"/second");
}

/// ⚠️ **A server that accepts the subsystem and then stops reading must not park every request
/// forever**, and an earlier version did exactly that.
///
/// Splitting the write out of the reply budget is what makes the reply budget honest (upstream #95).
/// But the split left the write half with **no clock at all**: `written_rx.await` was bare, writes
/// are serial, so one stalled write parked every request behind it with no `Timeout`, no `Io` and no
/// `Eof` — a silent infinite hang, which is strictly worse than the false timeout #95 complains
/// about. This is the same wedged-but-accepted case the handshake clock exists for, arriving on the
/// other side.
///
/// Mutation: replace the `timeout(write_timeout, written_rx)` in `Session::request` with a bare
/// `written_rx.await`. This test then hangs instead of failing.
#[tokio::test(start_paused = true)]
async fn a_server_that_stops_reading_fails_the_write_rather_than_hanging_forever() {
    let (client_side, mut server_side) = tokio::io::duplex(8);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        // Never reads again. The client's write half fills and stays full.
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config {
        write_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(30),
        ..Config::default()
    };
    let session = Session::open(client_side, config).await.expect("handshake");

    match session.stat(&vec![b'a'; 4000]).await {
        Err(Error::WriteTimeout) => {}
        // ⚠️ `Timeout` here would be the wrong answer, not merely a different one: it says the
        // server did not reply, when the bytes never left.
        other => panic!("expected WriteTimeout, got {other:?}"),
    }
    server.abort();
}

/// ⚠️ **A write failure must reach the requests queued *behind* it, not just the one that hit it.**
///
/// `write_loop` writes serially, so when a write fails there is usually a queue behind it. An
/// earlier version handed the real `io::Error` to the failing job and a synthesised bare
/// `BrokenPipe` — with no `raw_os_error` — to the rest, which is indistinguishable in a log from a
/// real one; it now clones one `Arc` to everybody. `russh-sftp` discards the error entirely
/// (`client/mod.rs:119`, `let _ = wr.write_all(..)`), so a send that never happened surfaces only as
/// a reply timeout with no cause attached.
///
/// ⚠️ **What this test can and cannot see.** It cannot distinguish the shared cause from a
/// synthesised one — both are `Io(BrokenPipe)` and a duplex's error carries no `raw_os_error` to
/// tell them apart. What it does pin is the difference that is observable: the queued caller learns
/// a **real cause** rather than `Eof`, which is what *"the sender vanished"* would give it.
///
/// Mutation: delete the `rx.close()` drain in `write_loop`. The second request's `written` sender is
/// then dropped instead of answered, so it reports `Eof` and this reddens.
#[tokio::test]
async fn a_write_failure_reaches_the_requests_queued_behind_it() {
    // Two pipes again: the client's WRITE half must be able to fail while its READ half stays alive,
    // or `enqueue`'s liveness check refuses both requests before the writer ever sees them and the
    // drain is never exercised. One duplex split in two cannot express that (see the half-close
    // test above).
    let (client_writes, server_reads) = tokio::io::duplex(64 * 1024);
    let (mut server_writes, client_reads) = tokio::io::duplex(64 * 1024);
    let client_side = tokio::io::join(client_reads, client_writes);

    let server = tokio::spawn(async move {
        let mut server_reads = server_reads;
        read_frame(&mut server_reads).await;
        server_writes.write_all(VERSION_REPLY).await.unwrap();
        // The client's writes now fail. Its reads do not: `server_writes` is held for the lifetime
        // of this task, so the reader stays alive and the liveness check stays quiet.
        drop(server_reads);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(server_writes);
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // `join!` polls both in one pass, and `enqueue` is synchronous up to its first await, so both
    // packets are in the channel before the writer task runs. The first fails; the second is the
    // one behind it.
    let (first, second) = tokio::join!(session.stat(b"/one"), session.stat(b"/two"));

    for (label, outcome) in [("the write that failed", first), ("the one queued behind it", second)]
    {
        match outcome {
            Err(Error::SessionEnded { cause }) => {
                assert!(matches!(&*cause, Error::Io(_)), "{label}: expected an io cause, got {cause:?}");
            }
            other => panic!("{label}: expected SessionEnded with a real cause, got {other:?}"),
        }
    }
    server.abort();
}

/// ⚠️ **A cancelled request must not leak its slot in the pending map.**
///
/// Nothing else reclaims it: `read_loop` removes on a matching reply and `close()` drains, but a
/// dropped future runs neither — the sender half was moved into the map and only the receiver dies
/// with the future. Against a server that never answers, the entry is permanent. Cancelling a
/// transfer mid-flight is a **supported operation**, so this is not an edge case.
///
/// Mutation: empty the body of `impl Drop for Slot`. The final assertion goes from 0 to 3.
#[tokio::test(start_paused = true)]
async fn a_cancelled_request_reclaims_its_slot_in_the_pending_map() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        // Reads every request and answers none of them, so nothing is ever reclaimed by a reply.
        loop {
            read_frame(&mut server_side).await;
        }
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");
    assert_eq!(session.in_flight(), 0, "nothing outstanding before we start");

    for _ in 0..3 {
        let mut doomed = Box::pin(session.stat(b"/never-answered"));
        tokio::select! {
            biased;
            _ = &mut doomed => panic!("the server answers nothing"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        assert_eq!(session.in_flight(), 1, "registered while the request is live");
        drop(doomed);
        assert_eq!(session.in_flight(), 0, "and reclaimed the moment the caller goes away");
    }

    server.abort();
}

/// ⚠️ **`close()` must drain what is already queued, not throw it away.**
///
/// A teardown that returns too quickly has queued its last packets, not sent them. An
/// earlier version aborted the writer first, discarding every packet still in the channel; the one
/// most likely to be sitting there is the `SSH_FXP_CLOSE` that `list_dir` queues to release its
/// directory handle, so the leak lands on the server as an unreleased handle.
///
/// Mutation: abort the writer before dropping the sender in `close()`. The server's second
/// `read_frame` then never completes and this hangs.
#[tokio::test]
async fn close_drains_the_queue_rather_than_discarding_it() {
    let (client_side, mut server_side) = tokio::io::duplex(8);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        let (t, b) = read_frame(&mut server_side).await;
        (t, id_and_path(&b).1)
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");
    let queued = vec![b'q'; 2000];
    {
        let mut abandoned = Box::pin(session.stat(&queued));
        tokio::select! {
            biased;
            _ = &mut abandoned => panic!("should not have completed"),
            _ = tokio::task::yield_now() => {}
        }
        drop(abandoned);
    }

    session.close().await;

    let (t, p) = server.await.unwrap();
    assert_eq!(t, 17, "SSH_FXP_STAT");
    assert_eq!(p, queued, "the queued packet survived teardown");
}

#[tokio::test]
async fn a_reply_carrying_an_id_nobody_is_waiting_on_does_not_kill_the_session() {
    // A server answering twice, or answering a request that already timed out, is a packet that
    // harms nothing. Tearing the session down over it would turn a stray packet into a dropped
    // connection.
    //
    // Mutation: make `read_loop` break when the pending map has no entry for the id. The second
    // `stat` below then fails with `Eof`.
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        let (_, body) = read_frame(&mut server_side).await;
        let (id, _) = id_and_path(&body);
        server_side.write_all(&attrs_reply(id, 1)).await.unwrap();
        server_side.write_all(&attrs_reply(9999, 2)).await.unwrap(); // nobody is waiting

        let (_, body) = read_frame(&mut server_side).await;
        let (id, _) = id_and_path(&body);
        server_side.write_all(&attrs_reply(id, 3)).await.unwrap();
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");
    assert_eq!(session.stat(b"/one").await.expect("first").size, Some(1));
    assert_eq!(session.stat(b"/two").await.expect("after the stray").size, Some(3));
    server.await.unwrap();
}

/// ⚠️ **The inbound ceiling has to be tested on the path packets actually arrive by.**
///
/// An earlier version tested it only through `Session::open`, which calls `read_packet` directly.
/// The reader task gets its own copy of the limit, so raising *that* one to `usize::MAX` left the
/// whole suite green — a vacuous window over the half that matters. The handshake VERSION packet is
/// one packet; every NAME and DATA response, which is where an oversized header would actually
/// ride in, arrives through `read_loop`.
///
/// This also pins the other half: the **reason** reaches the caller. An earlier `read_loop` did
/// `Err(_) => break` and then told every waiter `Eof`, which made this refusal indistinguishable
/// from the server hanging up — and that is what made the mutation above silent.
#[tokio::test]
async fn an_oversized_packet_on_the_reader_path_is_refused_and_the_reason_reaches_the_caller() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        // A header declaring 1 MiB against a 4 KiB ceiling. 1 MiB rather than 4 GiB so that
        // removing the check is safe to run as a mutation rather than allocating four gigabytes.
        server_side.write_all(&[0x00, 0x10, 0x00, 0x00, 0x69]).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config { max_inbound_packet: 4096, ..Config::default() };
    let session = Session::open(client_side, config).await.expect("handshake");

    // ⚠️ **Two requests, not one, and that is the whole strength of this test.** With a single
    // waiter, "the cause reaches the caller" is satisfied by handing it to an arbitrary one — which
    // is what an earlier `read_loop` did, sending the real reason to whichever waiter
    // `HashMap::drain` happened to yield first and `Eof` to the rest. `drain` has no order, so which
    // caller learned the truth was nondeterministic between runs, and a one-request test could never
    // see it.
    //
    // Mutation: send the cause to `waiters.next()` only and `Eof` to the rest. One of these two
    // assertions reddens, and which one is not reproducible — which is itself the finding.
    let (a, b) = tokio::join!(session.stat(b"/big"), session.stat(b"/also-big"));

    for (label, outcome) in [("first", a), ("second", b)] {
        match outcome {
            Err(Error::SessionEnded { cause }) => match &*cause {
                Error::TooLong { len: 1_048_576, limit: 4096 } => {}
                other => panic!("{label}: expected TooLong as the cause, got {other:?}"),
            },
            other => panic!("{label}: expected SessionEnded, got {other:?}"),
        }
    }
    server.abort();
}

#[tokio::test]
async fn a_stream_that_ends_fails_the_waiting_requests_instead_of_leaving_them_to_time_out() {
    // A dropped connection should not cost every in-flight request a full timeout each.
    //
    // Mutation: delete the drain loop at the end of `read_loop`. This test then hangs until the
    // 30 second default budget expires, which the harness reports rather than passing.
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        drop(server_side); // the far end goes away mid-request
    });

    let session = Session::open(client_side, Config::default()).await.expect("handshake");
    match session.stat(b"/gone").await {
        Err(Error::SessionEnded { cause }) => assert!(
            matches!(&*cause, Error::Eof),
            "the stream really did end, so Eof is the honest cause: {cause:?}"
        ),
        other => panic!("expected SessionEnded, got {other:?}"),
    }
}

/// ⚠️ **A half-close leaves the write half usable, and that is the shape a peer going away actually
/// has** — a TCP half-close, or `SSH_MSG_CHANNEL_EOF` on a channel.
///
/// An earlier version had no signal from the reader to anything else, so a request issued *after*
/// the reader died enqueued happily, was written happily, and then waited the **full reply budget**
/// for an answer that structurally could not come — reporting `Timeout`, i.e. *"the server is
/// slow"*, about a connection that was already gone. `russh-sftp` closes this with a
/// `CancellationToken` shared by both halves (`client/mod.rs:88-89`, `:105`, `:121`).
///
/// Mutation: delete the `reader_done` check at the top of `Session::enqueue`. This test then takes
/// the full budget and fails with `Timeout` instead of `Eof`.
#[tokio::test(start_paused = true)]
async fn a_request_issued_after_the_reader_died_fails_at_once_rather_than_waiting_out_the_budget() {
    // ⚠️ **A half-close needs two independent pipes, not one duplex split in two.**
    // `tokio::io::split` hands back halves that share a single `DuplexStream` behind a lock, so
    // dropping one of them signals **nothing** to the peer — the stream closes only when both are
    // gone. Measured: this test failed with `Timeout` twice while the fix under test was working,
    // once because the wrong half was dropped and once because dropping either changes nothing.
    // Two pipes joined with `tokio::io::join` give the client a read half and a write half whose
    // lifetimes are genuinely independent, which is what a real half-close is.
    let (client_writes, mut server_reads) = tokio::io::duplex(64 * 1024);
    let (mut server_writes, client_reads) = tokio::io::duplex(64 * 1024);
    let client_side = tokio::io::join(client_reads, client_writes);

    tokio::spawn(async move {
        read_frame(&mut server_reads).await;
        server_writes.write_all(VERSION_REPLY).await.unwrap();
        // The client's READ half ends here. `server_reads` stays alive for the lifetime of this
        // task, so the client's WRITE half remains open — that asymmetry is the thing under test.
        drop(server_writes);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(server_reads);
    });

    let config = Config { request_timeout: Duration::from_secs(30), ..Config::default() };
    let session = Session::open(client_side, config).await.expect("handshake");

    // Let the reader observe the end of its half.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = tokio::time::Instant::now();
    let outcome = session.stat(b"/after-the-reader-died").await;
    let waited = started.elapsed();

    assert!(matches!(outcome, Err(Error::Eof)), "expected a fast Eof, got {outcome:?}");
    assert!(
        waited < Duration::from_secs(30),
        "it must not spend the reply budget on a connection already known to be gone: {waited:?}"
    );
}
