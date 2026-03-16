use kamekichi::{Bot, Event, Host, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Greeter;

impl Bot for Greeter {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        if let Event::MessageCreate {
            channel,
            author,
            author_name,
            content,
            ..
        } = event
        {
            if host.eq(host.bot_user(), *author) {
                return Ok(());
            }
            let words: Vec<&str> = content
                .split_whitespace()
                .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
                .collect();
            let has_greeting = words.iter().any(|w| {
                let w = w.to_lowercase();
                w == "hello" || w == "hi" || w == "hey"
            });
            if has_greeting {
                host.send_message(*channel, &format!("Hey there, {author_name}!"));
            }
        }
        Ok(())
    }
}
