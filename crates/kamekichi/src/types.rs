use crate::refs::{ChannelRef, MessageRef, RoleRef, UserRef};

#[non_exhaustive]
pub enum Event {
    Started,
    Reaction {
        user: UserRef,
        channel: ChannelRef,
        message: MessageRef,
        emoji: String,
        added: bool,
    },
    MessageCreate {
        message_id: MessageRef,
        channel: ChannelRef,
        author: UserRef,
        author_name: String,
        content: String,
    },
    MessageUpdate {
        message: MessageRef,
        channel: ChannelRef,
        content: Option<String>,
    },
    MessageDelete {
        message: MessageRef,
        channel: ChannelRef,
    },
}

#[non_exhaustive]
pub enum Action {
    SendMessage {
        channel: ChannelRef,
        content: String,
    },
    EditMessage {
        channel: ChannelRef,
        message: MessageRef,
        content: String,
    },
    AddRole {
        user: UserRef,
        role: RoleRef,
    },
    RemoveRole {
        user: UserRef,
        role: RoleRef,
    },
    AddReaction {
        channel: ChannelRef,
        message: MessageRef,
        emoji: String,
    },
    UpsertMessage {
        message: MessageRef,
        channel: ChannelRef,
        content: String,
    },
}

pub(crate) enum LogEntry {
    Reaction {
        channel_id: u64,
        message_id: u64,
        user_id: u64,
        emoji: String,
        added: bool,
    },
    MessageCreate {
        channel_id: u64,
        message_id: u64,
        author_id: u64,
        author_name: String,
        content: String,
    },
    MessageEdit {
        channel_id: u64,
        message_id: u64,
        content: String,
    },
    MessageDelete {
        channel_id: u64,
        message_id: u64,
    },
}

impl LogEntry {
    pub(crate) fn from_event(
        event: &Event,
        resolve_ch: impl Fn(ChannelRef) -> Option<u64>,
        resolve_msg: impl Fn(MessageRef) -> Option<u64>,
        resolve_user: impl Fn(UserRef) -> Option<u64>,
    ) -> Option<Self> {
        match event {
            Event::Started => None,
            Event::Reaction {
                user,
                channel,
                message,
                emoji,
                added,
            } => Some(LogEntry::Reaction {
                channel_id: resolve_ch(*channel)?,
                message_id: resolve_msg(*message)?,
                user_id: resolve_user(*user)?,
                emoji: emoji.clone(),
                added: *added,
            }),
            Event::MessageCreate {
                message_id,
                channel,
                author,
                author_name,
                content,
            } => Some(LogEntry::MessageCreate {
                channel_id: resolve_ch(*channel)?,
                message_id: resolve_msg(*message_id)?,
                author_id: resolve_user(*author)?,
                author_name: author_name.clone(),
                content: content.clone(),
            }),
            Event::MessageUpdate { content: None, .. } => None,
            Event::MessageUpdate {
                message,
                channel,
                content: Some(content),
            } => Some(LogEntry::MessageEdit {
                channel_id: resolve_ch(*channel)?,
                message_id: resolve_msg(*message)?,
                content: content.clone(),
            }),
            Event::MessageDelete { message, channel } => Some(LogEntry::MessageDelete {
                channel_id: resolve_ch(*channel)?,
                message_id: resolve_msg(*message)?,
            }),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}
