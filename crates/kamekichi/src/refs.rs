use std::cell::RefCell;
use std::collections::HashMap;

use serde::Deserialize;

use crate::state::ManagedMsg;

/// Flag bit used to distinguish direct snowflake IDs from stable-name indices.
/// Discord snowflakes encode a millisecond timestamp from epoch 2015-01-01 in
/// the upper 42 bits, so bit 63 won't be set until the year ~2084.  This is
/// safe to use as a tag bit for the foreseeable future.
const DIRECT_BIT: u64 = 1 << 63;

fn parse_snowflake(s: &str) -> Option<u64> {
    s.parse::<u64>().ok()
}

struct RefTable {
    names: Vec<String>,
    ids: Vec<u64>,
}

impl RefTable {
    const fn new() -> Self {
        RefTable {
            names: Vec::new(),
            ids: Vec::new(),
        }
    }

    fn push(&mut self, name: String) -> u64 {
        if let Some(idx) = self.names.iter().position(|n| *n == name) {
            return idx as u64;
        }
        let idx = self.names.len() as u64;
        self.names.push(name);
        idx
    }

    fn normalize_with(&mut self, map: &HashMap<String, u64>) {
        self.ids.clear();
        self.ids.resize(self.names.len(), 0);
        for (i, name) in self.names.iter().enumerate() {
            if let Some(&val) = map.get(name) {
                self.ids[i] = val;
            }
        }
    }

    fn name(&self, index: u64) -> Option<&str> {
        self.names.get(index as usize).map(|s| s.as_str())
    }

    fn resolve(&self, index: u64) -> Option<u64> {
        let id = *self.ids.get(index as usize)?;
        if id != 0 { Some(id) } else { None }
    }
}

pub struct RefRegistry {
    channels: RefTable,
    messages: RefTable,
    roles: RefTable,
}

thread_local! {
    static ACTIVE: RefCell<Option<RefRegistry>> = const { RefCell::new(None) };
}

/// RAII guard that installs a fresh [`RefRegistry`] for its lifetime.
///
/// Dropping the guard without calling [`into_registry`](Self::into_registry)
/// cleans up the thread-local automatically (e.g. on error or panic).
pub(crate) struct RefRegistryScope;

impl RefRegistryScope {
    pub(crate) fn new() -> Self {
        RefRegistry::install();
        RefRegistryScope
    }

    /// Consume the scope and return the populated registry.
    pub(crate) fn into_registry(self) -> RefRegistry {
        // Prevent `Drop::drop` from running a redundant `take()`.
        std::mem::forget(self);
        RefRegistry::take()
    }
}

impl Drop for RefRegistryScope {
    fn drop(&mut self) {
        // Clean up the thread-local if the scope was not explicitly consumed.
        RefRegistry::take();
    }
}

impl RefRegistry {
    pub const fn new() -> Self {
        RefRegistry {
            channels: RefTable::new(),
            messages: RefTable::new(),
            roles: RefTable::new(),
        }
    }

    pub(crate) fn install() {
        ACTIVE.with_borrow_mut(|r| *r = Some(RefRegistry::new()));
    }

    pub(crate) fn take() -> RefRegistry {
        ACTIVE
            .with_borrow_mut(|r| r.take())
            .unwrap_or_else(RefRegistry::new)
    }

    pub(crate) fn normalize(
        &mut self,
        channels: &HashMap<String, u64>,
        roles: &HashMap<String, u64>,
        managed: &HashMap<String, ManagedMsg>,
    ) {
        self.channels.normalize_with(channels);
        self.roles.normalize_with(roles);
        self.messages.ids.clear();
        self.messages.ids.resize(self.messages.names.len(), 0);
        for (i, name) in self.messages.names.iter().enumerate() {
            if let Some(entry) = managed.get(name) {
                self.messages.ids[i] = entry.message_id;
            }
        }
    }

    pub(crate) fn channel_names(&self) -> &[String] {
        &self.channels.names
    }

    pub(crate) fn role_names(&self) -> &[String] {
        &self.roles.names
    }

    pub(crate) fn message_name(&self, r: MessageRef) -> Option<&str> {
        if r.0 & DIRECT_BIT != 0 {
            return None;
        }
        self.messages.name(r.0)
    }
}

fn deserialize_id<E: serde::de::Error>(id_str: &str) -> Result<u64, E> {
    parse_snowflake(id_str).ok_or_else(|| E::custom("invalid snowflake id"))
}

fn deserialize_stablename_index<E: serde::de::Error>(
    name: &str,
    push: impl FnOnce(&mut RefRegistry) -> u64,
) -> Result<u64, E> {
    if name.contains('\t') || name.contains('\n') || name.contains('\r') {
        return Err(E::custom(
            "stable name must not contain tab, newline, or carriage return",
        ));
    }
    ACTIVE.with_borrow_mut(|r| {
        r.as_mut().map(push).ok_or_else(|| {
            E::custom("stablename ref deserialized outside of RefRegistry::install() scope")
        })
    })
}

pub trait Ref: Copy {
    fn resolve(self, registry: &RefRegistry) -> Option<u64>;
}

macro_rules! ref_type {
    ($name:ident, $field:ident, $resolve:ident) => {
        ref_type!(@base $name, $field, $resolve);

        impl $name {
            pub fn from_stablename(name: &str, registry: &mut RefRegistry) -> Self {
                assert!(
                    !name.contains('\t') && !name.contains('\n') && !name.contains('\r'),
                    "stable name must not contain tab, newline, or carriage return: {name:?}"
                );
                $name(registry.$field.push(name.to_string()))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                if let Some(id_str) = s.strip_prefix("id:") {
                    Ok($name::from_id(deserialize_id(id_str)?))
                } else if let Some(name) = s.strip_prefix("stablename:") {
                    let idx = deserialize_stablename_index(name, |reg| reg.$field.push(name.to_string()))?;
                    Ok($name(idx))
                } else {
                    Err(serde::de::Error::custom("expected 'id:...' or 'stablename:...'"))
                }
            }
        }
    };
    (@id_only $name:ident, $resolve:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_id(id: u64) -> Self {
                $name(id | DIRECT_BIT)
            }

            #[allow(dead_code)]
            pub(crate) fn parse(s: &str) -> Option<Self> {
                parse_snowflake(s).map(Self::from_id)
            }
        }

        impl RefRegistry {
            pub(crate) fn $resolve(&self, r: $name) -> Option<u64> {
                Some(r.0 & !DIRECT_BIT)
            }
        }

        impl Ref for $name {
            fn resolve(self, registry: &RefRegistry) -> Option<u64> {
                registry.$resolve(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                if let Some(id_str) = s.strip_prefix("id:") {
                    Ok($name::from_id(deserialize_id(id_str)?))
                } else {
                    Err(serde::de::Error::custom("expected 'id:...'"))
                }
            }
        }
    };
    (@base $name:ident, $field:ident, $resolve:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(u64);

        impl $name {
            pub const fn from_id(id: u64) -> Self {
                $name(id | DIRECT_BIT)
            }

            #[allow(dead_code)]
            pub(crate) fn parse(s: &str) -> Option<Self> {
                parse_snowflake(s).map(Self::from_id)
            }
        }

        impl RefRegistry {
            pub(crate) fn $resolve(&self, r: $name) -> Option<u64> {
                if r.0 & DIRECT_BIT != 0 {
                    return Some(r.0 & !DIRECT_BIT);
                }
                self.$field.resolve(r.0)
            }
        }

        impl Ref for $name {
            fn resolve(self, registry: &RefRegistry) -> Option<u64> {
                registry.$resolve(self)
            }
        }
    };
}

ref_type!(ChannelRef, channels, resolve_channel);
ref_type!(MessageRef, messages, resolve_message);
ref_type!(RoleRef, roles, resolve_role);
ref_type!(@id_only UserRef, resolve_user);
