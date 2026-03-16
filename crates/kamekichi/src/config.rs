use std::fs;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

const DEFAULT_CONFIG: &str = "config.json";
/// How often to stat the config file for modification-time changes.
/// 5 s balances reload responsiveness with the cost of a stat(2) call per poll cycle.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

fn deserialize_snowflake<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let s = String::deserialize(d)?;
    s.parse::<u64>().map_err(serde::de::Error::custom)
}

#[derive(Deserialize, PartialEq)]
pub struct PlatformConfig {
    pub token: String,
    #[serde(deserialize_with = "deserialize_snowflake")]
    pub guild_id: u64,
    #[serde(default)]
    intents: Vec<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

impl PlatformConfig {
    pub fn intent_bits(&self) -> u64 {
        use kamekichi_discord::intent;
        if self.intents.is_empty() {
            return intent::GUILDS
                | intent::GUILD_MESSAGES
                | intent::GUILD_MESSAGE_REACTIONS
                | intent::MESSAGE_CONTENT;
        }
        self.intents.iter().fold(0u64, |bits, name| {
            bits | match name.to_lowercase().as_str() {
                "guilds" => intent::GUILDS,
                "guild_members" => intent::GUILD_MEMBERS,
                "guild_moderation" => intent::GUILD_MODERATION,
                "guild_expressions" => intent::GUILD_EXPRESSIONS,
                "guild_integrations" => intent::GUILD_INTEGRATIONS,
                "guild_webhooks" => intent::GUILD_WEBHOOKS,
                "guild_invites" => intent::GUILD_INVITES,
                "guild_voice_states" => intent::GUILD_VOICE_STATES,
                "guild_presences" => intent::GUILD_PRESENCES,
                "guild_messages" => intent::GUILD_MESSAGES,
                "guild_message_reactions" => intent::GUILD_MESSAGE_REACTIONS,
                "guild_message_typing" => intent::GUILD_MESSAGE_TYPING,
                "direct_messages" => intent::DIRECT_MESSAGES,
                "direct_message_reactions" => intent::DIRECT_MESSAGE_REACTIONS,
                "direct_message_typing" => intent::DIRECT_MESSAGE_TYPING,
                "message_content" => intent::MESSAGE_CONTENT,
                "guild_scheduled_events" => intent::GUILD_SCHEDULED_EVENTS,
                "auto_moderation_configuration" => intent::AUTO_MODERATION_CONFIGURATION,
                "auto_moderation_execution" => intent::AUTO_MODERATION_EXECUTION,
                "guild_message_polls" => intent::GUILD_MESSAGE_POLLS,
                "direct_message_polls" => intent::DIRECT_MESSAGE_POLLS,
                other => {
                    eprintln!("Unknown intent: {other}");
                    0
                }
            }
        })
    }
}

pub struct FullConfig {
    pub platform: PlatformConfig,
    pub log_file: Option<String>,
}

pub fn load(path: &str) -> Result<(FullConfig, serde_json::Value), Box<dyn std::error::Error>> {
    let json = fs::read_to_string(path)?;
    let raw: serde_json::Value = serde_json::from_str(&json)?;
    let platform: PlatformConfig = serde_json::from_value(raw.clone())?;
    let log_file = raw
        .get("log_file")
        .and_then(|v| v.as_str())
        .map(String::from);
    let bot = raw.get("bot").cloned().unwrap_or(serde_json::Value::Null);
    Ok((FullConfig { platform, log_file }, bot))
}

pub fn default_path() -> &'static str {
    DEFAULT_CONFIG
}

pub struct Watcher {
    path: String,
    mtime: SystemTime,
    last_check: Instant,
}

impl Watcher {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            mtime: fs::metadata(path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH),
            last_check: Instant::now(),
        }
    }

    pub fn poll(&mut self) -> Option<(FullConfig, serde_json::Value)> {
        if self.last_check.elapsed() < POLL_INTERVAL {
            return None;
        }
        self.last_check = Instant::now();
        let mtime = fs::metadata(&self.path).and_then(|m| m.modified()).ok()?;
        if mtime == self.mtime {
            return None;
        }
        match load(&self.path) {
            Ok(configs) => {
                self.mtime = mtime;
                Some(configs)
            }
            Err(e) => {
                eprintln!("Failed to reload config: {e}");
                None
            }
        }
    }
}
