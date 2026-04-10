use serde::Deserialize;

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
