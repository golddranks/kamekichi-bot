use std::io;
use std::str::Utf8Error;

use crate::read_buf::FillError;

/// Actionable error from a WebSocket operation.
#[derive(Debug)]
pub enum Error {
    /// The connection is dead or protocol-violated.  Might get fixed by reconnecting.
    Reconnect(ConnectionError),
    /// Caller error.  Reconnecting will not help.
    Fatal(CallerError),
}

/// Connection-level error.  The connection is dead or protocol-violated;
/// the caller should reconnect.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConnectionError {
    /// I/O error on the underlying stream.
    Io(io::Error),
    /// The peer closed the connection (EOF).
    Closed,
    /// Response headers exceeded the size limit.
    HeadersTooLarge,
    /// Server did not respond with HTTP 101.
    BadStatus,
    /// Missing `Sec-WebSocket-Accept` header.
    MissingAccept,
    /// Invalid `Sec-WebSocket-Accept` value.
    BadAccept,
    /// Non-zero reserved bits in a frame header.
    BadReservedBits(u8),
    /// Frame payload exceeds the configured maximum.
    PayloadTooLarge(u64),
    /// Continuation frame without a preceding data frame.
    UnexpectedContinuation,
    /// Reassembled fragmented message exceeds the configured maximum.
    FragmentedMessageTooLarge(usize),
    /// Text frame or close reason is not valid UTF-8.
    InvalidUtf8(Utf8Error),
    /// New data frame received while a fragmented message is in progress.
    DataDuringFragmentation,
    /// Control frame with FIN bit unset.
    FragmentedControl,
    /// Control frame payload exceeds 125 bytes.
    ControlPayloadTooLarge(usize),
    /// Close frame payload is 1 byte (must be 0 or >= 2).
    BadClosePayload,
    /// Unrecognized frame opcode.
    UnknownOpcode(u8),
    /// Extended length field uses more bytes than necessary.
    NonMinimalLength,
    /// 64-bit payload length has the most significant bit set.
    PayloadLengthMsb,
    /// Missing or invalid `Upgrade: websocket` header.
    MissingUpgrade,
    /// Missing or invalid `Connection: Upgrade` header.
    MissingConnection,
    /// Close status code is not valid per RFC 6455 section 7.4.
    InvalidCloseCode(u16),
    /// Server sent a masked frame (clients must not receive masked frames).
    MaskedServerFrame,
    /// Too many frames arrived faster than they could trickle in.
    Flood,
    /// Server selected a subprotocol that was not offered by the client,
    /// or sent `Sec-WebSocket-Protocol` when the client did not request one.
    InvalidSubprotocol,
    /// No frame was received before the [`send_ping`](crate::WebSocket::send_ping) deadline.
    PingTimeout,
}

/// Caller-side error.  Reconnecting will not help.
#[derive(Debug)]
#[non_exhaustive]
pub enum CallerError {
    /// The connection is closing; no further operations are allowed.
    Closing,
    /// `host` or `path` passed to [`connect`](WebSocket::connect) contains CR or LF.
    InvalidHeaderValue,
    /// Close code is not valid for sending (RFC 6455 §7.4).
    InvalidCloseCode(u16),
    /// Close reason exceeds 123 bytes.
    CloseReasonTooLong(usize),
}

impl ConnectionError {
    pub(crate) fn is_would_block(&self) -> bool {
        matches!(
            self,
            ConnectionError::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
        )
    }
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::Io(e) => write!(f, "IO: {e}"),
            ConnectionError::Closed => write!(f, "connection closed"),
            ConnectionError::HeadersTooLarge => write!(f, "response headers too large"),
            ConnectionError::BadStatus => write!(f, "expected HTTP 101"),
            ConnectionError::MissingAccept => write!(f, "missing Sec-WebSocket-Accept"),
            ConnectionError::BadAccept => write!(f, "invalid Sec-WebSocket-Accept"),
            ConnectionError::BadReservedBits(bits) => write!(f, "non-zero RSV bits: {bits:#x}"),
            ConnectionError::PayloadTooLarge(len) => {
                write!(f, "frame payload too large: {len} bytes")
            }
            ConnectionError::UnexpectedContinuation => write!(f, "unexpected continuation frame"),
            ConnectionError::FragmentedMessageTooLarge(len) => {
                write!(f, "fragmented message too large: {len} bytes")
            }
            ConnectionError::InvalidUtf8(e) => {
                write!(f, "invalid UTF-8 in text frame at {}", e.valid_up_to())
            }
            ConnectionError::DataDuringFragmentation => {
                write!(f, "new data frame during fragmentation")
            }
            ConnectionError::FragmentedControl => write!(f, "fragmented control frame"),
            ConnectionError::ControlPayloadTooLarge(len) => {
                write!(f, "control frame payload too large: {len}")
            }
            ConnectionError::BadClosePayload => {
                write!(f, "close frame payload must be 0 or >= 2 bytes")
            }
            ConnectionError::UnknownOpcode(op) => write!(f, "unknown opcode: {op}"),
            ConnectionError::NonMinimalLength => write!(f, "non-minimal payload length encoding"),
            ConnectionError::PayloadLengthMsb => {
                write!(f, "64-bit payload length has MSB set (RFC 6455 §5.2)")
            }
            ConnectionError::MissingUpgrade => write!(f, "missing or invalid Upgrade header"),
            ConnectionError::MissingConnection => {
                write!(f, "missing or invalid Connection header")
            }
            ConnectionError::InvalidCloseCode(code) => write!(f, "invalid close code: {code}"),
            ConnectionError::MaskedServerFrame => write!(f, "server sent a masked frame"),
            ConnectionError::Flood => write!(f, "frame flood detected"),
            ConnectionError::InvalidSubprotocol => {
                write!(f, "server selected an unrequested subprotocol")
            }
            ConnectionError::PingTimeout => write!(f, "ping timeout: no response by deadline"),
        }
    }
}

impl std::error::Error for ConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConnectionError::Io(e) => Some(e),
            ConnectionError::InvalidUtf8(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ConnectionError {
    fn from(e: io::Error) -> Self {
        ConnectionError::Io(e)
    }
}

impl From<Utf8Error> for ConnectionError {
    fn from(e: Utf8Error) -> Self {
        ConnectionError::InvalidUtf8(e)
    }
}

impl From<FillError> for ConnectionError {
    fn from(e: FillError) -> Self {
        match e {
            FillError::Eof => ConnectionError::Closed,
            FillError::BufferFull => ConnectionError::HeadersTooLarge,
            FillError::Io(e) => e.into(),
        }
    }
}

impl std::fmt::Display for CallerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallerError::Closing => write!(f, "connection is closing"),
            CallerError::InvalidHeaderValue => write!(f, "invalid character in header value"),
            CallerError::InvalidCloseCode(code) => write!(f, "invalid close code: {code}"),
            CallerError::CloseReasonTooLong(len) => {
                write!(f, "close reason too long: {len} bytes (max 123)")
            }
        }
    }
}

impl std::error::Error for CallerError {}

impl From<ConnectionError> for Error {
    fn from(e: ConnectionError) -> Self {
        Error::Reconnect(e)
    }
}

impl From<CallerError> for Error {
    fn from(e: CallerError) -> Self {
        Error::Fatal(e)
    }
}

impl From<FillError> for Error {
    fn from(e: FillError) -> Self {
        Error::Reconnect(e.into())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Reconnect(ConnectionError::Io(e))
    }
}

impl From<Utf8Error> for Error {
    fn from(e: Utf8Error) -> Self {
        Error::Reconnect(ConnectionError::InvalidUtf8(e))
    }
}

impl Error {
    pub(crate) fn is_would_block(&self) -> bool {
        matches!(self, Error::Reconnect(e) if e.is_would_block())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Reconnect(_) => write!(f, "connection error"),
            Error::Fatal(_) => write!(f, "caller error"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Reconnect(e) => Some(e),
            Error::Fatal(e) => Some(e),
        }
    }
}
