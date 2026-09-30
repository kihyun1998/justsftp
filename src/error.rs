//! Errors, and the SFTP status code as it comes off the wire.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// `SSH_FXP_STATUS` codes, v3 (draft-ietf-secsh-filexfer-02 § 7).
///
/// ⚠️ **`Unknown(u32)` is load-bearing, not defensive.** `russh-sftp` models this as a serde enum
/// with nine variants and no catch-all, so a server answering with a code from a later protocol
/// version does not produce an unknown *status* — it fails **the whole packet**, and the request it
/// belonged to surfaces as a decode error with the real reason discarded. A status code is a number
/// on the wire; refusing to hold one we have no name for buys nothing and loses the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Ok,
    Eof,
    NoSuchFile,
    PermissionDenied,
    Failure,
    BadMessage,
    /// Code 6. ⚠️ **Kept, deliberately.** `openssh-sftp-protocol` rejects 6 and 7 at parse time on the
    /// grounds that they are locally generated pseudo-errors a server "MUST NOT return"
    /// (`response.rs:245-249`). That reasoning is about what a *correct* server does; a client that
    /// turns a protocol violation into a decode failure loses the ability to report what the server
    /// actually said. We carry the value and let the caller decide.
    NoConnection,
    /// Code 7. See `NoConnection`.
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

    /// `Eof` is how the server ends a directory walk, so it is a control signal rather than a
    /// failure. Everything except this and `Ok` is an error.
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Ok | Self::Eof)
    }
}

/// The server answered, and what it said was a failure.
///
/// `message` and `language_tag` are the two fields the spec **does** define as text
/// (`error message (ISO-10646 UTF-8)`), which is why they are `String` here while every path in
/// this crate is bytes. The split is the point of the crate.
///
/// ⚠️ **`message` is prose a remote server wrote.** It is parsed as text and handed over verbatim,
/// escape sequences and control characters included; whether it may be shown, and after what
/// sanitising, is the caller's. Everything else in [`Error`] is a structured variant carrying
/// values, not sentences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: StatusCode,
    pub message: String,
    pub language_tag: String,
}

#[derive(Debug)]
pub enum Error {
    /// A packet ended before a field it declared. Carries what was asked for and what was left, so
    /// a fixture that is one byte short says so instead of saying "bad message".
    Truncated {
        needed: usize,
        had: usize,
    },
    /// A length field a correct server would never send. **This is the guard against an allocation
    /// sized by the far end** — see `session::Limits::max_inbound_packet`.
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
    /// A reply arrived carrying a request id nothing is waiting on.
    UnknownRequestId(u32),
    /// A new request drew an id that is **already outstanding** — the `u32` counter wrapped.
    ///
    /// ⚠️ Distinct from `UnknownRequestId`, and an earlier draft reused that one for this. They are
    /// exact inverses — nothing waiting versus something already waiting — so the log line read as
    /// the opposite of what happened. `russh-sftp` has neither: it `insert`s blindly
    /// (`rawsession.rs:206`), dropping the previous sender, and the earlier caller is told the
    /// sender was dropped.
    RequestIdInUse(u32),
    /// The server answered the request with `SSH_FXP_STATUS` and a failing code.
    Status(Status),
    /// The server offered a protocol version this client does not speak.
    UnsupportedVersion {
        theirs: u32,
        ours: u32,
    },
    /// The session ended, and this is why.
    ///
    /// ⚠️ **`Arc` is what lets every waiter learn the real cause, and it is not decoration.**
    /// `Error` cannot be `Clone` — `std::io::Error` is not — so an earlier version handed the real
    /// reason to *one* waiter and told the rest `Eof`. With N requests in flight that is N−1 wrong
    /// answers, and "first" was whichever one `HashMap::drain` happened to yield, which is
    /// nondeterministic. Worse, `Eof` is a **false statement** for the cases that matter most: on a
    /// `TooLong` refusal the stream is not over at all — this client refused to continue.
    SessionEnded {
        cause: std::sync::Arc<Error>,
    },
    /// The request could not be written to the stream inside the budget.
    ///
    /// ⚠️ **Distinct from `Timeout`, and the distinction is the whole point.** `Timeout` means the
    /// server did not answer; this means the bytes never got out — a peer that accepted the
    /// subsystem and then stopped reading, so the channel window never reopens. Collapsing the two
    /// reports "the server is slow" for a connection that is wedged.
    WriteTimeout,
    /// The request was written to the stream and no reply arrived inside the budget.
    ///
    /// ⚠️ **The clock starts when the bytes are written, not when they are queued** — see
    /// `session`. That distinction is upstream #95 and it is why this variant exists rather than a
    /// bare `Timeout`.
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
            Self::UnknownRequestId(id) => write!(f, "reply for unknown request id {id}"),
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
