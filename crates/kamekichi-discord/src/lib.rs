#![forbid(unsafe_code)]

use std::net::TcpStream;
use std::sync::Arc;

use serde::Deserialize;
type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

mod gateway;
mod heartbeat;
mod http;

pub use gateway::PollError;
pub use gateway::SessData;

use crate::gateway::{ConnectData, SessionState};
use crate::http::HttpClient;

fn deserialize_snowflake<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<u64, D::Error> {
    use serde::de::Error as _;
    String::deserialize(d)?.parse().map_err(D::Error::custom)
}

fn deserialize_optional_snowflake<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<u64>, D::Error> {
    use serde::de::Error as _;
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(D::Error::custom),
    }
}

fn deserialize_string_or_null<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Gateway intent bitflags for the IDENTIFY payload.
pub mod intent {
    pub const GUILDS: u64 = 1 << 0;
    pub const GUILD_MEMBERS: u64 = 1 << 1;
    pub const GUILD_MODERATION: u64 = 1 << 2;
    pub const GUILD_EXPRESSIONS: u64 = 1 << 3;
    pub const GUILD_INTEGRATIONS: u64 = 1 << 4;
    pub const GUILD_WEBHOOKS: u64 = 1 << 5;
    pub const GUILD_INVITES: u64 = 1 << 6;
    pub const GUILD_VOICE_STATES: u64 = 1 << 7;
    pub const GUILD_PRESENCES: u64 = 1 << 8;
    pub const GUILD_MESSAGES: u64 = 1 << 9;
    pub const GUILD_MESSAGE_REACTIONS: u64 = 1 << 10;
    pub const GUILD_MESSAGE_TYPING: u64 = 1 << 11;
    pub const DIRECT_MESSAGES: u64 = 1 << 12;
    pub const DIRECT_MESSAGE_REACTIONS: u64 = 1 << 13;
    pub const DIRECT_MESSAGE_TYPING: u64 = 1 << 14;
    pub const MESSAGE_CONTENT: u64 = 1 << 15;
    pub const GUILD_SCHEDULED_EVENTS: u64 = 1 << 16;
    pub const AUTO_MODERATION_CONFIGURATION: u64 = 1 << 20;
    pub const AUTO_MODERATION_EXECUTION: u64 = 1 << 21;
    pub const GUILD_MESSAGE_POLLS: u64 = 1 << 24;
    pub const DIRECT_MESSAGE_POLLS: u64 = 1 << 25;
}

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

impl std::error::Error for Error {}

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

impl From<kamekichi_ws::ConnectionError> for Error {
    fn from(e: kamekichi_ws::ConnectionError) -> Self {
        Error::WebSocket(kamekichi_ws::Error::Reconnect(e))
    }
}

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Discord bot client managing a gateway session and HTTP requests.
pub struct Client {
    tls: std::sync::Arc<rustls::ClientConfig>,
    http: HttpClient,
    session: Option<gateway::Session>,
}

impl Client {
    pub fn new(token: String, user_agent: Option<String>) -> Self {
        let tls = {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            std::sync::Arc::new(
                rustls::ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth(),
            )
        };
        let http = {
            let user_agent =
                user_agent.unwrap_or_else(|| "DiscordBot (kamekichi-discord, 0.1)".to_owned());
            HttpClient::new(token, user_agent, Arc::clone(&tls))
        };
        Client {
            tls,
            http,
            session: None,
        }
    }

    /// Establish a gateway connection, optionally resuming a previous session.
    pub fn connect(&mut self, intents: u64, resume: Option<SessData>) -> Result {
        let state = match resume {
            Some(sess) => SessionState::Resuming(sess),
            None => SessionState::Identifying(ConnectData {
                gateway_url: self.http.get_gateway_url()?,
                intents,
            }),
        };
        self.session = Some(gateway::Session::connect(
            Arc::clone(&self.tls),
            &self.http.token,
            state,
        )?);
        Ok(())
    }

    /// Poll the gateway for the next event.
    ///
    /// Returns `Ok` for events and idle timeouts.  Errors are classified
    /// by the action the caller should take — see [`PollError`].
    pub fn poll(&mut self) -> std::result::Result<Option<Event>, PollError> {
        let session = self.session.as_mut().ok_or(PollError::NotConnected)?;
        match session.poll() {
            Ok(v) => Ok(v),
            Err(e) => {
                let session = self
                    .session
                    .take()
                    .expect("session was Some at the start of poll()");
                let (err, returned) = gateway::classify_poll_error(e, session);
                self.session = returned;
                Err(err)
            }
        }
    }

    /// Returns the bot's user ID from the current session.
    pub fn bot_user_id(&self) -> Result<u64> {
        Ok(self
            .session
            .as_ref()
            .ok_or(Error::NotConnected)?
            .data
            .bot_user_id)
    }

    /// Returns the guild IDs from the READY payload.
    pub fn guild_ids(&self) -> Result<&[u64]> {
        Ok(self
            .session
            .as_ref()
            .ok_or(Error::NotConnected)?
            .data
            .guild_ids
            .as_slice())
    }

    pub fn send_message(&self, channel_id: u64, content: &str) -> Result<(u16, Option<u64>)> {
        self.http.send_message(channel_id, content)
    }

    pub fn edit_message(&self, channel_id: u64, message_id: u64, content: &str) -> Result<u16> {
        self.http.edit_message(channel_id, message_id, content)
    }

    pub fn add_reaction(&self, channel_id: u64, message_id: u64, emoji: &str) -> Result<u16> {
        self.http.add_reaction(channel_id, message_id, emoji)
    }

    pub fn add_role(&self, guild_id: u64, user_id: u64, role_id: u64) -> Result<u16> {
        self.http.add_role(guild_id, user_id, role_id)
    }

    pub fn remove_role(&self, guild_id: u64, user_id: u64, role_id: u64) -> Result<u16> {
        self.http.remove_role(guild_id, user_id, role_id)
    }
}

/// A message author (user ID and username).
#[derive(Debug, Deserialize)]
pub struct Author {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub username: String,
}

/// A reaction emoji — either a Unicode emoji (name only) or a custom
/// guild emoji (name + snowflake ID).
#[derive(Debug, Deserialize)]
pub struct Emoji {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_snowflake")]
    pub id: Option<u64>,
}

impl std::fmt::Display for Emoji {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.name.as_deref(), self.id) {
            (Some(name), Some(id)) => write!(f, "{name}:{id}"),
            (Some(name), None) => f.write_str(name),
            (None, Some(id)) => write!(f, "_{id}"),
            (None, None) => Ok(()),
        }
    }
}

/// An unavailable guild stub from the READY payload (ID only).
#[derive(Debug, Deserialize)]
pub struct PartialGuild {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
}

/// A guild channel (from GUILD_CREATE).
#[derive(Debug, Deserialize)]
pub struct Channel {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    pub name: String,
}

/// A guild role (from GUILD_CREATE).
#[derive(Debug, Deserialize)]
pub struct Role {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    pub name: String,
}

/// The READY event payload, received after a successful IDENTIFY.
#[derive(Debug, Deserialize)]
pub struct Ready {
    pub user: Author,
    #[serde(default)]
    pub guilds: Vec<PartialGuild>,
    pub session_id: String,
    pub resume_gateway_url: String,
}

/// A GUILD_CREATE event — full guild snapshot with channels and roles.
#[derive(Debug, Deserialize)]
pub struct GuildCreate {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub roles: Vec<Role>,
}

/// A MESSAGE_REACTION_ADD or MESSAGE_REACTION_REMOVE event.
#[derive(Debug, Deserialize)]
pub struct Reaction {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub user_id: u64,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub channel_id: u64,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub message_id: u64,
    pub emoji: Emoji,
}

/// A MESSAGE_CREATE event.
#[derive(Debug, Deserialize)]
pub struct MessageCreate {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub channel_id: u64,
    pub author: Author,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub content: String,
}

/// A MESSAGE_UPDATE event. Fields other than `id` and `channel_id` are
/// optional — Discord sends partial updates.
#[derive(Debug, Deserialize)]
pub struct MessageUpdate {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub channel_id: u64,
    #[serde(default)]
    pub content: Option<String>,
}

/// A MESSAGE_DELETE event.
#[derive(Debug, Deserialize)]
pub struct MessageDelete {
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub id: u64,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub channel_id: u64,
}

/// A gateway dispatch event returned by [`Client::poll`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    Ready(Ready),
    Resumed,
    GuildCreate(GuildCreate),
    ReactionAdd(Reaction),
    ReactionRemove(Reaction),
    MessageCreate(MessageCreate),
    MessageUpdate(MessageUpdate),
    MessageDelete(MessageDelete),
}
