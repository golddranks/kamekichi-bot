use std::{cell::RefCell, sync::Arc};

use crate::Error;

mod client;

use client::{Client, Method};

pub struct HttpApi {
    client: Client,
}

impl HttpApi {
    pub(crate) fn new(
        token: String,
        user_agent: String,
        tls_config: Arc<rustls::ClientConfig>,
    ) -> Self {
        Self {
            client: Client {
                host: "discord.com".to_string(),
                tls_config,
                token,
                user_agent,
                conn: RefCell::new(None),
            },
        }
    }

    pub fn send_message(
        &self,
        channel_id: u64,
        content: &str,
    ) -> Result<(u16, Option<u64>), Error> {
        let path = format!("/api/v10/channels/{channel_id}/messages");
        let body = serde_json::json!({ "content": content }).to_string();
        let (status, resp_body) = self.client.request(Method::Post, &path, Some(&body))?;
        let message_id = if (200..300).contains(&status) {
            serde_json::from_str::<serde_json::Value>(&resp_body)
                .ok()
                .and_then(|v| v.get("id")?.as_str()?.parse::<u64>().ok())
        } else {
            None
        };
        Ok((status, message_id))
    }

    pub fn edit_message(
        &self,
        channel_id: u64,
        message_id: u64,
        content: &str,
    ) -> Result<u16, Error> {
        let path = format!("/api/v10/channels/{channel_id}/messages/{message_id}");
        let body = serde_json::json!({ "content": content }).to_string();
        Ok(self.client.request(Method::Patch, &path, Some(&body))?.0)
    }

    pub fn add_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> Result<u16, Error> {
        let emoji_encoded = client::percent_encode(emoji);
        let path = format!(
            "/api/v10/channels/{channel_id}/messages/{message_id}/reactions/{emoji_encoded}/@me"
        );
        Ok(self.client.request(Method::Put, &path, None)?.0)
    }

    /// Fetch the gateway WebSocket URL from `GET /api/v10/gateway/bot`,
    /// with `?v=10&encoding=json` appended.
    pub fn get_gateway_url(&self) -> Result<String, Error> {
        let (status, body) = self
            .client
            .request(Method::Get, "/api/v10/gateway/bot", None)?;
        if !(200..300).contains(&status) {
            return Err(Error::RetriesExhausted { status });
        }
        #[derive(serde::Deserialize)]
        struct GatewayBot {
            url: String,
        }
        let gw: GatewayBot = serde_json::from_str(&body)?;
        Ok(format!("{}/?v=10&encoding=json", gw.url))
    }

    pub fn add_role(&self, guild_id: u64, user_id: u64, role_id: u64) -> Result<u16, Error> {
        let path = format!("/api/v10/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        Ok(self.client.request(Method::Put, &path, None)?.0)
    }

    pub fn remove_role(&self, guild_id: u64, user_id: u64, role_id: u64) -> Result<u16, Error> {
        let path = format!("/api/v10/guilds/{guild_id}/members/{user_id}/roles/{role_id}");
        Ok(self.client.request(Method::Delete, &path, None)?.0)
    }
}
