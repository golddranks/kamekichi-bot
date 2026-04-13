#![forbid(unsafe_code)]

use std::net::TcpStream;
use std::sync::Arc;

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

mod error;
mod gateway;
mod heartbeat;
mod http_api;
mod message;

pub use gateway::{PollError, SessData};

pub use error::Error;
use error::{Result, StdResult};
use gateway::{ConnectData, Session, SessionState};
use http_api::HttpApi;
pub use message::{Channel, Event, Role};

/// Discord bot client managing a gateway session and HTTP requests.
pub struct Client {
    tls: Arc<rustls::ClientConfig>,
    http: HttpApi,
    session: Option<Session>,
    token: String,
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
            HttpApi::new(token.clone(), user_agent, Arc::clone(&tls))
        };
        Client {
            tls,
            http,
            session: None,
            token,
        }
    }

    /// Establish a gateway connection.
    pub fn connect(&mut self, intents: u64) -> Result {
        self.session = Some(gateway::Session::connect(
            Arc::clone(&self.tls),
            &self.token,
            SessionState::Identifying(ConnectData {
                gateway_url: self.http.get_gateway_url()?,
                intents,
            }),
        )?);
        Ok(())
    }

    /// Resume a gateway connection using a previous session.
    pub fn resume(&mut self, resume: SessData) -> Result {
        self.session = Some(gateway::Session::connect(
            Arc::clone(&self.tls),
            &self.token,
            SessionState::Resuming(resume),
        )?);
        Ok(())
    }

    /// Poll the gateway for the next event.
    ///
    /// Returns `Ok` for events and idle timeouts.  Errors are classified
    /// by the action the caller should take — see [`PollError`].
    pub fn poll(&mut self) -> StdResult<Option<Event>, PollError> {
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
