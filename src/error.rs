//! Errors, and the SFTP status code as it comes off the wire.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// `SSH_FXP_STATUS` codes, v3 (draft-ietf-secsh-filexfer-02 § 7). A code v3 does not name is
/// carried as `Unknown`, not refused.
// Why `Unknown` and codes 6 and 7 are carried: docs/map/territory/errors.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok,
    Eof,
    NoSuchFile,
    PermissionDenied,
    Failure,
    BadMessage,
    /// Code 6. A server should not send it; it is carried if one does.
    NoConnection,
    /// Code 7. A server should not send it; it is carried if one does.
    ConnectionLost,
    OpUnsupported,
    Unknown(u32),
}

impl StatusCode {
    pub fn from_wire(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::Eof,
            2 => Self::NoSuchFile,
            3 => Self::PermissionDenied,
            4 => Self::Failure,
            5 => Self::BadMessage,
            6 => Self::NoConnection,
            7 => Self::ConnectionLost,
            8 => Self::OpUnsupported,
            other => Self::Unknown(other),
        }
    }

    pub fn to_wire(self) -> u32 {
        match self {
            Self::Ok => 0,
            Self::Eof => 1,
            Self::NoSuchFile => 2,
            Self::PermissionDenied => 3,
            Self::Failure => 4,
            Self::BadMessage => 5,
            Self::NoConnection => 6,
            Self::ConnectionLost => 7,
            Self::OpUnsupported => 8,
            Self::Unknown(v) => v,
        }
    }

    /// Everything except `Ok` and `Eof` is an error; `Eof` is how a server ends a directory walk or
    /// a file read.
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Ok | Self::Eof)
    }
}

/// The server answered, and what it said was a failure.
///
/// `message` and `language_tag` are the two fields the spec defines as text, so they are `String`
/// while every path is bytes.
///
/// ⚠️ **`message` is prose a remote server wrote**, handed over verbatim, escape sequences and
/// control characters included. Whether it may be shown, and after what sanitising, is the
/// caller's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: StatusCode,
    pub message: String,
    pub language_tag: String,
}

#[derive(Debug)]
pub enum Error {
    /// A packet ended before a field it declared: how many bytes were needed and how many were left.
    Truncated {
        needed: usize,
        had: usize,
    },
    /// A length above a ceiling: an inbound packet over [`Config::max_inbound_packet`], an outbound
    /// one over [`Config::max_outbound_packet`], or, for a write, data longer than the server's
    /// limits allow.
    ///
    /// [`Config::max_inbound_packet`]: crate::Config::max_inbound_packet
    /// [`Config::max_outbound_packet`]: crate::Config::max_outbound_packet
    TooLong {
        len: u64,
        limit: u64,
    },
    /// A packet type byte this client does not implement.
    UnknownPacketType(u8),
    /// A reply arrived whose type cannot answer the request that is waiting.
    UnexpectedReply {
        expected: &'static str,
        got: u8,
    },
    /// A new request drew an id that is **already outstanding** — the `u32` counter wrapped.
    RequestIdInUse(u32),
    /// The server answered the request with `SSH_FXP_STATUS` and a failing code.
    Status(Status),
    /// The server offered a protocol version this client does not speak.
    UnsupportedVersion {
        theirs: u32,
        ours: u32,
    },
    /// The session ended, and `cause` is why. When the reader stops, every waiting request gets the
    /// same cause; when a write fails, the failing request and those queued behind it do.
    SessionEnded {
        cause: std::sync::Arc<Error>,
    },
    /// The request's bytes did not reach the stream within [`Config::write_timeout`]: the peer
    /// stopped reading. Not the same as [`Error::Timeout`].
    ///
    /// [`Config::write_timeout`]: crate::Config::write_timeout
    WriteTimeout,
    /// The request was written and no reply arrived within [`Config::request_timeout`], counted
    /// from when the bytes reached the stream. Also the handshake's, where one budget covers
    /// writing `SSH_FXP_INIT` and the reply.
    ///
    /// [`Config::request_timeout`]: crate::Config::request_timeout
    Timeout,
    /// The stream ended.
    Eof,
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, had } => {
                write!(
                    f,
                    "packet truncated: needed {needed} bytes, {had} remaining"
                )
            }
            Self::TooLong { len, limit } => {
                write!(f, "declared length {len} exceeds the limit {limit}")
            }
            Self::UnknownPacketType(t) => write!(f, "unknown packet type {t}"),
            Self::UnexpectedReply { expected, got } => {
                write!(f, "expected {expected}, got packet type {got}")
            }
            Self::RequestIdInUse(id) => write!(f, "request id {id} is already outstanding"),
            Self::Status(s) => write!(f, "server status {:?}: {}", s.code, s.message),
            Self::UnsupportedVersion { theirs, ours } => {
                write!(
                    f,
                    "server offered SFTP version {theirs}, this client speaks {ours}"
                )
            }
            Self::SessionEnded { cause } => write!(f, "session ended: {cause}"),
            Self::WriteTimeout => write!(f, "the request could not be written within the budget"),
            Self::Timeout => write!(f, "no reply within the request timeout"),
            Self::Eof => write!(f, "stream ended"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrecognised_status_code_is_carried_rather_than_failing_the_packet() {
        // The mutation that reddens this: give `from_wire` a `_ => Self::Failure` arm instead of
        // `Unknown(other)`. Then a v4+ code is silently reported as a plain failure and the number
        // the server sent is gone.
        assert_eq!(StatusCode::from_wire(9), StatusCode::Unknown(9));
        assert_eq!(StatusCode::from_wire(31), StatusCode::Unknown(31));
        assert_eq!(StatusCode::Unknown(31).to_wire(), 31);
    }

    #[test]
    fn every_defined_code_round_trips_through_the_wire_value() {
        // Mutation: swap any two arms of `from_wire` (e.g. 2 and 3). This reddens because the
        // round trip is checked against the *number*, not against the enum's own ordering.
        for v in 0..=8u32 {
            assert_eq!(
                StatusCode::from_wire(v).to_wire(),
                v,
                "code {v} did not round trip"
            );
        }
    }

    #[test]
    fn eof_is_not_an_error_but_every_other_nonzero_code_is() {
        // Mutation: make `is_error` `!matches!(self, Self::Ok)`. Then a directory walk's terminating
        // EOF is reported as a failure and every listing ends in an error.
        assert!(!StatusCode::Ok.is_error());
        assert!(!StatusCode::Eof.is_error());
        assert!(StatusCode::NoSuchFile.is_error());
        assert!(StatusCode::PermissionDenied.is_error());
        assert!(StatusCode::Unknown(9).is_error());
    }
}
