use kamekichi::{Bot, ChannelRef, Event, Host, MessageRef, Result, RoleRef};
use serde::Deserialize;

#[derive(Deserialize)]
struct Kippe {
    reaction_roles: Vec<ReactionRoleMapping>,
    managed_messages: Vec<ManagedMessage>,
}

#[derive(Deserialize)]
struct ReactionRoleMapping {
    message: MessageRef,
    emoji: String,
    role: RoleRef,
    seed_reaction: bool,
}

#[derive(Deserialize)]
struct ManagedMessage {
    message: MessageRef,
    channel: ChannelRef,
    content: String,
}

impl Bot for Kippe {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        host.log(event);
        match event {
            Event::Started => {
                for m in &self.managed_messages {
                    host.upsert_message(m.message, m.channel, &m.content);
                }
                for r in &self.reaction_roles {
                    if r.seed_reaction
                        && let Some(mm) = self
                            .managed_messages
                            .iter()
                            .find(|m| host.eq(m.message, r.message))
                    {
                        host.add_reaction(mm.channel, r.message, &r.emoji);
                    }
                }
            }
            Event::Reaction {
                message,
                user,
                emoji,
                added,
                ..
            } => {
                if host.eq(host.bot_user(), *user) {
                    return Ok(());
                }
                for m in &self.reaction_roles {
                    if host.eq(m.message, *message) && m.emoji == *emoji && *added {
                        host.add_role(*user, m.role);
                    } else {
                        host.remove_role(*user, m.role);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn main() {
    if let Err(e) = kamekichi::run::<Kippe>() {
        eprintln!("Fatal: {e}");
        std::process::exit(1);
    }
}
