//! An SFTP v3 client over any async byte stream. **A path is bytes.**
//!
//! A remote filename is whatever bytes the server's filesystem holds, and SFTP has no
//! open-by-directory-entry, so a name that does not survive the listing byte for byte cannot be
//! opened, transferred, renamed or deleted. Every path, filename, long name, handle and extension
//! name here is `Vec<u8>` and goes back to the server unchanged; only the `SSH_FXP_STATUS` message
//! and language tag, the two fields draft-ietf-secsh-filexfer-02 defines as text, are `String`.
//!
//! # Listing a directory
//!
//! ```no_run
//! # async fn demo<S>(stream: S) -> Result<(), justsftp::Error>
//! # where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
//! use justsftp::{Config, FileType, Session};
//!
//! let session = Session::open(stream, Config::default()).await?;
//! let home = session.real_path(b".").await?;
//! for entry in session.list_dir(&home).await? {
//!     // Keep `entry.filename` to address the file; decode a copy only to show it.
//!     let shown = String::from_utf8_lossy(&entry.filename);
//!     let is_dir = entry.attrs.file_type() == Some(FileType::Directory);
//!     println!("{shown}{}", if is_dir { "/" } else { "" });
//! }
//! session.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! # Downloading and uploading
//!
//! The crate owns the loop, so the handle is closed whichever way it ends; the caller chooses the
//! chunk size and can stop after any chunk.
//!
//! ```no_run
//! # async fn demo(session: justsftp::Session) -> Result<(), Box<dyn std::error::Error>> {
//! use std::io::{Read, Write};
//! use justsftp::{Feed, Walk};
//!
//! let mut local = std::fs::File::create("report.pdf")?;
//! let download = session
//!     .read_file_watched(b"/srv/report.pdf", session.max_read_len(), &mut |chunk, _total| {
//!         local.write_all(chunk).map_or(Walk::Stop, |_| Walk::Continue)
//!     })
//!     .await?;
//! assert!(!download.stopped, "the local write failed part-way");
//!
//! let mut source = std::fs::File::open("notes.txt")?;
//! let upload = session
//!     .write_file_watched(b"/srv/notes.txt", 32 * 1024, &mut |len| {
//!         let mut buf = vec![0; len as usize];
//!         match source.read(&mut buf) {
//!             Ok(0) => Feed::Done,
//!             Ok(n) => Feed::Bytes(buf[..n].to_vec()),
//!             Err(_) => Feed::Stop,
//!         }
//!     })
//!     .await?;
//! println!("{} bytes written", upload.bytes);
//! # Ok(())
//! # }
//! ```
//!
//! # Over russh
//!
//! The crate names no SSH library. With `russh`, open a session channel, request the `sftp`
//! subsystem, **wait for the server's answer** — `request_subsystem` only sends it — and hand over
//! the channel's stream:
//!
//! ```ignore
//! use russh::ChannelMsg;
//!
//! let mut channel = handle.channel_open_session().await?;
//! channel.request_subsystem(true, "sftp").await?;
//! loop {
//!     match channel.wait().await {
//!         Some(ChannelMsg::Success) => break,
//!         Some(ChannelMsg::Failure) => return Err("the server refused the sftp subsystem".into()),
//!         Some(_) => continue,
//!         None => return Err("the channel closed".into()),
//!     }
//! }
//! let session = justsftp::Session::open(channel.into_stream(), justsftp::Config::default()).await?;
//! ```
//!
//! Without the wait, a server that refuses the subsystem looks like one that never answers: the
//! handshake times out. Put a timeout around the wait as well.
//!
//! # What this crate is not
//!
//! - **Not an SSH client.** It runs over any `AsyncRead + AsyncWrite + Unpin + Send + 'static`.
//! - **Not a decoder.** Turning a filename's bytes into something to show is the caller's.
//! - **Not pipelined.** One request is in flight per call; many calls can run at once.
//! - **Not the whole protocol.** Listing, transfer and basic metadata; no `SSH_FXP_SYMLINK`.
//! - **v3 only.** A server offering another version is refused at the handshake.
//!
//! A server's [`Status::message`] is its own text, passed on as sent, control characters
//! included.

#![warn(missing_docs)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/kihyun1998/justsftp/main/logo/icons/justsftp-icon-tile-256.png",
    html_favicon_url = "https://raw.githubusercontent.com/kihyun1998/justsftp/main/logo/icons/justsftp-icon-tile-32.png"
)]

mod attrs;
mod client;
mod error;
mod protocol;
mod session;
mod wire;

pub use attrs::{AttrsUpdate, Extension, FileAttributes, FileType};
pub use client::{Download, Feed, Listing, ReadFile, Upload, Walk, WriteFile};
pub use error::{Error, Result, Status, StatusCode};
pub use protocol::{
    DirEntry, Handle, OpenFlags, Request, Response, ServerLimits, ServerVersion, VERSION,
};
pub use session::{Config, Session};

// The README's examples are compiled by `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
