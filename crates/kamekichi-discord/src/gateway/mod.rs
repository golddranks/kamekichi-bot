use std::net::TcpStream;
use std::ops::Not;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;

use kamekichi_ws as ws;
use rand_core::SeedableRng;
use rand_pcg::Pcg64Mcg;

use crate::heartbeat::HeartbeatState;
use crate::{Error, Result, TlsStream, message as msg};

#[cfg(test)]
mod tests;

/// Read/write timeout applied to the TCP socket during the HELLO handshake,
/// before `poll()` takes over timeout management.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

const OP_DISPATCH: u8 = 0;
pub(crate) const OP_HEARTBEAT: u8 = 1;
const OP_IDENTIFY: u8 = 2;
const OP_RESUME: u8 = 6;
const OP_RECONNECT: u8 = 7;
const OP_INVALID_SESSION: u8 = 9;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

#[derive(Deserialize)]
struct GatewayPayload<'a> {
    op: u8,
    #[serde(rename = "d", borrow)]
    data: Option<&'a RawValue>,
    #[serde(rename = "s")]
    sequence: Option<u64>,
    #[serde(rename = "t")]
    #[serde(borrow)]
    event_type: Option<&'a str>,
}

#[derive(Deserialize)]
struct HelloData {
    heartbeat_interval: u64,
}

/// An active gateway connection with heartbeat and session state.
pub struct Session {
    ws: ws::WebSocket<TlsStream, Pcg64Mcg>,
    heartbeat: HeartbeatState,
    pub(crate) data: SessData,
    /// Reusable buffer for outgoing gateway payloads (heartbeats, identify).
    write_buf: Vec<u8>,
}

/// Data from a fully initialized gateway session.
///
/// Produced by a successful IDENTIFY+READY handshake and carried across
/// resume attempts.  Passed back to [`Session::connect`] via
/// [`SessionState::Resuming`] to resume a disconnected session.
#[derive(Debug, Clone)]
pub struct SessData {
    session_id: String,
    resume_gateway_url: String,
    /// The last dispatch sequence number received from the gateway.
    sequence: u64,
    pub(crate) bot_user_id: u64,
    /// Guild IDs from the READY payload.
    pub(crate) guild_ids: Vec<u64>,
}

/// Parameters for a fresh IDENTIFY handshake.
pub struct ConnectData {
    pub gateway_url: String,
    pub intents: u64,
}

/// Whether to resume an existing session or identify as a new one.
pub enum SessionState {
    /// Resume a previous session using saved data.
    Resuming(SessData),
    /// Start a fresh session with an IDENTIFY handshake.
    Identifying(ConnectData),
}

/// Actionable error from [`Client::poll()`](crate::Client::poll).
///
/// Each variant tells the caller what to do, not just what went wrong.
#[derive(Debug)]
pub enum PollError {
    /// Connection lost but the session is valid for resume.
    /// Back off, then call `connect(intents, Some(session))`.
    Resume {
        error: Error,
        session: Box<SessData>,
    },
    /// Session is no longer valid. Back off, then call `connect(intents, None)`.
    Reconnect(Error),
    /// Configuration error (bad token, invalid intents, etc.). Do not retry.
    Fatal(Error),
    /// One message could not be processed but the connection is still alive.
    /// Log and keep calling `poll()`.
    Transient(Error),
    /// No active gateway session. Caller must `connect()` first.
    NotConnected,
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PollError::Resume { error, .. } => write!(f, "resume: {error}"),
            PollError::Reconnect(e) => write!(f, "reconnect: {e}"),
            PollError::Fatal(e) => write!(f, "fatal: {e}"),
            PollError::Transient(e) => write!(f, "transient: {e}"),
            PollError::NotConnected => write!(f, "not connected"),
        }
    }
}

impl std::error::Error for PollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PollError::Resume { error, .. } => Some(error),
            PollError::Reconnect(e) => Some(e),
            PollError::Fatal(e) => Some(e),
            PollError::Transient(e) => Some(e),
            PollError::NotConnected => None,
        }
    }
}

/// Returns `true` for gateway close codes that indicate a configuration
/// error — reconnecting with the same parameters will fail again.
fn is_fatal_close_code(code: u16) -> bool {
    matches!(code, 4004 | 4010 | 4011 | 4012 | 4013 | 4014)
}

/// Returns `true` for close codes where the session is dead and resume
/// will fail — the caller must do a fresh identify.
fn is_non_resumable_close_code(code: u16) -> bool {
    matches!(code, 4007 | 4009)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorClass {
    Fatal,
    Reconnect,
    Resume,
    Transient,
}

/// Classify an error by the action the caller should take.
fn classify_error(error: &Error) -> ErrorClass {
    use Error::*;
    match error {
        // Fatal gateway close codes — don't retry.
        GatewayClosed {
            code: Some(code), ..
        } if is_fatal_close_code(*code) => ErrorClass::Fatal,

        // Session dead (invalid seq / timed out) — fresh identify.
        GatewayClosed {
            code: Some(code), ..
        } if is_non_resumable_close_code(*code) => ErrorClass::Reconnect,

        // Connection-level failures — resume with session data.
        Reconnect
        | ConnectionZombied
        | ConnectionClosed
        | Io(_)
        | Tls(_)
        | Rng(_)
        | WebSocket(_)
        | GatewayClosed { .. }
        | InvalidSession { resumable: true } => ErrorClass::Resume,

        // Session explicitly invalidated — fresh identify.
        InvalidSession { resumable: false } => ErrorClass::Reconnect,

        // Single-message parse failures — connection is alive.
        Json(_) | UnexpectedOpcode(_) | UnexpectedBinary | MalformedDispatch | MissingPayload => {
            ErrorClass::Transient
        }

        // These errors originate from connect() or HTTP, not poll().
        // Listed explicitly so new Error variants cause a compile error.
        InvalidDnsName
        | ResponseTooLarge(_)
        | BadStatus
        | InvalidUtf8
        | HeadersTooLarge
        | MalformedChunk
        | RetriesExhausted { .. }
        | ApiError { .. }
        | BadHeartbeatInterval(_)
        | BadResumeUrl
        | BadToken
        | BadSessionId
        | NotConnected
        | HandshakeTimeout
        | HelloNotText
        | UnexpectedHandshakeDispatch => unreachable!("non-poll error reached classify_error"),
    }
}

/// Classify an error from `Session::poll()` into a [`PollError`].
///
/// Takes the session by value so the caller decides when to extract it.
/// Disconnect errors consume the session for resume data; transient
/// errors return it so the caller can put it back.
pub(crate) fn classify_poll_error(error: Error, session: Session) -> (PollError, Option<Session>) {
    match classify_error(&error) {
        ErrorClass::Fatal => (PollError::Fatal(error), None),
        ErrorClass::Reconnect => (PollError::Reconnect(error), None),
        ErrorClass::Resume => {
            let sess = Box::new(session.into_resume_data());
            (
                PollError::Resume {
                    error,
                    session: sess,
                },
                None,
            )
        }
        ErrorClass::Transient => (PollError::Transient(error), Some(session)),
    }
}

impl Session {
    /// Open a gateway connection, perform the TLS/WS handshake, and send IDENTIFY or RESUME.
    pub(crate) fn connect(
        tls_config: Arc<rustls::ClientConfig>,
        token: &str,
        state: SessionState,
    ) -> Result<Self> {
        // TLS + WebSocket handshake
        let mut ws = {
            let url_str = match state {
                SessionState::Resuming(ref r) => &r.resume_gateway_url,
                SessionState::Identifying(ref c) => &c.gateway_url,
            };
            let url = parse_gateway_url(url_str)?;
            // The resume_gateway_url from READY is a bare host — ensure the
            // gateway version and encoding are always present in the path.
            let path = if url.path.contains('?') {
                url.path.to_owned()
            } else {
                format!("{}?v=10&encoding=json", url.path)
            };
            let server_name = rustls::pki_types::ServerName::try_from(url.host)?;
            let tls_conn = rustls::ClientConnection::new(tls_config, server_name.to_owned())?;
            let tcp = 'conn: {
                let mut last_err = None;
                for addr in std::net::ToSocketAddrs::to_socket_addrs(&(url.host, url.port))? {
                    match TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
                        Ok(tcp) => break 'conn tcp,
                        Err(e) => last_err = Some(e),
                    }
                }
                return Err(last_err
                    .unwrap_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "DNS returned no addresses",
                        )
                    })
                    .into());
            };
            tcp.set_nodelay(true)?;
            tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
            tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed)?;
            ws::WebSocket::new(Pcg64Mcg::seed_from_u64(u64::from_ne_bytes(seed))).connect(
                rustls::StreamOwned::new(tls_conn, tcp),
                url.host,
                &path,
            )?
        };

        // Receive HELLO and extract heartbeat interval
        let heartbeat_interval_ms = {
            let text = match ws.read_message()? {
                ws::ReadStatus::Message(ws::Message::Text(p)) => p,
                ws::ReadStatus::Message(ws::Message::Close(code, reason)) => {
                    return Err(Error::GatewayClosed {
                        code,
                        reason: reason.to_owned(),
                    });
                }
                ws::ReadStatus::Message(_) => return Err(Error::HelloNotText),
                ws::ReadStatus::Idle => return Err(Error::HandshakeTimeout),
            };
            let hello: GatewayPayload = serde_json::from_str(text)?;
            if hello.op != OP_HELLO {
                return Err(Error::UnexpectedOpcode(hello.op));
            }
            let data: HelloData =
                serde_json::from_str(hello.data.ok_or(Error::MissingPayload)?.get())?;
            data.heartbeat_interval
        };

        // Send RESUME or IDENTIFY via format! — no serde_json allocation.
        // Token and session_id are checked for characters that would need
        // JSON escaping; in practice Discord tokens are base64 + dots.
        let mut write_buf = Vec::new();
        {
            use std::io::Write;
            if is_safe_for_json_literal(token).not() {
                return Err(Error::BadToken);
            }
            match state {
                SessionState::Resuming(ref resume) => {
                    if is_safe_for_json_literal(&resume.session_id).not() {
                        return Err(Error::BadSessionId);
                    }
                    write!(
                        write_buf,
                        r#"{{"op":{},"d":{{"token":"{}","session_id":"{}","seq":{}}}}}"#,
                        OP_RESUME, token, resume.session_id, resume.sequence,
                    )
                }
                SessionState::Identifying(ref connect) => write!(
                    write_buf,
                    r#"{{"op":{},"d":{{"token":"{}","intents":{},"properties":{{"os":"{}","browser":"discord-bot","device":"discord-bot"}}}}}}"#,
                    OP_IDENTIFY, token, connect.intents, std::env::consts::OS,
                ),
            }
            .expect("write to Vec cannot fail");
            ws.send_text(
                std::str::from_utf8(&write_buf).expect("identify/resume payload is ASCII"),
            )?;
        }

        // Jittered first heartbeat
        let mut heartbeat = {
            let mut rng = [0u8; 4];
            getrandom::fill(&mut rng)?;
            let jitter = f64::from(u32::from_le_bytes(rng)) / f64::from(u32::MAX);
            HeartbeatState::new(heartbeat_interval_ms, jitter)?
        };

        // Switch timeouts from the tight handshake deadline to the
        // heartbeat interval.
        ws.inner()
            .get_ref()
            .set_write_timeout(Some(heartbeat.interval()))?;

        // Resolve the session data to use for the connection, either by
        // resuming from an existing session or identifying as a new session,
        // which needs sending an identify payload and polling for the READY event.
        let data = match state {
            SessionState::Resuming(resume) => {
                // Switch from the tight handshake deadline to the
                // heartbeat-driven read timeout before returning.
                ws.inner()
                    .get_ref()
                    .set_read_timeout(Some(heartbeat.poll_timeout()))?;
                resume
            }
            SessionState::Identifying(_) => {
                let deadline = Instant::now() + heartbeat.backstop_window();
                loop {
                    heartbeat.send_if_due(&mut ws, None, &mut write_buf)?;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(Error::HandshakeTimeout);
                    }
                    ws.inner()
                        .get_ref()
                        .set_read_timeout(Some(heartbeat.poll_timeout().min(remaining)))?;
                    match ws.read_message() {
                        Ok(ws::ReadStatus::Message(ws::Message::Text(text))) => match process_text(text)? {
                            GatewayAction::HeartbeatAck => heartbeat.receive_ack(),
                            GatewayAction::SendHeartbeat => {
                                heartbeat.server_requested(&mut ws, None, &mut write_buf)?;
                            }
                            GatewayAction::Dispatch(dispatch) => {
                                let event_type =
                                    dispatch.event_type.ok_or(Error::MalformedDispatch)?;
                                let data = dispatch.data.ok_or(Error::MissingPayload)?;
                                if event_type != EventType::Ready {
                                    return Err(Error::UnexpectedHandshakeDispatch);
                                }
                                let ready: msg::Ready = serde_json::from_str(data)?;
                                break SessData {
                                    session_id: ready.session_id,
                                    resume_gateway_url: ready.resume_gateway_url,
                                    sequence: dispatch.sequence,
                                    bot_user_id: ready.user.id,
                                    guild_ids: ready.guilds.iter().map(|g| g.id).collect(),
                                };
                            }
                        },
                        Ok(ws::ReadStatus::Message(ws::Message::Close(code, reason))) => {
                            return Err(Error::GatewayClosed {
                                code,
                                reason: reason.to_owned(),
                            });
                        }
                        Ok(ws::ReadStatus::Message(ws::Message::Binary(_))) => return Err(Error::UnexpectedBinary),
                        Ok(ws::ReadStatus::Idle) => {
                            heartbeat.send_if_due(&mut ws, None, &mut write_buf)?;
                        }
                        Err(ws::Error::Reconnect(ws::ConnectionError::Closed)) => {
                            return Err(Error::ConnectionClosed);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
        };

        // Assemble session, restoring resume state if present
        Ok(Session {
            ws,
            heartbeat,
            data,
            write_buf,
        })
    }

    /// Read the next gateway message, handling heartbeats and control opcodes internally.
    pub fn poll(&mut self) -> Result<Option<msg::Event>> {
        loop {
            self.heartbeat.send_if_due(
                &mut self.ws,
                Some(self.data.sequence),
                &mut self.write_buf,
            )?;

            // Ignore errors — the previous timeout is still in effect, and
            // if the fd is actually dead the next read will surface it.
            let _ = self
                .ws
                .inner()
                .get_ref()
                .set_read_timeout(Some(self.heartbeat.poll_timeout()));

            match self.ws.read_message() {
                Ok(ws::ReadStatus::Message(ws::Message::Text(text))) => {
                    match process_text(text)? {
                        GatewayAction::HeartbeatAck => self.heartbeat.receive_ack(),
                        GatewayAction::SendHeartbeat => {
                            self.heartbeat.server_requested(
                                &mut self.ws,
                                Some(self.data.sequence),
                                &mut self.write_buf,
                            )?;
                        }
                        GatewayAction::Dispatch(dispatch) => {
                            // Advance the sequence cursor *before* validating
                            // or parsing.  On failure Discord replays the
                            // identical payload on resume — advancing first
                            // loses one unparseable event instead of looping
                            // forever.
                            self.data.sequence = dispatch.sequence;
                            let event_type = dispatch.event_type.ok_or(Error::MalformedDispatch)?;
                            let data = dispatch.data.ok_or(Error::MissingPayload)?;
                            if let Some(event) = parse_event(event_type, data)? {
                                return Ok(Some(event));
                            }
                        }
                    }
                }
                Ok(ws::ReadStatus::Message(ws::Message::Close(code, reason))) => {
                    return Err(Error::GatewayClosed {
                        code,
                        reason: reason.to_owned(),
                    });
                }
                Ok(ws::ReadStatus::Message(ws::Message::Binary(_))) => return Err(Error::UnexpectedBinary),
                Ok(ws::ReadStatus::Idle) => return Ok(None),
                Err(ws::Error::Reconnect(ws::ConnectionError::Closed)) => {
                    return Err(Error::ConnectionClosed);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Consume the session and return its data for a future resume.
    ///
    /// The connection is dropped without a close frame — sending one
    /// would invalidate the session and prevent resume.
    pub fn into_resume_data(self) -> SessData {
        self.data
    }
}

/// Known gateway event types. Unknown events map to `Unknown` and are
/// silently dropped — no allocation needed for the event name.
#[derive(Debug, PartialEq, Eq)]
enum EventType {
    Ready,
    Resumed,
    GuildCreate,
    MessageCreate,
    MessageUpdate,
    MessageDelete,
    ReactionAdd,
    ReactionRemove,
    Unknown,
}

impl EventType {
    fn from_name(s: &str) -> Self {
        match s {
            "READY" => Self::Ready,
            "RESUMED" => Self::Resumed,
            "GUILD_CREATE" => Self::GuildCreate,
            "MESSAGE_CREATE" => Self::MessageCreate,
            "MESSAGE_UPDATE" => Self::MessageUpdate,
            "MESSAGE_DELETE" => Self::MessageDelete,
            "MESSAGE_REACTION_ADD" => Self::ReactionAdd,
            "MESSAGE_REACTION_REMOVE" => Self::ReactionRemove,
            _ => Self::Unknown,
        }
    }
}

/// Parsed dispatch fields, borrowing `data` from the frame text.
///
/// `event_type` and `data` are `Option` so that the sequence number can
/// be extracted and advanced even when the rest of the dispatch is
/// malformed — preventing duplicate replays on resume.
#[derive(Debug)]
struct Dispatch<'a> {
    sequence: u64,
    event_type: Option<EventType>,
    data: Option<&'a str>,
}

#[derive(Debug)]
enum GatewayAction<'a> {
    Dispatch(Dispatch<'a>),
    HeartbeatAck,
    SendHeartbeat,
}

/// Parse a text gateway message into an action.
///
/// Returns an error for reconnect / invalid session / protocol violations.
fn process_text<'a>(text: &'a str) -> Result<GatewayAction<'a>> {
    let payload: GatewayPayload = serde_json::from_str(text)?;
    match payload.op {
        OP_HEARTBEAT_ACK => Ok(GatewayAction::HeartbeatAck),
        OP_HEARTBEAT => Ok(GatewayAction::SendHeartbeat),
        OP_RECONNECT => Err(Error::Reconnect),
        OP_INVALID_SESSION => {
            let resumable = payload.data.is_some_and(|d| d.get() == "true");
            Err(Error::InvalidSession { resumable })
        }
        OP_DISPATCH => {
            let sequence = payload.sequence.ok_or(Error::MalformedDispatch)?;
            Ok(GatewayAction::Dispatch(Dispatch {
                sequence,
                event_type: payload.event_type.map(EventType::from_name),
                data: payload.data.map(|d| d.get()),
            }))
        }
        // Discord's opcode table (0–11) is a closed set with no
        // documented forward-compatibility policy.  Treating an
        // unknown opcode as an error is deliberate.
        _ => Err(Error::UnexpectedOpcode(payload.op)),
    }
}

/// Returns `true` if every byte is printable ASCII (0x20–0x7E) and not
/// `"` or `\` — i.e. the string can be inserted into a JSON `"…"` literal
/// without any escaping.
///
/// This is intentionally byte-level: Discord tokens and session IDs are
/// always ASCII (base64 + dots), so non-ASCII bytes indicate garbage.
const fn is_safe_for_json_literal(s: &str) -> bool {
    let s = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let b = s[i];
        if b == b'"' || b == b'\\' || b < b' ' || b > b'~' {
            return false;
        }
        i += 1;
    }
    true
}

const _: () = assert!(
    is_safe_for_json_literal(std::env::consts::OS),
    "std::env::consts::OS must be safe for JSON literal interpolation",
);

struct GatewayUrl<'a> {
    host: &'a str,
    port: u16,
    path: &'a str,
}

/// Parse a `wss://` gateway URL into host, port, and path components.
fn parse_gateway_url(url: &str) -> Result<GatewayUrl<'_>> {
    let after_scheme = url.strip_prefix("wss://").ok_or(Error::BadResumeUrl)?;
    // Split authority from path+query at the first '/'.
    // A bare query without a leading slash (e.g. `wss://host?v=10`) is
    // rejected — it would produce an invalid HTTP request-target.
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    if authority.contains('?') || authority.contains('[') {
        return Err(Error::BadResumeUrl);
    }
    let path = if authority_end < after_scheme.len() {
        &after_scheme[authority_end..]
    } else {
        "/"
    };
    // Split authority into host and optional port.
    let (host, port) = match authority.rfind(':') {
        Some(i) => {
            let port_str = &authority[i + 1..];
            let p = port_str.parse::<u16>().map_err(|_| Error::BadResumeUrl)?;
            (&authority[..i], p)
        }
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(Error::BadResumeUrl);
    }
    Ok(GatewayUrl { host, port, path })
}

/// Deserialize a dispatch payload into an `Event`, or `Ok(None)` for unrecognized event types.
fn parse_event(event_type: EventType, data: &str) -> Result<Option<msg::Event>> {
    use msg::Event;
    use serde_json::from_str;
    Ok(Some(match event_type {
        EventType::Ready => Event::Ready(from_str(data)?),
        EventType::Resumed => Event::Resumed,
        EventType::GuildCreate => Event::GuildCreate(from_str(data)?),
        EventType::MessageCreate => Event::MessageCreate(from_str(data)?),
        EventType::MessageUpdate => Event::MessageUpdate(from_str(data)?),
        EventType::MessageDelete => Event::MessageDelete(from_str(data)?),
        EventType::ReactionAdd => Event::ReactionAdd(from_str(data)?),
        EventType::ReactionRemove => Event::ReactionRemove(from_str(data)?),
        EventType::Unknown => return Ok(None),
    }))
}
