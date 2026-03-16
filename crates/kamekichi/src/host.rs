use std::collections::HashMap;
use std::sync::LazyLock;

/// Maximum number of actions a single event handler invocation may queue.
/// Prevents runaway handlers from exhausting memory.
const MAX_ACTIONS: usize = 100;

use crate::logger::Logger;
use crate::refs::{ChannelRef, MessageRef, Ref, RefRegistry, RoleRef, UserRef};
use crate::state::{ManagedMsg, content_hash};
use crate::types::{Action, Event, LogEntry};

static EMPTY_MANAGED: LazyLock<HashMap<String, ManagedMsg>> = LazyLock::new(HashMap::new);
static EMPTY_REGISTRY: RefRegistry = RefRegistry::new();

pub struct Host<'a> {
    managed: &'a HashMap<String, ManagedMsg>,
    registry: &'a RefRegistry,
    logger: Option<&'a mut Logger>,
    bot_user_id: UserRef,
    actions: Vec<Action>,
}

impl Host<'_> {
    pub(crate) fn new<'a>(
        managed: &'a HashMap<String, ManagedMsg>,
        registry: &'a RefRegistry,
        logger: &'a mut Logger,
        bot_user_id: UserRef,
    ) -> Host<'a> {
        Host {
            managed,
            registry,
            logger: Some(logger),
            bot_user_id,
            actions: Vec::new(),
        }
    }

    pub fn empty() -> Host<'static> {
        Host {
            managed: &EMPTY_MANAGED,
            registry: &EMPTY_REGISTRY,
            logger: None,
            bot_user_id: UserRef::from_id(0),
            actions: Vec::new(),
        }
    }

    pub fn bot_user(&self) -> UserRef {
        self.bot_user_id
    }

    fn push_action(&mut self, action: Action) {
        if self.actions.len() >= MAX_ACTIONS {
            eprintln!("Action queue full ({MAX_ACTIONS}); dropping action");
            return;
        }
        self.actions.push(action);
    }

    pub fn log(&mut self, event: &Event) {
        let reg = self.registry;
        if let Some(entry) = LogEntry::from_event(
            event,
            |r| reg.resolve_channel(r),
            |r| reg.resolve_message(r),
            |r| reg.resolve_user(r),
        ) && let Some(logger) = &mut self.logger
        {
            logger.write(&entry);
        }
    }

    pub fn send_message(&mut self, channel: ChannelRef, content: &str) {
        self.push_action(Action::SendMessage {
            channel,
            content: content.to_string(),
        });
    }

    pub fn edit_message(&mut self, channel: ChannelRef, message: MessageRef, content: &str) {
        self.push_action(Action::EditMessage {
            channel,
            message,
            content: content.to_string(),
        });
    }

    pub fn add_role(&mut self, user: UserRef, role: RoleRef) {
        self.push_action(Action::AddRole { user, role });
    }

    pub fn add_reaction(&mut self, channel: ChannelRef, message: MessageRef, emoji: &str) {
        self.push_action(Action::AddReaction {
            channel,
            message,
            emoji: emoji.to_string(),
        });
    }

    pub fn remove_role(&mut self, user: UserRef, role: RoleRef) {
        self.push_action(Action::RemoveRole { user, role });
    }

    pub fn upsert_message(&mut self, message: MessageRef, channel: ChannelRef, content: &str) {
        let hash = content_hash(content);
        let channel_id = self.registry.resolve_channel(channel);
        if let Some(name) = self.registry.message_name(message)
            && self
                .managed
                .get(name)
                .is_some_and(|e| e.content_hash == hash && channel_id == Some(e.channel_id))
        {
            return;
        }
        self.push_action(Action::UpsertMessage {
            message,
            channel,
            content: content.to_string(),
        });
    }

    pub fn eq<T: Ref + PartialEq>(&self, a: T, b: T) -> bool {
        match (a.resolve(self.registry), b.resolve(self.registry)) {
            (Some(ra), Some(rb)) => ra == rb,
            (None, None) => a == b,
            _ => false,
        }
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub(crate) fn into_actions(self) -> Vec<Action> {
        self.actions
    }
}
