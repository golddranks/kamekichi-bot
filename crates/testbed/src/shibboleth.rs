use kamekichi::{Bot, Event, Host, Result, RoleRef};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Shibboleth {
    pub passphrase: String,
    pub role: RoleRef,
}

impl Bot for Shibboleth {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        if let Event::MessageCreate {
            content, author, ..
        } = event
            && !host.eq(host.bot_user(), *author)
            && content.trim() == self.passphrase
        {
            host.add_role(*author, self.role);
        }
        Ok(())
    }
}
