# justsftp

An SFTP v3 client for Rust over any async byte stream, where **a path is bytes, never a `String`**.

A remote filename is whatever bytes the server's filesystem holds. Clients that decode it as UTF-8
lose non-UTF-8 names — `한글.txt` in EUC-KR, `日本語.txt` in Shift_JIS — and because SFTP addresses
a file by its path, such a file shows up in a listing but cannot be opened, transferred, renamed or
deleted. justsftp reads every path, filename, handle and extension name as raw bytes and sends them
back unchanged.

```rust
use justsftp::{Config, Session};

// `stream` is any AsyncRead + AsyncWrite + Unpin + Send + 'static.
let session = Session::open(stream, Config::default()).await?;
let home = session.real_path(b".").await?;
for entry in session.list_dir(&home).await? {
    // `entry.filename` is Vec<u8>. How to draw it is your decision.
    println!("{}", String::from_utf8_lossy(&entry.filename));
}
session.close().await;
```

## What it is not

- **Not an SSH client.** It runs over any `AsyncRead + AsyncWrite + Unpin + Send`. With `russh`,
  open a channel, request the `sftp` subsystem, and pass `channel.into_stream()`.
- **Not a decoder.** It never converts a path; displaying one is the caller's.
- **Not the whole protocol.** Listing, transfer and basic metadata. No `SSH_FXP_SYMLINK`.
- **v3 only.** A server offering another version is refused at the handshake.

## Dependencies

`tokio` only.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
