use std::sync::Arc;

use crate::Error;

mod client;

use client::{Client, Method};

/// Discord REST API client.
///
/// Wraps [`Client`] to provide typed methods for common endpoints.
/// All methods block the calling thread for I/O and rate-limit waits.
pub struct HttpApi {
    client: Client,
}

fn check_status(status: u16) -> Result<(), Error> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(Error::ApiError { status })
    }
}

impl HttpApi {
    pub(crate) fn new(
        token: String,
        user_agent: String,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Self {
        Self {
            client: Client::new(tls_config, token, user_agent),
        }
    }

    /// Post a message to a channel. Returns the new message's snowflake ID,
    /// or `None` if the response didn't contain a parseable `"id"` field.
    pub fn send_message(
        &mut self,
        channel_id: u64,
        content: &str,
    ) -> Result<Option<u64>, Error> {
        let path = format!("/api/v10/channels/{channel_id}/messages");
        let body = serde_json::json!({ "content": content }).to_string();
        let (status, resp_body) = self.client.request(Method::Post, &path, Some(&body))?;
        check_status(status)?;
        let value: serde_json::Value = serde_json::from_str(&resp_body)?;
        let message_id = value
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok());
        Ok(message_id)
    }

    /// Replace a message's content.
    pub fn edit_message(
        &mut self,
        channel_id: u64,
        message_id: u64,
        content: &str,
    ) -> Result<(), Error> {
        let path = format!("/api/v10/channels/{channel_id}/messages/{message_id}");
        let body = serde_json::json!({ "content": content }).to_string();
        let (status, _) = self.client.request(Method::Patch, &path, Some(&body))?;
        check_status(status)
    }

    /// Add a reaction to a message. `emoji` is percent-encoded automatically.
    pub fn add_reaction(
        &mut self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> Result<(), Error> {
        let emoji_encoded = client::percent_encode(emoji);
        let path = format!(
            "/api/v10/channels/{channel_id}/messages/{message_id}/reactions/{emoji_encoded}/@me"
        );
        let (status, _) = self.client.request(Method::Put, &path, None)?;
        check_status(status)
    }

    /// Fetch the gateway WebSocket URL from `GET /api/v10/gateway/bot`,
    /// with `?v=10&encoding=json` appended.
    pub fn get_gateway_url(&mut self) -> Result<String, Error> {
        let (status, body) = self
            .client
            .request(Method::Get, "/api/v10/gateway/bot", None)?;
        check_status(status)?;
        #[derive(serde::Deserialize)]
        struct GatewayBot {
            url: String,
        }
        let gw: GatewayBot = serde_json::from_str(&body)?;
        let base = gw.url.trim_end_matches('/');
        Ok(format!("{base}/?v=10&encoding=json"))
    }

    /// Assign a role to a guild member.
    pub fn add_role(&mut self, guild_id: u64, user_id: u64, role_id: u64) -> Result<(), Error> {
        let path = format!("/api/v10/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        let (status, _) = self.client.request(Method::Put, &path, None)?;
        check_status(status)
    }

    /// Remove a role from a guild member.
    pub fn remove_role(&mut self, guild_id: u64, user_id: u64, role_id: u64) -> Result<(), Error> {
        let path = format!("/api/v10/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        let (status, _) = self.client.request(Method::Delete, &path, None)?;
        check_status(status)
    }
}
