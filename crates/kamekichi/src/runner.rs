use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::config::FullConfig;
use crate::refs::{ChannelRef, MessageRef, RefRegistry, RefRegistryScope, UserRef};
use crate::state::State;
use crate::types::Action;
use crate::{Bot, Event, Host};

const STATE_FILE: &str = "state.tsv";

enum SessionExit {
    /// Recoverable — retry (with or without resume data).
    Retry(Box<dyn std::error::Error>),
    /// Configuration error — do not retry.
    Fatal(Box<dyn std::error::Error>),
}

impl From<kamekichi_discord::Error> for SessionExit {
    fn from(e: kamekichi_discord::Error) -> Self {
        SessionExit::Retry(Box::new(e))
    }
}

pub fn run<B: Bot>() -> crate::Result {
    let config_path = crate::config::default_path();
    let (config, bot_config) = crate::config::load(config_path)?;
    let scope = RefRegistryScope::new();
    let bot = B::new(serde_json::from_value(bot_config)?)?;
    let registry = scope.into_registry();
    run_inner(bot, registry, config, config_path)
}

pub fn run_bot(bot: impl Bot, registry: RefRegistry, config_path: &str) -> crate::Result {
    let (config, _) = crate::config::load(config_path)?;
    run_inner(bot, registry, config, config_path)
}

fn run_inner(
    mut bot: impl Bot,
    mut registry: RefRegistry,
    mut config: FullConfig,
    config_path: &str,
) -> crate::Result {
    let mut discord = kamekichi_discord::Client::new(
        config.platform.token.clone(),
        config.platform.user_agent.clone(),
    );

    let mut log = crate::logger::Logger::open(config.log_file.as_ref());
    let mut state = crate::state::load(STATE_FILE);

    let mut resume_data: Option<kamekichi_discord::SessData> = None;
    let mut channels = ChannelRoleTracker::new();

    loop {
        match run_session(
            &mut bot,
            &mut registry,
            &mut log,
            &mut state,
            &mut config,
            &mut discord,
            config_path,
            &mut resume_data,
            &mut channels,
        ) {
            Ok(()) => unreachable!(),
            Err(SessionExit::Retry(e)) => {
                log.flush();
                if resume_data.is_some() {
                    eprintln!("Gateway error: {e}, attempting resume in 1s...");
                    std::thread::sleep(Duration::from_secs(1));
                } else {
                    eprintln!("Gateway error: {e}, reconnecting in 5s...");
                    std::thread::sleep(Duration::from_secs(5));
                    channels.clear();
                }
            }
            Err(SessionExit::Fatal(e)) => {
                log.flush();
                eprintln!("Fatal gateway error: {e}");
                return Err(e);
            }
        }
    }
}

fn save_state(state: &State) {
    crate::state::save(STATE_FILE, state);
}

fn convert_event(raw: &kamekichi_discord::Event) -> Option<Event> {
    match raw {
        kamekichi_discord::Event::ReactionAdd(r) | kamekichi_discord::Event::ReactionRemove(r) => {
            Some(Event::Reaction {
                user: UserRef::from_id(r.user_id),
                channel: ChannelRef::from_id(r.channel_id),
                message: MessageRef::from_id(r.message_id),
                emoji: r.emoji.to_string(),
                added: matches!(raw, kamekichi_discord::Event::ReactionAdd(_)),
            })
        }
        kamekichi_discord::Event::MessageCreate(m) => Some(Event::MessageCreate {
            message_id: MessageRef::from_id(m.id),
            channel: ChannelRef::from_id(m.channel_id),
            author: UserRef::from_id(m.author.id),
            author_name: m.author.username.clone(),
            content: m.content.clone(),
        }),
        kamekichi_discord::Event::MessageUpdate(m) => Some(Event::MessageUpdate {
            message: MessageRef::from_id(m.id),
            channel: ChannelRef::from_id(m.channel_id),
            content: m.content.clone(),
        }),
        kamekichi_discord::Event::MessageDelete(m) => Some(Event::MessageDelete {
            message: MessageRef::from_id(m.id),
            channel: ChannelRef::from_id(m.channel_id),
        }),
        _ => None,
    }
}

struct ChannelRoleTracker {
    channels: HashMap<String, u64>,
    channel_ids: HashMap<u64, String>,
    channel_name_ids: HashMap<String, Vec<u64>>,
    roles: HashMap<String, u64>,
    role_ids: HashMap<u64, String>,
    role_name_ids: HashMap<String, Vec<u64>>,
    ambiguous_channels: HashSet<String>,
    ambiguous_roles: HashSet<String>,
}

impl ChannelRoleTracker {
    fn new() -> Self {
        Self {
            channels: HashMap::new(),
            channel_ids: HashMap::new(),
            channel_name_ids: HashMap::new(),
            roles: HashMap::new(),
            role_ids: HashMap::new(),
            role_name_ids: HashMap::new(),
            ambiguous_channels: HashSet::new(),
            ambiguous_roles: HashSet::new(),
        }
    }

    fn clear(&mut self) {
        self.channels.clear();
        self.channel_ids.clear();
        self.channel_name_ids.clear();
        self.roles.clear();
        self.role_ids.clear();
        self.role_name_ids.clear();
        self.ambiguous_channels.clear();
        self.ambiguous_roles.clear();
    }

    fn insert_channel(&mut self, id: u64, name: &str) {
        if self.channel_ids.contains_key(&id) {
            return;
        }
        self.channel_ids.insert(id, name.to_string());
        let ids = self.channel_name_ids.entry(name.to_string()).or_default();
        ids.push(id);
        match ids.len() {
            1 => {
                self.channels.entry(name.to_string()).or_insert(id);
            }
            _ => {
                self.ambiguous_channels.insert(name.to_string());
            }
        }
    }

    fn insert_role(&mut self, id: u64, name: &str) {
        if self.role_ids.contains_key(&id) {
            return;
        }
        self.role_ids.insert(id, name.to_string());
        let ids = self.role_name_ids.entry(name.to_string()).or_default();
        ids.push(id);
        match ids.len() {
            1 => {
                self.roles.entry(name.to_string()).or_insert(id);
            }
            _ => {
                self.ambiguous_roles.insert(name.to_string());
            }
        }
    }

    fn load_guild(
        &mut self,
        channels: &[kamekichi_discord::Channel],
        roles: &[kamekichi_discord::Role],
    ) {
        for ch in channels {
            self.insert_channel(ch.id, &ch.name);
        }
        eprintln!("Loaded {} channels", self.channel_ids.len());
        for r in roles {
            self.insert_role(r.id, &r.name);
        }
        eprintln!("Loaded {} roles", self.role_ids.len());
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    bot: &mut impl Bot,
    registry: &mut RefRegistry,
    log: &mut crate::logger::Logger,
    state: &mut State,
    config: &mut FullConfig,
    discord: &mut kamekichi_discord::Client,
    config_path: &str,
    resume_data: &mut Option<kamekichi_discord::SessData>,
    tracker: &mut ChannelRoleTracker,
) -> Result<(), SessionExit> {
    if let Some(data) = resume_data.take() {
        discord.resume(data)?;
    } else {
        discord.connect(config.platform.intent_bits())?;
    }
    if !discord.guild_ids()?.contains(&config.platform.guild_id) {
        return Err(SessionExit::Fatal(
            format!(
                "guild {} not found in READY guilds list",
                config.platform.guild_id
            )
            .into(),
        ));
    }
    let mut watcher = crate::config::Watcher::new(config_path);

    loop {
        let event = if let Some((new_config, bot_config)) = watcher.poll() {
            let platform_changed = new_config.platform.token != config.platform.token
                || new_config.platform.guild_id != config.platform.guild_id
                || new_config.platform.intent_bits() != config.platform.intent_bits()
                || new_config.log_file != config.log_file;
            if platform_changed {
                eprintln!(
                    "Warning: platform config changes (token, guild_id, intents, log_file) require a restart"
                );
            }
            let scope = RefRegistryScope::new();
            match serde_json::from_value(bot_config)
                .map_err(Into::into)
                .and_then(|c| Bot::new(c))
            {
                Ok(new_bot) => {
                    *bot = new_bot;
                    *registry = scope.into_registry();
                    eprintln!("Config reloaded");
                    Some(Event::Started)
                }
                Err(e) => {
                    // `scope` is dropped here, cleaning up the thread-local automatically.
                    drop(scope);
                    eprintln!("Config reload failed: {e}");
                    None
                }
            }
        } else {
            match discord.poll() {
                Ok(Some(raw)) => match &raw {
                    kamekichi_discord::Event::Resumed => Some(Event::Started),
                    kamekichi_discord::Event::GuildCreate(gc) => {
                        if gc.id == config.platform.guild_id {
                            tracker.load_guild(&gc.channels, &gc.roles);
                            Some(Event::Started)
                        } else {
                            None
                        }
                    }
                    _ => convert_event(&raw),
                },
                Ok(None) => None,
                Err(kamekichi_discord::PollError::Resume { error, session }) => {
                    *resume_data = Some(*session);
                    return Err(SessionExit::Retry(Box::new(error)));
                }
                Err(kamekichi_discord::PollError::Reconnect(error)) => {
                    return Err(SessionExit::Retry(Box::new(error)));
                }
                Err(kamekichi_discord::PollError::Fatal(error)) => {
                    return Err(SessionExit::Fatal(Box::new(error)));
                }
                Err(kamekichi_discord::PollError::Transient(error)) => {
                    eprintln!("Warning: {error}");
                    None
                }
                Err(kamekichi_discord::PollError::NotConnected) => {
                    return Err(SessionExit::Fatal("not connected".into()));
                }
            }
        };

        if matches!(event, Some(Event::Started)) {
            // Seed state channels/roles from gateway for names not already in state.
            // Existing state entries take priority: they were persisted deliberately
            // and should not be overwritten by an ambiguous gateway mapping.
            let mut state_changed = false;
            for name in registry.channel_names() {
                if !state.channels.contains_key(name) {
                    if tracker.ambiguous_channels.contains(name) {
                        eprintln!(
                            "Warning: stable name '{name}' matches multiple channels; add it to state.tsv to disambiguate"
                        );
                    } else if let Some(&id) = tracker.channels.get(name) {
                        eprintln!("Resolved channel '{name}' -> {id} from gateway");
                        state.channels.insert(name.clone(), id);
                        state_changed = true;
                    }
                }
            }
            for name in registry.role_names() {
                if !state.roles.contains_key(name) {
                    if tracker.ambiguous_roles.contains(name) {
                        eprintln!(
                            "Warning: stable name '{name}' matches multiple roles; add it to state.tsv to disambiguate"
                        );
                    } else if let Some(&id) = tracker.roles.get(name) {
                        eprintln!("Resolved role '{name}' -> {id} from gateway");
                        state.roles.insert(name.clone(), id);
                        state_changed = true;
                    }
                }
            }
            registry.normalize(&state.channels, &state.roles, &state.managed);
            if state_changed {
                save_state(state);
            }
        }

        match event {
            Some(event) => {
                let bot_user_id = UserRef::from_id(discord.bot_user_id()?);
                let mut host = Host::new(&state.managed, registry, log, bot_user_id);
                match bot.handle_event(&event, &mut host) {
                    Ok(()) => {
                        execute_actions(
                            discord,
                            registry,
                            state,
                            host.into_actions(),
                            config.platform.guild_id,
                        );
                    }
                    Err(e) => {
                        let n = host.actions().len();
                        if n > 0 {
                            eprintln!("Event handler error: {e}; dropping {n} queued action(s)");
                        } else {
                            eprintln!("Event handler error: {e}");
                        }
                    }
                }
            }
            None => log.flush(),
        }
    }
}

fn execute_actions(
    discord: &mut kamekichi_discord::Client,
    registry: &mut RefRegistry,
    state: &mut State,
    actions: Vec<Action>,
    guild_id: u64,
) {
    for action in actions {
        match action {
            Action::SendMessage { channel, content } => {
                let Some(channel_id) = registry.resolve_channel(channel) else {
                    eprintln!("send_message failed: unresolved channel");
                    continue;
                };
                match discord.send_message(channel_id, &content) {
                    Ok(_) => {}
                    Err(e) => eprintln!("Failed to send message: {e}"),
                }
            }
            Action::EditMessage {
                channel,
                message,
                content,
            } => {
                let Some(channel_id) = registry.resolve_channel(channel) else {
                    eprintln!("edit_message failed: unresolved channel");
                    continue;
                };
                let Some(message_id) = registry.resolve_message(message) else {
                    eprintln!("edit_message failed: unresolved message");
                    continue;
                };
                match discord.edit_message(channel_id, message_id, &content) {
                    Ok(()) => {}
                    Err(e) => eprintln!("Failed to edit message: {e}"),
                }
            }
            Action::AddRole { user, role } => {
                let Some(user_id) = registry.resolve_user(user) else {
                    eprintln!("add_role failed: unresolved user");
                    continue;
                };
                let Some(role_id) = registry.resolve_role(role) else {
                    eprintln!("add_role failed: unresolved role");
                    continue;
                };
                match discord.add_role(guild_id, user_id, role_id) {
                    Ok(()) => eprintln!("Assigned role {role_id} to {user_id}"),
                    Err(e) => eprintln!("Failed to assign role: {e}"),
                }
            }
            Action::RemoveRole { user, role } => {
                let Some(user_id) = registry.resolve_user(user) else {
                    eprintln!("remove_role failed: unresolved user");
                    continue;
                };
                let Some(role_id) = registry.resolve_role(role) else {
                    eprintln!("remove_role failed: unresolved role");
                    continue;
                };
                match discord.remove_role(guild_id, user_id, role_id) {
                    Ok(()) => eprintln!("Removed role {role_id} from {user_id}"),
                    Err(e) => eprintln!("Failed to remove role: {e}"),
                }
            }
            Action::AddReaction {
                channel,
                message,
                emoji,
            } => {
                let Some(channel_id) = registry.resolve_channel(channel) else {
                    eprintln!("add_reaction failed: unresolved channel");
                    continue;
                };
                let Some(message_id) = registry.resolve_message(message) else {
                    eprintln!("add_reaction failed: unresolved message");
                    continue;
                };
                match discord.add_reaction(channel_id, message_id, &emoji) {
                    Ok(()) => {}
                    Err(e) => eprintln!("Failed to add reaction: {e}"),
                }
            }
            Action::UpsertMessage {
                message,
                channel,
                content,
            } => {
                let content_hash = crate::state::content_hash(&content);
                let Some(name) = registry.message_name(message) else {
                    eprintln!("upsert_message failed: unresolved message ref");
                    continue;
                };
                let Some(channel_id) = registry.resolve_channel(channel) else {
                    eprintln!("upsert_message failed: unresolved channel for '{name}'");
                    continue;
                };
                if let Some(existing) = state.managed.get(name) {
                    if existing.channel_id != channel_id {
                        eprintln!(
                            "Managed message '{name}' already exists in a different channel, \
                             delete state.tsv entry to move it"
                        );
                        continue;
                    }
                    let msg_id = existing.message_id;
                    match discord.edit_message(channel_id, msg_id, &content) {
                        Ok(()) => {
                            eprintln!("Managed message '{name}' updated");
                            state.managed.insert(
                                name.to_string(),
                                crate::state::ManagedMsg {
                                    message_id: msg_id,
                                    channel_id,
                                    content_hash,
                                },
                            );
                            registry.normalize(&state.channels, &state.roles, &state.managed);
                            save_state(state);
                        }
                        Err(e) => eprintln!("Failed to update managed message '{name}': {e}"),
                    }
                } else {
                    match discord.send_message(channel_id, &content) {
                        Ok(Some(msg_id)) => {
                            eprintln!("Managed message '{name}' posted as {msg_id}");
                            state.managed.insert(
                                name.to_string(),
                                crate::state::ManagedMsg {
                                    message_id: msg_id,
                                    channel_id,
                                    content_hash,
                                },
                            );
                            registry.normalize(&state.channels, &state.roles, &state.managed);
                            save_state(state);
                        }
                        Ok(None) => {
                            eprintln!("Failed to get message ID for managed message '{name}'")
                        }
                        Err(e) => eprintln!("Failed to post managed message '{name}': {e}"),
                    }
                }
            }
        }
    }
}
