//! An SFTP v3 client over any async byte stream. **A path is bytes.**
//!
//! A remote filename is whatever bytes the server's filesystem holds, and SFTP has no
//! open-by-directory-entry, so a name that does not survive the listing byte for byte cannot be
//! opened, transferred, renamed or deleted. Every path, filename, long name, handle and extension
//! name here is `Vec<u8>`; only the `SSH_FXP_STATUS` message and language tag, the two fields
//! draft-ietf-secsh-filexfer-02 mandates as UTF-8, are `String`.
//!
//! # What this crate is not
//!
//! - **Not an SSH client.** It is driven over any `AsyncRead + AsyncWrite + Unpin + Send`, such as
//!   `russh::Channel::into_stream()`.
//! - **Not a server.** What answers on the far side in a test is a byte fixture.
//! - **Not pipelined.** The request/response core supports many requests in flight; a pipelining
//!   policy is the caller's.
//! - **Not complete.** The verb set covers listing, transfer and basic metadata.
//! - **Not a decoder.** Turning a filename's bytes into something to draw is the caller's.
//!
//! # Example
//!
//! ```no_run
//! # async fn demo<S>(stream: S) -> Result<(), justsftp::Error>
//! # where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {
//! use justsftp::{Config, Session};
//!
//! let session = Session::open(stream, Config::default()).await?;
//! let home = session.real_path(b".").await?;
//! for entry in session.list_dir(&home).await? {
//!     // `entry.filename` is bytes. Turning it into something to draw is the caller's decision.
//!     let _ = (&entry.filename, entry.attrs.file_type());
//! }
//! # Ok(())
//! # }
//! ```

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
