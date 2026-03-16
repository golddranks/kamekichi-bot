pub(crate) mod config;
pub(crate) mod host;
pub(crate) mod logger;
pub(crate) mod refs;
mod runner;
pub(crate) mod state;
pub(crate) mod types;
pub use host::Host;
pub use refs::{ChannelRef, MessageRef, Ref, RefRegistry, RoleRef, UserRef};
pub use runner::{run, run_bot};
pub use types::{Action, Event};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Escape a string for TSV: backslash, tab, newline, carriage-return.
pub(crate) fn escape_tsv(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Inverse of `escape_tsv`.
///
/// Unknown escape sequences (e.g. `\x`) are passed through unchanged as
/// backslash + character, and a lone trailing backslash is preserved as `\`.
/// `escape_tsv` never produces those sequences, so round-trips are lossless.
pub(crate) fn unescape_tsv(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub trait Bot: Sized + serde::de::DeserializeOwned {
    fn new(config: Self) -> Result<Self> {
        Ok(config)
    }
    fn handle_event(&self, event: &Event, host: &mut Host) -> Result;
}
