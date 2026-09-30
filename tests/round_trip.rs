//! The round trip that this crate exists for: **list a directory holding a non-UTF-8 filename, get
//! the bytes back, and open the file addressed by those bytes.**
//!
//! # Why the fixture is a literal byte array
//!
//! Three obvious fixtures were rejected, each for a different reason:
//!
//! - **`russh-sftp`'s server half.** `protocol::File.filename` is a `String` and the same struct
//!   serves both halves, so it **cannot emit** these bytes. The one test this whole change exists to
//!   make possible is the one that harness cannot express.
//! - **A packet built with our own encoder.** Self-drawn: a decoder graded against its own encoder
//!   agrees with itself and proves nothing.
//! - **A string literal in the test source.** The whole defect is that a `String` cannot hold the
//!   bytes.
//!
//! So the bytes below are written out by hand, and the only thing this file shares with the code
//! under test is the wire format itself.
//!
//! # No SSH anywhere
//!
//! The transport is `tokio::io::duplex` — two ends of an in-memory pipe. No SSH, no server, no
//! channel, no network, no `russh`. That separability is the design, not a testing convenience.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use justsftp::{Config, Error, FileType, OpenFlags, Session};

/// `한글.txt` as EUC-KR. **Not valid UTF-8**, and the exact shape paths-are-bytes.md records: `C7`
/// is an invalid lead byte and becomes U+FFFD, but the `D1 B1` that follows *is* a valid UTF-8
/// sequence decoding to a real Cyrillic letter — so a lossy decode does not even look broken.
const KOREAN_NAME: &[u8] = &[0xC7, 0xD1, 0xB1, 0xDB, 0x2E, 0x74, 0x78, 0x74];

/// `SSH_FXP_VERSION`, version 3, no extensions.
const VERSION_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x05, // length = 5 (covers the type byte)
    0x02, // SSH_FXP_VERSION
    0x00, 0x00, 0x00, 0x03, // version 3
];

/// `SSH_FXP_HANDLE` for request id 1. The handle is **not valid UTF-8 and contains a NUL** —
/// servers put counters and pointers in these, and `russh-sftp` types the field as `String`.
const DIR_HANDLE_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x0D, // length = 13
    0x66, // 102, SSH_FXP_HANDLE
    0x00, 0x00, 0x00, 0x01, // request id 1
    0x00, 0x00, 0x00, 0x04, // handle length 4
    0x00, 0xFF, 0x80, 0x01, // handle bytes
];

/// `SSH_FXP_NAME` for request id 2. **Two entries**, and the first carries an
/// `SSH_FILEXFER_ATTR_EXTENDED` tail.
///
/// ⚠️ The second entry is the load-bearing part. A decoder that recognises the EXTENDED flag and
/// then does not consume the tail — which is exactly `russh-sftp`, `file_attrs.rs:380`,
/// `// todo: extended implementation` — leaves 14 bytes in the buffer and misreads **entry two**.
/// With one entry the defect is invisible.
/// ⚠️ **`filename` and `longname` must differ in every entry, and an earlier version of this
/// fixture repeated the same eight bytes in both.** They are adjacent same-typed byte-strings in the
/// decoder, so with equal values a swap of those two lines left the *entire suite green* — while on
/// a real server `entry.filename` would become an `ls -l` line and opening by it would address
/// nothing. That is the vacuous-window pattern, and the crate already applies the opposite
/// discipline to `rename`'s two adjacent paths for exactly this reason.
const NAME_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x66, // length = 102
    0x68, // 104, SSH_FXP_NAME
    0x00, 0x00, 0x00, 0x02, // request id 2
    0x00, 0x00, 0x00, 0x02, // count = 2
    // ── entry 1 ──
    0x00, 0x00, 0x00, 0x08, // filename length 8
    0xC7, 0xD1, 0xB1, 0xDB, 0x2E, 0x74, 0x78, 0x74, // 한글.txt, EUC-KR
    0x00, 0x00, 0x00, 0x0C, // longname length 12 — deliberately NOT the filename
    0x2D, 0x72, 0x77, 0x2D, // "-rw-", the head of an ls -l line
    0xC7, 0xD1, 0xB1, 0xDB, 0x2E, 0x74, 0x78, 0x74, // ...ending in the same name
    0x80, 0x00, 0x00, 0x0D, // flags: SIZE | PERMISSIONS | ACMODTIME | EXTENDED
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2A, // size = 42
    0x00, 0x00, 0x81, 0xA4, // mode 0o100644 — S_IFREG plus rw-r--r--
    0x11, 0x11, 0x11, 0x11, // atime
    0x22, 0x22, 0x22, 0x22, // mtime  (atime first — swapped upstream once)
    0x00, 0x00, 0x00, 0x01, // one extension pair
    0x00, 0x00, 0x00, 0x02, 0x61, 0x62, // "ab"
    0x00, 0x00, 0x00, 0x02, 0x63, 0x64, // "cd"
    // ── entry 2 ── only reachable if the extension tail above was consumed
    0x00, 0x00, 0x00, 0x04, 0x6E, 0x65, 0x78, 0x74, // "next"
    0x00, 0x00, 0x00, 0x05, 0x64, 0x6E, 0x65, 0x78, 0x74, // "dnext" — again distinct
    0x00, 0x00, 0x00, 0x04, // flags: PERMISSIONS
    0x00, 0x00, 0x41, 0xED, // mode 0o040755 — S_IFDIR plus rwxr-xr-x
];

/// `SSH_FXP_STATUS` / `SSH_FX_EOF` for request id 3 — how a directory walk ends.
const EOF_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x11, // length = 17
    0x65, // 101, SSH_FXP_STATUS
    0x00, 0x00, 0x00, 0x03, // request id 3
    0x00, 0x00, 0x00, 0x01, // SSH_FX_EOF
    0x00, 0x00, 0x00, 0x00, // error message, empty
    0x00, 0x00, 0x00, 0x00, // language tag, empty
];

/// `SSH_FXP_STATUS` / `SSH_FX_OK` for request id 4 — the CLOSE.
const CLOSE_OK_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x11, 0x65, //
    0x00, 0x00, 0x00, 0x04, // request id 4
    0x00, 0x00, 0x00, 0x00, // SSH_FX_OK
    0x00, 0x00, 0x00, 0x00, //
    0x00, 0x00, 0x00, 0x00, //
];

/// `SSH_FXP_HANDLE` for request id 5 — the answer to opening the file by its returned bytes.
const FILE_HANDLE_REPLY: &[u8] = &[
    0x00, 0x00, 0x00, 0x0C, // length = 12
    0x66, // SSH_FXP_HANDLE
    0x00, 0x00, 0x00, 0x05, // request id 5
    0x00, 0x00, 0x00, 0x03, // handle length 3
    0xAA, 0x00, 0xBB, // handle bytes, again not text
];

/// Reads one framed packet **without using the crate under test**. Deliberately a second, tiny
/// implementation: a harness that shares the decoder it is grading grades nothing.
async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> (u8, Vec<u8>) {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.expect("length prefix");
    let n = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await.expect("packet body");
    (buf[0], buf[1..].to_vec())
}

/// Every fixture declares its own length. Checking that against the array's real size catches a
/// hand-counting slip in *this file* rather than letting it surface as a decoder bug.
#[test]
fn the_fixtures_are_internally_consistent() {
    for (name, f) in [
        ("VERSION", VERSION_REPLY),
        ("DIR_HANDLE", DIR_HANDLE_REPLY),
        ("NAME", NAME_REPLY),
        ("EOF", EOF_REPLY),
        ("CLOSE_OK", CLOSE_OK_REPLY),
        ("FILE_HANDLE", FILE_HANDLE_REPLY),
    ] {
        let declared = u32::from_be_bytes([f[0], f[1], f[2], f[3]]) as usize;
        assert_eq!(
            declared,
            f.len() - 4,
            "{name}: declared length disagrees with the array"
        );
    }
}

#[tokio::test]
async fn a_listing_returns_the_servers_bytes_and_a_file_opens_by_them() {
    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);

    let server = tokio::spawn(async move {
        let mut seen: Vec<(u8, Vec<u8>)> = Vec::new();

        seen.push(read_frame(&mut server_side).await); // SSH_FXP_INIT
        server_side.write_all(VERSION_REPLY).await.unwrap();

        for reply in [
            DIR_HANDLE_REPLY,
            NAME_REPLY,
            EOF_REPLY,
            CLOSE_OK_REPLY,
            FILE_HANDLE_REPLY,
        ] {
            seen.push(read_frame(&mut server_side).await);
            server_side.write_all(reply).await.unwrap();
        }
        seen
    });

    let session = Session::open(client_side, Config::default())
        .await
        .expect("handshake");
    assert_eq!(session.server_version().version, 3);

    let entries = session.list_dir(b"/home/user").await.expect("listing");

    // ── ① The bytes came back. This is the assertion the whole change exists for. ──
    //
    // Mutation that reddens it: decode the filename with `String::from_utf8_lossy(..).into_bytes()`
    // in `wire::Reader::string`. That is the upstream defect in one line, and this fails with
    // `EF BF BD D1 B1 EF BF BD 2E 74 78 74` against the expected eight bytes.
    assert_eq!(
        entries.len(),
        2,
        "the extension tail must not have eaten entry two"
    );
    assert_eq!(
        entries[0].filename, KOREAN_NAME,
        "the server's bytes, unchanged"
    );
    // Mutation: swap the `filename` and `longname` lines in `Response::decode`'s NAME arm. With
    // the two fields carrying different bytes this reddens; with the fixture repeating one value in
    // both, it did not.
    assert_eq!(
        entries[0].longname, b"-rw-\xC7\xD1\xB1\xDB.txt",
        "longname is NOT the filename"
    );
    assert_eq!(entries[0].attrs.size, Some(42));
    assert_eq!(entries[0].attrs.file_type(), Some(FileType::Regular));
    assert_eq!(entries[0].attrs.permissions(), Some(0o644));
    assert_eq!(entries[0].attrs.atime, Some(0x1111_1111));
    assert_eq!(entries[0].attrs.mtime, Some(0x2222_2222));
    assert_eq!(entries[0].attrs.extensions.len(), 1);

    // ── ② An ordinary UTF-8 name is unaffected, and it is only reachable through the tail. ──
    assert_eq!(entries[1].filename, b"next");
    assert_eq!(
        entries[1].longname, b"dnext",
        "and distinct in the second entry too"
    );
    assert_eq!(entries[1].attrs.file_type(), Some(FileType::Directory));
    assert_eq!(entries[1].attrs.permissions(), Some(0o755));

    // ── ③ Listing the bytes is half the fix. Addressing by them is the half that matters. ──
    let handle = session
        .open_file(&entries[0].filename, OpenFlags::READ)
        .await
        .expect("open by the bytes we were handed");
    assert_eq!(handle.as_bytes(), &[0xAA, 0x00, 0xBB]);

    let seen = server.await.unwrap();

    // ── ④ And the assertion that ③ cannot fake: what actually went out on the wire. ──
    //
    // `open_file` could return a handle while having sent a mangled path — the fixture would answer
    // it regardless, because a byte-array server does not check its input. So the OPEN request is
    // read back and the filename bytes are found in it.
    //
    // Mutation: route `path` through `String::from_utf8_lossy` in `Request::encode`. ③ still passes;
    // this reddens. That asymmetry is why both are here.
    let (open_type, open_body) = &seen[5];
    assert_eq!(*open_type, 3, "SSH_FXP_OPEN");
    assert_eq!(&open_body[0..4], &[0, 0, 0, 5], "request id 5");
    assert_eq!(&open_body[4..8], &[0, 0, 0, 8], "path length 8");
    assert_eq!(
        &open_body[8..16],
        KOREAN_NAME,
        "the path on the wire is the server's bytes"
    );

    // The handshake, and the four requests a listing costs.
    assert_eq!(seen[0].0, 1, "SSH_FXP_INIT");
    assert_eq!(seen[1].0, 11, "SSH_FXP_OPENDIR");
    assert_eq!(seen[2].0, 12, "SSH_FXP_READDIR");
    assert_eq!(seen[3].0, 12, "SSH_FXP_READDIR again, until EOF");
    assert_eq!(seen[4].0, 4, "SSH_FXP_CLOSE — the handle is not leaked");

    // The dir handle went back out byte for byte too.
    assert_eq!(&seen[2].1[4..12], &[0, 0, 0, 4, 0x00, 0xFF, 0x80, 0x01]);
}

/// The named mutation, run as a permanent test rather than only by hand.
///
/// ⚠️ **What this proves and what it does not.** On its own, "corrupted input gives a different
/// answer" is weak — a decoder that returned a constant would fail it too. Paired with the
/// byte-exact assertion above, it establishes that the assertion is reading *the fixture* and not
/// something it reconstructed: flip one byte of the filename and only that byte moves.
#[tokio::test]
async fn corrupting_one_byte_of_the_fixture_changes_exactly_that_byte() {
    let mut corrupted = NAME_REPLY.to_vec();
    let filename_at = 4 + 1 + 4 + 4 + 4; // length, type, id, count, filename length
    assert_eq!(corrupted[filename_at], 0xC7);
    corrupted[filename_at] = 0xC8;

    let (client_side, mut server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side.write_all(VERSION_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        server_side.write_all(DIR_HANDLE_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        server_side.write_all(&corrupted).await.unwrap();
        read_frame(&mut server_side).await;
        server_side.write_all(EOF_REPLY).await.unwrap();
        read_frame(&mut server_side).await;
        server_side.write_all(CLOSE_OK_REPLY).await.unwrap();
    });

    let session = Session::open(client_side, Config::default()).await.unwrap();
    let entries = session.list_dir(b"/home/user").await.unwrap();
    server.await.unwrap();

    assert_ne!(
        entries[0].filename, KOREAN_NAME,
        "the corrupted byte must be visible"
    );
    assert_eq!(
        entries[0].filename[0], 0xC8,
        "and it must be exactly the byte that was flipped"
    );
    assert_eq!(
        &entries[0].filename[1..],
        &KOREAN_NAME[1..],
        "nothing else moved"
    );
}

#[tokio::test]
async fn a_server_offering_a_version_this_client_does_not_speak_is_refused() {
    // ⚠️ Neither implementation read on disk has this guard, and both then over-read every STATUS
    // packet against a v2 server — the error message and language tag are v3's own addition
    // (draft § 10.1). Refusing here is what licenses reading those two fields unconditionally.
    //
    // Mutation: delete the version check in `Session::open`. This reddens.
    let (client_side, mut server_side) = tokio::io::duplex(1024);
    tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side
            .write_all(&[0x00, 0x00, 0x00, 0x05, 0x02, 0x00, 0x00, 0x00, 0x02])
            .await
            .unwrap();
    });

    match Session::open(client_side, Config::default()).await {
        Err(Error::UnsupportedVersion { theirs: 2, ours: 3 }) => {}
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[tokio::test]
async fn a_packet_larger_than_the_configured_ceiling_is_refused_before_it_is_allocated() {
    // ⚠️ `russh-sftp` passes `u32::MAX` here (`client/mod.rs:74`) and then does
    // `vec![0; length as usize]` (`utils.rs:21`), so a header declaring 4 GiB allocates 4 GiB.
    //
    // The declared length here is 1 MiB rather than 4 GiB **so that the mutation is safe to run**:
    // removing the `len > max` check must make this test fail, not make the test process allocate
    // four gigabytes. The mechanism is identical at either size — what is being observed is that
    // the ceiling is consulted before the buffer is sized.
    //
    // Mutation: delete the `len > max` block in `read_packet`. The read then succeeds in
    // allocating, finds no bytes behind the header, and this fails with `Eof` instead of `TooLong`.
    let (client_side, mut server_side) = tokio::io::duplex(1024);
    tokio::spawn(async move {
        read_frame(&mut server_side).await;
        server_side
            .write_all(&[0x00, 0x10, 0x00, 0x00, 0x02])
            .await
            .unwrap();
    });

    let config = Config {
        max_inbound_packet: 4096,
        ..Config::default()
    };
    match Session::open(client_side, config).await {
        Err(Error::TooLong {
            len: 1_048_576,
            limit: 4096,
        }) => {}
        other => panic!("expected TooLong, got {other:?}"),
    }
}
