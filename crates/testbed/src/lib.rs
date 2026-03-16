pub mod echo;
pub mod greeter;
pub mod logger;
pub mod sentinel;
pub mod shibboleth;

#[cfg(test)]
mod tests {
    use kamekichi::{Action, Bot, ChannelRef, Event, Host, MessageRef, RoleRef, UserRef};

    fn msg(content: &str) -> Event {
        Event::MessageCreate {
            message_id: MessageRef::from_id(1),
            channel: ChannelRef::from_id(100),
            author: UserRef::from_id(10),
            author_name: "alice".into(),
            content: content.into(),
        }
    }

    #[test]
    fn echo_repeats_message() {
        let bot = super::echo::Echo {};
        let mut host = Host::empty();
        bot.handle_event(&msg("hello"), &mut host).unwrap();
        assert_eq!(host.actions().len(), 1);
        assert!(matches!(
            &host.actions()[0],
            Action::SendMessage { channel, .. } if host.eq(*channel, ChannelRef::from_id(100))
        ));
    }

    #[test]
    fn sentinel_alerts_on_delete() {
        let bot = super::sentinel::Sentinel {
            watched_cn: ChannelRef::from_id(100),
            alert_ch: ChannelRef::from_id(999),
        };
        let event = Event::MessageDelete {
            message: MessageRef::from_id(1),
            channel: ChannelRef::from_id(100),
        };
        let mut host = Host::empty();
        bot.handle_event(&event, &mut host).unwrap();
        assert_eq!(host.actions().len(), 1);
        assert!(matches!(
            &host.actions()[0],
            Action::SendMessage { channel, .. } if host.eq(*channel, ChannelRef::from_id(999))
        ));
    }

    #[test]
    fn sentinel_ignores_unwatched_channel() {
        let bot = super::sentinel::Sentinel {
            watched_cn: ChannelRef::from_id(100),
            alert_ch: ChannelRef::from_id(999),
        };
        let event = Event::MessageDelete {
            message: MessageRef::from_id(1),
            channel: ChannelRef::from_id(200),
        };
        let mut host = Host::empty();
        bot.handle_event(&event, &mut host).unwrap();
        assert!(host.actions().is_empty());
    }

    #[test]
    fn greeter_says_hello() {
        let bot = super::greeter::Greeter {};
        let mut host = Host::empty();
        bot.handle_event(&msg("hello there"), &mut host).unwrap();
        assert_eq!(host.actions().len(), 1);
        assert!(matches!(
            &host.actions()[0],
            Action::SendMessage { content, .. } if content.contains("alice")
        ));
    }

    #[test]
    fn shibboleth_grants_role() {
        let bot = super::shibboleth::Shibboleth {
            passphrase: "open sesame".into(),
            role: RoleRef::from_id(300),
        };
        let mut host = Host::empty();
        bot.handle_event(&msg("open sesame"), &mut host).unwrap();
        assert_eq!(host.actions().len(), 1);
        assert!(matches!(
            &host.actions()[0],
            Action::AddRole { role, .. } if host.eq(*role, RoleRef::from_id(300))
        ));
    }

    #[test]
    fn shibboleth_ignores_wrong_phrase() {
        let bot = super::shibboleth::Shibboleth {
            passphrase: "open sesame".into(),
            role: RoleRef::from_id(300),
        };
        let mut host = Host::empty();
        bot.handle_event(&msg("close sesame"), &mut host).unwrap();
        assert!(host.actions().is_empty());
    }
}
