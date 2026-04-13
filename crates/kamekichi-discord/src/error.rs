pub type Result<T = ()> = std::result::Result<T, Error>;
pub type StdResult<T, E> = std::result::Result<T, E>;

/// Errors from gateway, HTTP, or protocol operations.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Tls(rustls::Error),
    InvalidDnsName,
    Json(serde_json::Error),
    Rng(getrandom::Error),
    WebSocket(kamekichi_ws::Error),
    ResponseTooLarge(usize),
    BadStatus,
    InvalidUtf8,
    ConnectionClosed,
    HeadersTooLarge,
    MalformedChunk,
    RetriesExhausted { status: u16 },
    ApiError { status: u16 },
    Reconnect,
    InvalidSession { resumable: bool },
    ConnectionZombied,
    GatewayClosed { code: Option<u16>, reason: String },
    HelloNotText,
    UnexpectedOpcode(u8),
    UnexpectedBinary,
    MalformedDispatch,
    MissingPayload,
    BadHeartbeatInterval(u64),
    BadResumeUrl,
    BadToken,
    BadSessionId,
    NotConnected,
    HandshakeTimeout,
    UnexpectedHandshakeDispatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO: {e}"),
            Error::Tls(e) => write!(f, "TLS: {e}"),
            Error::InvalidDnsName => write!(f, "invalid DNS name"),
            Error::Json(e) => write!(f, "JSON: {e}"),
            Error::Rng(e) => write!(f, "getrandom: {e}"),
            Error::WebSocket(e) => write!(f, "WebSocket: {e}"),
            Error::ResponseTooLarge(n) => write!(f, "response too large: {n} bytes"),
            Error::BadStatus => write!(f, "malformed HTTP status line"),
            Error::InvalidUtf8 => write!(f, "invalid UTF-8 in response body"),
            Error::ConnectionClosed => write!(f, "connection closed"),
            Error::HeadersTooLarge => write!(f, "response headers too large"),
            Error::MalformedChunk => write!(f, "malformed chunk encoding"),
            Error::RetriesExhausted { status } => {
                write!(f, "retries exhausted (last status {status})")
            }
            Error::ApiError { status } => {
                write!(f, "API error (status {status})")
            }
            Error::Reconnect => write!(f, "reconnect requested"),
            Error::InvalidSession { resumable } => {
                write!(f, "invalid session (resumable: {resumable})")
            }
            Error::ConnectionZombied => write!(f, "heartbeat ACK overdue, connection zombied"),
            Error::GatewayClosed { code, reason } => match (code, reason.is_empty()) {
                (Some(code), true) => write!(f, "gateway closed (code {code})"),
                (Some(code), false) => write!(f, "gateway closed (code {code}): {reason}"),
                (None, _) => write!(f, "gateway closed (no code)"),
            },
            Error::HelloNotText => write!(f, "HELLO was not a text frame"),
            Error::UnexpectedOpcode(op) => write!(f, "unexpected opcode {op}"),
            Error::UnexpectedBinary => write!(f, "unexpected binary frame on JSON gateway"),
            Error::MalformedDispatch => write!(f, "dispatch missing event name or data"),
            Error::MissingPayload => write!(f, "gateway payload missing data field"),
            Error::BadHeartbeatInterval(ms) => {
                write!(f, "invalid heartbeat interval: {ms}ms")
            }
            Error::BadResumeUrl => write!(f, "invalid resume_gateway_url"),
            Error::BadToken => write!(f, "token contains characters requiring JSON escaping"),
            Error::BadSessionId => {
                write!(f, "session_id contains characters requiring JSON escaping")
            }
            Error::NotConnected => write!(f, "no active gateway session"),
            Error::HandshakeTimeout => write!(f, "gateway handshake timed out"),
            Error::UnexpectedHandshakeDispatch => {
                write!(f, "unexpected non-READY dispatch during handshake")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Tls(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Rng(e) => Some(e),
            Error::WebSocket(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Self {
        Error::Tls(e)
    }
}

impl From<rustls::pki_types::InvalidDnsNameError> for Error {
    fn from(_: rustls::pki_types::InvalidDnsNameError) -> Self {
        Error::InvalidDnsName
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<getrandom::Error> for Error {
    fn from(e: getrandom::Error) -> Self {
        Error::Rng(e)
    }
}

impl From<kamekichi_ws::Error> for Error {
    fn from(e: kamekichi_ws::Error) -> Self {
        Error::WebSocket(e)
    }
}

