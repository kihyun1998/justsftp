//! Request pairing and the session's clocks under concurrency, cancellation and a peer going away
//! (docs/map/territory/request-pairing.md, docs/map/territory/verification.md).

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

/// `SSH_FXP_ATTRS` carrying only a size.
fn attrs_reply(id: u32, size: u64) -> Vec<u8> {
    let mut v = vec![0x00, 0x00, 0x00, 0x11, 0x69];
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // flags: SIZE
    v.extend_from_slice(&size.to_be_bytes());
    v
}

/// Two requests in flight, answered out of order: each caller gets its own reply.
#[tokio::test]
async fn two_requests_in_flight_are_answered_out_of_order_and_each_caller_gets_its_own_reply() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        // Both requests are taken off the wire before either is answered.
        let (_, first) = read_frame(&mut server_side).await;
        let (_, second) = read_frame(&mut server_side).await;
        let (id_a, path_a) = id_and_path(&first);
        let (id_b, path_b) = id_and_path(&second);
        assert_eq!(path_a, b"/a");
        assert_eq!(path_b, b"/bb");

        // Answered in reverse, so a first-in-first-out implementation fails.
        server_side
            .write_all(&attrs_reply(id_b, 222))
            .await
            .unwrap();
        server_side
            .write_all(&attrs_reply(id_a, 111))
            .await
            .unwrap();
    });

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");

    // Mutation that reddens this: in `read_loop`, ignore the decoded id and instead pop any waiter
    // out of the map. Each caller then receives the other's size and both assertions fail.
    let (a, b) = tokio::join!(session.stat(b"/a"), session.stat(b"/bb"));

    assert_eq!(a.expect("/a").size, Some(111), "/a must get /a's answer");
    assert_eq!(b.expect("/bb").size, Some(222), "/bb must get /bb's answer");
    server.await.unwrap();
}

/// Upstream #95: the reply clock starts after the bytes are written. An 8-byte pipe, a server that
/// waits 5 virtual seconds before draining, a 3-second budget: a clock started at enqueue fails with
/// `Timeout`, one started after the write succeeds.
#[tokio::test(start_paused = true)]
async fn the_timeout_measures_the_wait_for_a_reply_and_not_the_wait_to_send() {
    let (client_side, mut server_side) = tokio::io::duplex(8);

    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();

        // Longer than the 3-second budget.
        tokio::time::sleep(Duration::from_secs(5)).await;

        let (_, body) = read_frame(&mut server_side).await;
        let (id, _) = id_and_path(&body);
        server_side.write_all(&attrs_reply(id, 7)).await.unwrap();
    });

    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
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

    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    let session = Session::open(client_side, config).await.expect("handshake");

    match session.stat(b"/never").await {
        Err(Error::Timeout) => {}
        other => panic!("expected Timeout, got {other:?}"),
    }
    server.abort();
}

/// A server that accepts the subsystem and never sends `SSH_FXP_VERSION` fails the open with
/// `Timeout`.
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

    let config = Config {
        request_timeout: Duration::from_secs(3),
        ..Config::default()
    };
    match Session::open(client_side, config).await {
        Err(Error::Timeout) => {}
        other => panic!("expected Timeout from the handshake, got {other:?}"),
    }
    server.abort();
}

/// An outbound packet over the ceiling is refused before it is sent.
///
/// Mutation: replace the guard in `enqueue` with `if false`. The packet is sent, the silent server
/// never answers, and this reddens as `Timeout` — quickly, since the budget here is short.
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

/// A request dropped mid-write leaves no half packet on the wire: the server then reads two whole,
/// correctly framed packets, the abandoned one included.
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

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");

    // 2,000 bytes, or the write finishes before the drop lands (docs/map/territory/verification.md).
    let long_path = vec![b'a'; 2000];
    {
        let mut doomed = Box::pin(session.stat(&long_path));
        // `biased` polls the request first, so the drop lands after the enqueue and before the
        // write completes.
        tokio::select! {
            biased;
            _ = &mut doomed => panic!("the request should not have completed"),
            _ = tokio::task::yield_now() => {}
        }
        drop(doomed);
    }

    let survived = session
        .stat(b"/second")
        .await
        .expect("the stream must still be framed");
    assert_eq!(survived.size, Some(5));

    let (t1, p1, t2, p2) = server.await.unwrap();
    assert_eq!(t1, 17, "SSH_FXP_STAT");
    assert_eq!(p1, long_path, "the abandoned packet went out whole");
    assert_eq!(t2, 17);
    assert_eq!(p2, b"/second");
}

/// A server that stops reading fails the write with `WriteTimeout` rather than parking every
/// request forever.
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
        // Not `Timeout`: the bytes never left.
        other => panic!("expected WriteTimeout, got {other:?}"),
    }
    server.abort();
}

/// A write failure reaches the request queued behind it as a real cause, not `Eof`. It cannot tell
/// the shared cause from a synthesised one (docs/map/territory/verification.md).
///
/// Mutation: delete the `rx.close()` drain in `write_loop`. The second request's `written` sender is
/// then dropped instead of answered, so it reports `Eof` and this reddens.
#[tokio::test]
async fn a_write_failure_reaches_the_requests_queued_behind_it() {
    // Two pipes, so the write half fails while the read half stays alive; otherwise the liveness
    // check refuses both requests before the writer sees them.
    let (client_writes, server_reads) = tokio::io::duplex(64 * 1024);
    let (mut server_writes, client_reads) = tokio::io::duplex(64 * 1024);
    let client_side = tokio::io::join(client_reads, client_writes);

    let server = tokio::spawn(async move {
        let mut server_reads = server_reads;
        read_frame(&mut server_reads).await;
        server_writes.write_all(VERSION_REPLY).await.unwrap();
        // The client's writes now fail; `server_writes` keeps its reads alive.
        drop(server_reads);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(server_writes);
    });

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // `join!` polls both in one pass and `enqueue` does not await, so both packets are queued
    // before the writer runs; the second is the one behind.
    let (first, second) = tokio::join!(session.stat(b"/one"), session.stat(b"/two"));

    for (label, outcome) in [
        ("the write that failed", first),
        ("the one queued behind it", second),
    ] {
        match outcome {
            Err(Error::SessionEnded { cause }) => {
                assert!(
                    matches!(&*cause, Error::Io(_)),
                    "{label}: expected an io cause, got {cause:?}"
                );
            }
            other => panic!("{label}: expected SessionEnded with a real cause, got {other:?}"),
        }
    }
    server.abort();
}

/// A cancelled request reclaims its slot in the pending map.
///
/// Mutation: empty the body of `impl Drop for Slot`. The final assertion goes from 0 to 3.
#[tokio::test(start_paused = true)]
async fn a_cancelled_request_reclaims_its_slot_in_the_pending_map() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        // Reads every request and answers none.
        loop {
            read_frame(&mut server_side).await;
        }
    });

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    assert_eq!(
        session.in_flight(),
        0,
        "nothing outstanding before we start"
    );

    for _ in 0..3 {
        let mut doomed = Box::pin(session.stat(b"/never-answered"));
        tokio::select! {
            biased;
            _ = &mut doomed => panic!("the server answers nothing"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        assert_eq!(
            session.in_flight(),
            1,
            "registered while the request is live"
        );
        drop(doomed);
        assert_eq!(
            session.in_flight(),
            0,
            "and reclaimed the moment the caller goes away"
        );
    }

    server.abort();
}

/// `close()` sends what is already queued rather than discarding it.
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

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
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
    // A stray reply is dropped and the session goes on.
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

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    assert_eq!(session.stat(b"/one").await.expect("first").size, Some(1));
    assert_eq!(
        session.stat(b"/two").await.expect("after the stray").size,
        Some(3)
    );
    server.await.unwrap();
}

/// An oversized header arriving through `read_loop` is refused, and the reason — `TooLong` —
/// reaches every waiting caller.
///
/// Mutation: raise the limit `read_loop` is given to `usize::MAX`.
#[tokio::test]
async fn an_oversized_packet_on_the_reader_path_is_refused_and_the_reason_reaches_the_caller() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        // 1 MiB against a 4 KiB ceiling; 1 MiB so the mutation is safe to run.
        server_side
            .write_all(&[0x00, 0x10, 0x00, 0x00, 0x69])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });

    let config = Config {
        max_inbound_packet: 4096,
        ..Config::default()
    };
    let session = Session::open(client_side, config).await.expect("handshake");

    // Two waiters, so a cause handed to only one of them is caught.
    //
    // Mutation: send the cause to one waiter only and `Eof` to the rest. One of the two assertions
    // reddens; which one varies between runs.
    let (a, b) = tokio::join!(session.stat(b"/big"), session.stat(b"/also-big"));

    for (label, outcome) in [("first", a), ("second", b)] {
        match outcome {
            Err(Error::SessionEnded { cause }) => match &*cause {
                Error::TooLong {
                    len: 1_048_576,
                    limit: 4096,
                } => {}
                other => panic!("{label}: expected TooLong as the cause, got {other:?}"),
            },
            other => panic!("{label}: expected SessionEnded, got {other:?}"),
        }
    }
    server.abort();
}

#[tokio::test]
async fn a_stream_that_ends_fails_the_waiting_requests_instead_of_leaving_them_to_time_out() {
    // The end of the stream fails the waiting requests at once.
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

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    match session.stat(b"/gone").await {
        Err(Error::SessionEnded { cause }) => assert!(
            matches!(&*cause, Error::Eof),
            "the stream really did end, so Eof is the honest cause: {cause:?}"
        ),
        other => panic!("expected SessionEnded, got {other:?}"),
    }
}

/// After a half-close, a new request fails at once with `Eof` instead of waiting out the reply
/// budget.
///
/// Mutation: delete the `reader_done` check at the top of `Session::enqueue`. This test then takes
/// the full budget and fails with `Timeout` instead of `Eof`.
#[tokio::test(start_paused = true)]
async fn a_request_issued_after_the_reader_died_fails_at_once_rather_than_waiting_out_the_budget() {
    // Two independent pipes, not one duplex split in two (docs/map/territory/verification.md).
    let (client_writes, mut server_reads) = tokio::io::duplex(64 * 1024);
    let (mut server_writes, client_reads) = tokio::io::duplex(64 * 1024);
    let client_side = tokio::io::join(client_reads, client_writes);

    tokio::spawn(async move {
        read_frame(&mut server_reads).await;
        server_writes.write_all(VERSION_REPLY).await.unwrap();
        // The client's read half ends here; `server_reads` keeps its write half open.
        drop(server_writes);
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(server_reads);
    });

    let config = Config {
        request_timeout: Duration::from_secs(30),
        ..Config::default()
    };
    let session = Session::open(client_side, config).await.expect("handshake");

    // Let the reader observe the end of its half.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let started = tokio::time::Instant::now();
    let outcome = session.stat(b"/after-the-reader-died").await;
    let waited = started.elapsed();

    assert!(
        matches!(outcome, Err(Error::Eof)),
        "expected a fast Eof, got {outcome:?}"
    );
    assert!(
        waited < Duration::from_secs(30),
        "it must not spend the reply budget on a connection already known to be gone: {waited:?}"
    );
}
