use kamekichi::{Bot, Event, Host, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Echo;

impl Bot for Echo {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        if let Event::MessageCreate {
            channel,
            author,
            content,
            ..
        } = event
            && !content.is_empty()
            && !host.eq(host.bot_user(), *author)
        {
            host.send_message(*channel, content);
        }
        Ok(())
    }
}
