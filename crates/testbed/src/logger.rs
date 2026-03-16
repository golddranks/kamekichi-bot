use kamekichi::{Bot, Event, Host, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Logger {}

impl Bot for Logger {
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result {
        host.log(event);
        Ok(())
    }
}
