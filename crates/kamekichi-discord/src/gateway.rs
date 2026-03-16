use std::net::TcpStream;
use std::ops::Not;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::value::RawValue;

use kamekichi_ws as ws;
use rand_core::SeedableRng;
use rand_xoshiro::Xoshiro128PlusPlus;

use crate::heartbeat::HeartbeatState;
use crate::{Error, Event, Result, TlsStream};

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
    ws: ws::WebSocket<TlsStream, Xoshiro128PlusPlus>,
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

impl std::error::Error for PollError {}

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
            ws::WebSocket::new(Xoshiro128PlusPlus::seed_from_u64(u64::from_ne_bytes(seed)))
                .connect(rustls::StreamOwned::new(tls_conn, tcp), url.host, &path)?
        };

        // Receive HELLO and extract heartbeat interval
        let heartbeat_interval_ms = {
            let text = match ws.read_message()? {
                Some(ws::Message::Text(p)) => p,
                Some(ws::Message::Close(code, reason)) => {
                    return Err(Error::GatewayClosed {
                        code,
                        reason: reason.to_owned(),
                    });
                }
                Some(_) => return Err(Error::HelloNotText),
                None => return Err(Error::HandshakeTimeout),
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
                        Ok(Some(ws::Message::Text(text))) => match process_text(text)? {
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
                                let ready: crate::Ready = serde_json::from_str(data)?;
                                break SessData {
                                    session_id: ready.session_id,
                                    resume_gateway_url: ready.resume_gateway_url,
                                    sequence: dispatch.sequence,
                                    bot_user_id: ready.user.id,
                                    guild_ids: ready.guilds.iter().map(|g| g.id).collect(),
                                };
                            }
                        },
                        Ok(Some(ws::Message::Close(code, reason))) => {
                            return Err(Error::GatewayClosed {
                                code,
                                reason: reason.to_owned(),
                            });
                        }
                        Ok(Some(ws::Message::Binary(_))) => return Err(Error::UnexpectedBinary),
                        Ok(None) => {
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
    pub fn poll(&mut self) -> Result<Option<Event>> {
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
                Ok(Some(ws::Message::Text(text))) => {
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
                Ok(Some(ws::Message::Close(code, reason))) => {
                    return Err(Error::GatewayClosed {
                        code,
                        reason: reason.to_owned(),
                    });
                }
                Ok(Some(ws::Message::Binary(_))) => return Err(Error::UnexpectedBinary),
                Ok(None) => return Ok(None),
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
fn parse_event(event_type: EventType, data: &str) -> Result<Option<Event>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event_type: &str, data: serde_json::Value) -> Result<Option<Event>> {
        parse_event(EventType::from_name(event_type), &data.to_string())
    }

    #[test]
    fn parse_message_create() {
        let data = serde_json::json!({
            "id": "123",
            "channel_id": "456",
            "author": { "id": "789", "username": "testuser" },
            "content": "hello world"
        });
        match ev("MESSAGE_CREATE", data) {
            Ok(Some(Event::MessageCreate(m))) => {
                assert_eq!(m.id, 123);
                assert_eq!(m.channel_id, 456);
                assert_eq!(m.author.id, 789);
                assert_eq!(m.author.username, "testuser");
                assert_eq!(m.content, "hello world");
            }
            other => panic!("expected MessageCreate, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_update_with_content() {
        let data = serde_json::json!({
            "id": "100",
            "channel_id": "200",
            "content": "edited"
        });
        match ev("MESSAGE_UPDATE", data) {
            Ok(Some(Event::MessageUpdate(m))) => {
                assert_eq!(m.id, 100);
                assert_eq!(m.channel_id, 200);
                assert_eq!(m.content, Some("edited".into()));
            }
            other => panic!("expected MessageUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_update_without_content() {
        let data = serde_json::json!({
            "id": "100",
            "channel_id": "200"
        });
        match ev("MESSAGE_UPDATE", data) {
            Ok(Some(Event::MessageUpdate(m))) => {
                assert_eq!(m.content, None);
            }
            other => panic!("expected MessageUpdate, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_delete() {
        let data = serde_json::json!({
            "id": "100",
            "channel_id": "200"
        });
        match ev("MESSAGE_DELETE", data) {
            Ok(Some(Event::MessageDelete(m))) => {
                assert_eq!(m.id, 100);
                assert_eq!(m.channel_id, 200);
            }
            other => panic!("expected MessageDelete, got {other:?}"),
        }
    }

    #[test]
    fn parse_reaction_add_unicode() {
        let data = serde_json::json!({
            "user_id": "1",
            "channel_id": "2",
            "message_id": "3",
            "emoji": { "name": "\u{1f44d}", "id": null }
        });
        match ev("MESSAGE_REACTION_ADD", data) {
            Ok(Some(Event::ReactionAdd(r))) => {
                assert_eq!(r.emoji.to_string(), "\u{1f44d}");
            }
            other => panic!("expected ReactionAdd, got {other:?}"),
        }
    }

    #[test]
    fn parse_reaction_remove_custom() {
        let data = serde_json::json!({
            "user_id": "1",
            "channel_id": "2",
            "message_id": "3",
            "emoji": { "name": "kame", "id": "999" }
        });
        match ev("MESSAGE_REACTION_REMOVE", data) {
            Ok(Some(Event::ReactionRemove(r))) => {
                assert_eq!(r.emoji.to_string(), "kame:999");
            }
            other => panic!("expected ReactionRemove, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event() {
        let data = serde_json::json!({});
        assert!(matches!(ev("TYPING_START", data), Ok(None)));
    }

    #[test]
    fn parse_message_create_missing_author() {
        let data = serde_json::json!({
            "id": "123",
            "channel_id": "456",
            "content": "hello"
        });
        assert!(ev("MESSAGE_CREATE", data).is_err());
    }

    #[test]
    fn parse_message_create_null_content() {
        let data = serde_json::json!({
            "id": "123",
            "channel_id": "456",
            "author": { "id": "789", "username": "bot" },
            "content": null
        });
        match ev("MESSAGE_CREATE", data) {
            Ok(Some(Event::MessageCreate(m))) => {
                assert_eq!(m.content, "");
            }
            other => panic!("expected MessageCreate, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_create_missing_content() {
        let data = serde_json::json!({
            "id": "123",
            "channel_id": "456",
            "author": { "id": "789", "username": "bot" }
        });
        match ev("MESSAGE_CREATE", data) {
            Ok(Some(Event::MessageCreate(m))) => {
                assert_eq!(m.content, "");
            }
            other => panic!("expected MessageCreate, got {other:?}"),
        }
    }

    #[test]
    fn parse_message_create_missing_username() {
        let data = serde_json::json!({
            "id": "123",
            "channel_id": "456",
            "author": { "id": "789" },
            "content": "hello"
        });
        match ev("MESSAGE_CREATE", data) {
            Ok(Some(Event::MessageCreate(m))) => {
                assert_eq!(m.author.username, "");
                assert_eq!(m.content, "hello");
            }
            other => panic!("expected MessageCreate, got {other:?}"),
        }
    }

    #[test]
    fn parse_ready() {
        let data = serde_json::json!({
            "user": { "id": "111" },
            "session_id": "abc",
            "resume_gateway_url": "wss://gateway-us-east1-b.discord.gg",
            "guilds": [{ "id": "100" }, { "id": "200" }]
        });
        match ev("READY", data) {
            Ok(Some(Event::Ready(r))) => {
                assert_eq!(r.session_id, "abc");
                assert_eq!(r.resume_gateway_url, "wss://gateway-us-east1-b.discord.gg");
                assert_eq!(r.user.id, 111);
                let ids: Vec<u64> = r.guilds.iter().map(|g| g.id).collect();
                assert_eq!(ids, vec![100, 200]);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn parse_ready_missing_session_id() {
        let data = serde_json::json!({
            "user": { "id": "111" },
            "resume_gateway_url": "wss://gw.discord.gg",
            "guilds": []
        });
        assert!(ev("READY", data).is_err());
    }

    #[test]
    fn parse_ready_missing_user() {
        let data = serde_json::json!({
            "session_id": "abc",
            "resume_gateway_url": "wss://gw.discord.gg",
            "guilds": []
        });
        assert!(ev("READY", data).is_err());
    }

    #[test]
    fn parse_ready_missing_resume_url() {
        let data = serde_json::json!({
            "user": { "id": "111" },
            "session_id": "abc",
            "guilds": []
        });
        assert!(ev("READY", data).is_err());
    }

    #[test]
    fn parse_resumed() {
        let data = serde_json::json!({});
        assert!(matches!(ev("RESUMED", data), Ok(Some(Event::Resumed))));
    }

    #[test]
    fn parse_guild_create() {
        let data = serde_json::json!({
            "id": "100",
            "channels": [
                { "id": "1", "name": "general" },
                { "id": "2", "name": "bot" }
            ],
            "roles": [
                { "id": "10", "name": "admin" }
            ]
        });
        match ev("GUILD_CREATE", data) {
            Ok(Some(Event::GuildCreate(gc))) => {
                assert_eq!(gc.id, 100);
                let channels: Vec<(u64, &str)> = gc
                    .channels
                    .iter()
                    .map(|c| (c.id, c.name.as_str()))
                    .collect();
                assert_eq!(channels, vec![(1, "general"), (2, "bot")]);
                let roles: Vec<(u64, &str)> =
                    gc.roles.iter().map(|r| (r.id, r.name.as_str())).collect();
                assert_eq!(roles, vec![(10, "admin")]);
            }
            other => panic!("expected GuildCreate, got {other:?}"),
        }
    }

    #[test]
    fn parse_known_event_bad_data_returns_error() {
        let data = serde_json::json!({ "not": "a valid message" });
        let err = ev("MESSAGE_CREATE", data).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing field"),
            "expected missing-field error, got: {msg}"
        );
    }

    #[test]
    fn parse_reaction_bad_data_returns_error() {
        let data = serde_json::json!({ "user_id": "1" });
        assert!(ev("MESSAGE_REACTION_ADD", data).is_err());
    }

    #[test]
    fn parse_guild_create_bad_snowflake_returns_error() {
        let data = serde_json::json!({ "id": "not_a_number" });
        assert!(ev("GUILD_CREATE", data).is_err());
    }

    // ---- process_text ----

    fn gw(op: u8, d: Option<serde_json::Value>, s: Option<u64>, t: Option<&str>) -> String {
        serde_json::json!({ "op": op, "d": d, "s": s, "t": t }).to_string()
    }

    #[test]
    fn process_heartbeat_ack() {
        assert!(matches!(
            process_text(&gw(OP_HEARTBEAT_ACK, None, None, None)),
            Ok(GatewayAction::HeartbeatAck)
        ));
    }

    #[test]
    fn process_server_heartbeat() {
        assert!(matches!(
            process_text(&gw(OP_HEARTBEAT, None, None, None)),
            Ok(GatewayAction::SendHeartbeat)
        ));
    }

    #[test]
    fn process_reconnect() {
        assert!(matches!(
            process_text(&gw(OP_RECONNECT, None, None, None)),
            Err(Error::Reconnect)
        ));
    }

    #[test]
    fn process_invalid_session_resumable() {
        let text = gw(
            OP_INVALID_SESSION,
            Some(serde_json::json!(true)),
            None,
            None,
        );
        assert!(matches!(
            process_text(&text),
            Err(Error::InvalidSession { resumable: true })
        ));
    }

    #[test]
    fn process_invalid_session_not_resumable() {
        let text = gw(
            OP_INVALID_SESSION,
            Some(serde_json::json!(false)),
            None,
            None,
        );
        assert!(matches!(
            process_text(&text),
            Err(Error::InvalidSession { resumable: false })
        ));
    }

    #[test]
    fn process_invalid_session_null_data() {
        let text = gw(OP_INVALID_SESSION, None, None, None);
        assert!(matches!(
            process_text(&text),
            Err(Error::InvalidSession { resumable: false })
        ));
    }

    #[test]
    fn process_dispatch() {
        let text = gw(
            OP_DISPATCH,
            Some(serde_json::json!({"content": "hi"})),
            Some(42),
            Some("MESSAGE_CREATE"),
        );
        match process_text(&text).unwrap() {
            GatewayAction::Dispatch(d) => {
                assert_eq!(d.sequence, 42);
                assert_eq!(d.event_type, Some(EventType::MessageCreate));
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn process_dispatch_missing_sequence() {
        let text = gw(
            OP_DISPATCH,
            Some(serde_json::json!({})),
            None,
            Some("READY"),
        );
        assert!(matches!(process_text(&text), Err(Error::MalformedDispatch)));
    }

    #[test]
    fn process_dispatch_missing_event_type() {
        let text = gw(OP_DISPATCH, Some(serde_json::json!({})), Some(1), None);
        match process_text(&text).unwrap() {
            GatewayAction::Dispatch(d) => {
                assert_eq!(d.sequence, 1);
                assert!(d.event_type.is_none());
                assert!(d.data.is_some());
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn process_dispatch_missing_data() {
        let text = gw(OP_DISPATCH, None, Some(1), Some("READY"));
        match process_text(&text).unwrap() {
            GatewayAction::Dispatch(d) => {
                assert_eq!(d.sequence, 1);
                assert_eq!(d.event_type, Some(EventType::Ready));
                assert!(d.data.is_none());
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn process_unknown_opcode() {
        let text = gw(99, None, None, None);
        assert!(matches!(
            process_text(&text),
            Err(Error::UnexpectedOpcode(99))
        ));
    }

    #[test]
    fn process_malformed_json() {
        assert!(matches!(process_text("not json"), Err(Error::Json(_))));
    }

    // ---- parse_gateway_url ----

    #[test]
    fn gateway_url_host_only() {
        let u = parse_gateway_url("wss://gateway-us-east1-b.discord.gg").unwrap();
        assert_eq!(u.host, "gateway-us-east1-b.discord.gg");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn gateway_url_with_path() {
        let u = parse_gateway_url("wss://gw.discord.gg/?v=10&encoding=json").unwrap();
        assert_eq!(u.host, "gw.discord.gg");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/?v=10&encoding=json");
    }

    #[test]
    fn gateway_url_with_port() {
        let u = parse_gateway_url("wss://gw.discord.gg:8443/ws").unwrap();
        assert_eq!(u.host, "gw.discord.gg");
        assert_eq!(u.port, 8443);
        assert_eq!(u.path, "/ws");
    }

    #[test]
    fn gateway_url_with_port_no_path() {
        let u = parse_gateway_url("wss://gw.discord.gg:443").unwrap();
        assert_eq!(u.host, "gw.discord.gg");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn gateway_url_query_no_slash() {
        assert!(parse_gateway_url("wss://gw.discord.gg?v=10").is_err());
    }

    #[test]
    fn gateway_url_bad_scheme() {
        assert!(parse_gateway_url("https://gw.discord.gg").is_err());
    }

    #[test]
    fn gateway_url_bad_port() {
        assert!(parse_gateway_url("wss://gw.discord.gg:notaport/path").is_err());
    }

    #[test]
    fn gateway_url_empty_host() {
        assert!(parse_gateway_url("wss://").is_err());
        assert!(parse_gateway_url("wss://:443/path").is_err());
    }

    #[test]
    fn gateway_url_ipv6_rejected() {
        assert!(parse_gateway_url("wss://[::1]:443/path").is_err());
        assert!(parse_gateway_url("wss://[::1]/path").is_err());
    }

    // ---- is_safe_for_json_literal ----

    #[test]
    fn json_literal_safe_token() {
        assert!(is_safe_for_json_literal(
            "Bot MTIzNDU2Nzg5.abc123.xyz_ABC-789"
        ));
    }

    #[test]
    fn json_literal_rejects_quote() {
        assert!(is_safe_for_json_literal(r#"has"quote"#).not());
    }

    #[test]
    fn json_literal_rejects_backslash() {
        assert!(is_safe_for_json_literal(r"has\slash").not());
    }

    #[test]
    fn json_literal_rejects_non_ascii() {
        assert!(is_safe_for_json_literal("caf\u{e9}").not());
    }

    #[test]
    fn json_literal_rejects_control_chars() {
        assert!(is_safe_for_json_literal("has\nnewline").not());
        assert!(is_safe_for_json_literal("has\0null").not());
        assert!(is_safe_for_json_literal("has\ttab").not());
    }

    #[test]
    fn json_literal_accepts_empty() {
        assert!(is_safe_for_json_literal(""));
    }

    #[test]
    fn json_literal_accepts_all_safe_ascii() {
        let safe: String = (b' '..=b'~')
            .filter(|&b| b != b'"' && b != b'\\')
            .map(|b| b as char)
            .collect();
        assert!(is_safe_for_json_literal(&safe));
    }

    #[test]
    fn json_literal_os_constant_is_safe() {
        assert!(
            is_safe_for_json_literal(std::env::consts::OS),
            "std::env::consts::OS ({:?}) must be safe for JSON literal interpolation",
            std::env::consts::OS,
        );
    }

    // ---- classify_error ----

    #[test]
    fn classify_fatal_close_codes() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert_eq!(
                classify_error(&Error::GatewayClosed {
                    code: Some(code),
                    reason: String::new()
                }),
                ErrorClass::Fatal,
                "code {code}"
            );
        }
    }

    #[test]
    fn classify_non_resumable_close_codes() {
        for code in [4007, 4009] {
            assert_eq!(
                classify_error(&Error::GatewayClosed {
                    code: Some(code),
                    reason: String::new()
                }),
                ErrorClass::Reconnect,
                "code {code}"
            );
        }
    }

    #[test]
    fn classify_other_close_codes_resume() {
        for code in [4000, 4001, 4002, 4003, 4005, 4008] {
            assert_eq!(
                classify_error(&Error::GatewayClosed {
                    code: Some(code),
                    reason: String::new()
                }),
                ErrorClass::Resume,
                "code {code}"
            );
        }
    }

    #[test]
    fn classify_connection_errors_resume() {
        assert_eq!(classify_error(&Error::Reconnect), ErrorClass::Resume);
        assert_eq!(
            classify_error(&Error::ConnectionZombied),
            ErrorClass::Resume
        );
        assert_eq!(classify_error(&Error::ConnectionClosed), ErrorClass::Resume);
        assert_eq!(
            classify_error(&Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                ""
            ))),
            ErrorClass::Resume
        );
        assert_eq!(
            classify_error(&Error::WebSocket(kamekichi_ws::Error::Reconnect(
                kamekichi_ws::ConnectionError::Closed
            ))),
            ErrorClass::Resume
        );
        assert_eq!(
            classify_error(&Error::InvalidSession { resumable: true }),
            ErrorClass::Resume
        );
    }

    #[test]
    fn classify_invalid_session_not_resumable() {
        assert_eq!(
            classify_error(&Error::InvalidSession { resumable: false }),
            ErrorClass::Reconnect
        );
    }

    #[test]
    fn classify_transient_errors() {
        assert_eq!(
            classify_error(&Error::Json(serde_json::from_str::<()>("x").unwrap_err())),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_error(&Error::UnexpectedOpcode(99)),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_error(&Error::UnexpectedBinary),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_error(&Error::MalformedDispatch),
            ErrorClass::Transient
        );
        assert_eq!(
            classify_error(&Error::MissingPayload),
            ErrorClass::Transient
        );
    }
}
