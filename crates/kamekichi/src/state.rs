use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

use crate::{escape_tsv, unescape_tsv};

pub struct ManagedMsg {
    pub message_id: u64,
    pub channel_id: u64,
    pub content_hash: u64,
}

pub struct State {
    pub managed: HashMap<String, ManagedMsg>,
    pub channels: HashMap<String, u64>,
    pub roles: HashMap<String, u64>,
}

pub fn load(path: &str) -> State {
    let mut managed = HashMap::new();
    let mut channels = HashMap::new();
    let mut roles = HashMap::new();
    if let Ok(data) = fs::read_to_string(path) {
        for line in data.lines() {
            let parts: Vec<&str> = line.splitn(5, '\t').collect();
            match parts.as_slice() {
                ["MESSAGE", stablename, msg_id, ch_id, hash] => {
                    if let (Some(msg_id), Some(ch_id), Some(hash)) = (
                        msg_id
                            .strip_prefix("msg_id:")
                            .and_then(|s| s.parse::<u64>().ok()),
                        ch_id
                            .strip_prefix("ch_id:")
                            .and_then(|s| s.parse::<u64>().ok()),
                        hash.strip_prefix("hash:")
                            .and_then(|h| h.parse::<u64>().ok()),
                    ) {
                        managed.insert(
                            unescape_tsv(stablename),
                            ManagedMsg {
                                message_id: msg_id,
                                channel_id: ch_id,
                                content_hash: hash,
                            },
                        );
                    }
                }
                ["CHANNEL", name, ch_id] => {
                    if let Some(id) = ch_id
                        .strip_prefix("ch_id:")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        channels.insert(unescape_tsv(name), id);
                    }
                }
                ["ROLE", name, role_id] => {
                    if let Some(id) = role_id
                        .strip_prefix("role_id:")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        roles.insert(unescape_tsv(name), id);
                    }
                }
                _ => {}
            }
        }
    }
    State {
        managed,
        channels,
        roles,
    }
}

pub fn save(path: &str, state: &State) {
    let mut content = String::new();
    content.push_str("# version: 1\n");
    let mut managed: Vec<_> = state.managed.iter().collect();
    managed.sort_by_key(|(k, _)| k.as_str());
    for (k, e) in managed {
        content.push_str(&format!(
            "MESSAGE\t{}\tmsg_id:{}\tch_id:{}\thash:{}\n",
            escape_tsv(k),
            e.message_id,
            e.channel_id,
            e.content_hash
        ));
    }
    let mut channels: Vec<_> = state.channels.iter().collect();
    channels.sort_by_key(|(k, _)| k.as_str());
    for (name, id) in channels {
        content.push_str(&format!("CHANNEL\t{}\tch_id:{id}\n", escape_tsv(name)));
    }
    let mut roles: Vec<_> = state.roles.iter().collect();
    roles.sort_by_key(|(k, _)| k.as_str());
    for (name, id) in roles {
        content.push_str(&format!("ROLE\t{}\trole_id:{id}\n", escape_tsv(name)));
    }
    let tmp = format!("{path}.tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = write_result {
        eprintln!("Failed to write state file: {e}");
        let _ = fs::remove_file(&tmp);
    }
}

pub fn content_hash(s: &str) -> u64 {
    let digest = ring::digest::digest(&ring::digest::SHA256, s.as_bytes());
    let bytes: [u8; 8] = digest.as_ref()[..8]
        .try_into()
        .expect("SHA256 digest is 32 bytes");
    u64::from_le_bytes(bytes)
}
