<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/kihyun1998/justsftp/main/logo/png/justsftp-lockup-white.png">
    <img src="https://raw.githubusercontent.com/kihyun1998/justsftp/main/logo/png/justsftp-lockup-black.png" alt="justsftp" width="560">
  </picture>
</p>

[![crates.io](https://img.shields.io/crates/v/justsftp.svg)](https://crates.io/crates/justsftp)
[![docs.rs](https://img.shields.io/docsrs/justsftp)](https://docs.rs/justsftp)

An SFTP v3 client for Rust over any async byte stream, where **a path is bytes, never a `String`**.

A remote filename is whatever bytes the server's filesystem holds. Clients that decode it as UTF-8
lose the names that are not — `한글.txt` in EUC-KR, `日本語.txt` in Shift_JIS — and because SFTP
addresses a file by its path, such a file shows up in a listing but can never be opened,
transferred, renamed or deleted. justsftp reads every path, filename, handle and extension name as
raw bytes and sends them back unchanged.

```toml
[dependencies]
justsftp = "0.1"
```

## Example

```rust,no_run
use justsftp::{Config, FileType, Session};

async fn list_home<S>(stream: S) -> justsftp::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let session = Session::open(stream, Config::default()).await?;
    let home = session.real_path(b".").await?;
    for entry in session.list_dir(&home).await? {
        // Keep `entry.filename` to address the file; decode a copy only to show it.
        let shown = String::from_utf8_lossy(&entry.filename);
        let is_dir = entry.attrs.file_type() == Some(FileType::Directory);
        println!("{shown}{}", if is_dir { "/" } else { "" });
    }
    session.close().await;
    Ok(())
}
```

Downloading and uploading go through `read_file_watched` and `write_file_watched`, which own the
loop, close the handle whichever way it ends, and let the caller stop after any chunk. The
[documentation](https://docs.rs/justsftp) has examples of both.

## Connecting over SSH

justsftp names no SSH library. Give it the byte stream of a channel on which the `sftp` subsystem
was accepted. With [`russh`](https://crates.io/crates/russh):

```rust,ignore
use russh::ChannelMsg;

let mut channel = handle.channel_open_session().await?;
channel.request_subsystem(true, "sftp").await?;
// `request_subsystem` only sends the request; wait for the answer.
loop {
    match channel.wait().await {
        Some(ChannelMsg::Success) => break,
        Some(ChannelMsg::Failure) => return Err("the server refused the sftp subsystem".into()),
        Some(_) => continue,
        None => return Err("the channel closed".into()),
    }
}
let session = justsftp::Session::open(channel.into_stream(), justsftp::Config::default()).await?;
```

## What it covers

- Listing: `list_dir`, `list_dir_watched`, `read_dir`, `real_path`.
- Reading and writing: `read_file_watched`, `write_file_watched`, and `ReadFile` / `WriteFile` for a
  caller that drives the loop itself; `write_file_from` and `read_file_from` to resume.
- Metadata: `stat`, `lstat`, `fstat`, `set_stat`, `remove`, `rename`, `mkdir`, `rmdir`, `read_link`.
- `limits@openssh.com`: asked at open; the server's stated limits lower read and write lengths.
- Separate timeouts for writing a request and for the reply, and requests that are safe to cancel.

## What it does not do

- **It is not an SSH client.** It runs over any `AsyncRead + AsyncWrite + Unpin + Send + 'static`.
- **It does not decode names.** How to show a byte filename is your decision.
- **It is not pipelined.** Each call has one request in flight; many calls can run at once.
- **No `SSH_FXP_SYMLINK`.** Servers disagree on its argument order.
- **v3 only.** A server offering another version is refused at the handshake.
- **A server's status message is passed on as sent**, control characters included. Treat it as
  untrusted text if you show it.

## Dependencies

`tokio` only. Minimum Rust version: 1.96.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
