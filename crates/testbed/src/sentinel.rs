use kamekichi::{Bot, ChannelRef, Event, Host, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Sentinel {
    pub watched_cn: ChannelRef,
    pub alert_ch: ChannelRef,
}

impl Bot for Sentinel {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        match event {
            Event::MessageDelete { channel, message } => {
                if host.eq(self.watched_cn, *channel) {
                    host.send_message(
                        self.alert_ch,
                        &format!("Message {message:?} was deleted in {channel:?}"),
                    );
                }
            }
            Event::MessageUpdate {
                channel,
                message,
                content: Some(content),
            } => {
                if host.eq(self.watched_cn, *channel) {
                    host.send_message(
                        self.alert_ch,
                        &format!("Message {message:?} edited in {channel:?}: {content}"),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}
