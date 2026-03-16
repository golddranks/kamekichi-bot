use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::LogEntry;

pub struct Logger(Option<BufWriter<File>>);

impl Logger {
    pub fn open(path: Option<&String>) -> Self {
        Logger(path.and_then(|p| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map(BufWriter::new)
                .map_err(|e| eprintln!("Failed to open log file {p}: {e}"))
                .ok()
        }))
    }

    pub(crate) fn write(&mut self, entry: &LogEntry) {
        if let Some(w) = &mut self.0 {
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if let Err(e) = writeln!(w, "{ts}\t{}", format_entry(entry)) {
                eprintln!("Log write error: {e}; disabling logging");
                self.0 = None;
            }
        }
    }

    pub fn flush(&mut self) {
        if let Some(w) = &mut self.0
            && let Err(e) = w.flush()
        {
            eprintln!("Log flush error: {e}; disabling logging");
            self.0 = None;
        }
    }
}

fn format_entry(entry: &LogEntry) -> String {
    match entry {
        LogEntry::Reaction {
            channel_id,
            message_id,
            user_id,
            emoji,
            added,
        } => {
            let tag = if *added {
                "REACTION_ADD"
            } else {
                "REACTION_REMOVE"
            };
            format!(
                "{tag}\t{channel_id}\t{message_id}\t{user_id}\t{}",
                escape(emoji)
            )
        }
        LogEntry::MessageCreate {
            channel_id,
            message_id,
            author_id,
            author_name,
            content,
        } => format!(
            "MESSAGE\t{channel_id}\t{message_id}\t{author_id}\t{}\t{}",
            escape(author_name),
            escape(content)
        ),
        LogEntry::MessageEdit {
            channel_id,
            message_id,
            content,
        } => format!("EDIT\t{channel_id}\t{message_id}\t{}", escape(content)),
        LogEntry::MessageDelete {
            channel_id,
            message_id,
        } => format!("DELETE\t{channel_id}\t{message_id}"),
    }
}

fn escape(s: &str) -> String {
    crate::escape_tsv(s)
}
