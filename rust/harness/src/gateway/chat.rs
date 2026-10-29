//! Who a message is from, on which surface, in one value.
//!
//! Every chat-scoped thing in the system used to be a bare `i64`: the session name, the history file, the
//! stop flag, the goal, the job that reports into it. That is one namespace, and the moment a second
//! surface exists it is the wrong one: Discord's chat id is a snowflake string, two surfaces can hand out
//! the same number, and the cursor file is shared. `Chat` is the namespace, and it renders as `tg-123` so
//! every name that existed before the change stays the same.
//!
//! A surface id is a slug: `tg`, `discord`, `matrix`. It never contains a dash, because the key splits on
//! the first one. The chat id is whatever the platform calls it, kept as a string and never parsed except
//! where the platform's own API needs a number.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chat {
    surface: String,
    id: String,
}

impl Chat {
    pub fn new(surface: impl Into<String>, id: impl Into<String>) -> Chat {
        let surface = surface.into();
        debug_assert!(
            !surface.contains('-') && !surface.is_empty(),
            "a surface id is a slug without a dash: {surface:?}"
        );
        Chat {
            surface,
            id: id.into(),
        }
    }

    /// A Telegram chat, which is the surface the old `i64` keys meant.
    pub fn telegram(id: i64) -> Chat {
        Chat::new("tg", id.to_string())
    }

    pub fn surface(&self) -> &str {
        &self.surface
    }

    /// The name a person reads in the ledger: the built-in surface keeps its old name so the record stays
    /// continuous, and a connector reports its own id.
    pub fn surface_name(&self) -> &str {
        if self.surface == "tg" {
            "telegram"
        } else {
            &self.surface
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// The id as a number, when this is a Telegram chat. A Bot API takes integers, and a Discord
    /// snowflake also parses as one, so the surface has to agree before the number is handed over.
    pub fn telegram_id(&self) -> Option<i64> {
        if self.surface != "tg" {
            return None;
        }
        self.id.parse().ok()
    }

    /// Read a key as written in a file: `tg-123`, `discord-1180`, or a bare `123` from before surfaces.
    pub fn parse(text: &str) -> Option<Chat> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Ok(number) = text.parse::<i64>() {
            return Some(Chat::telegram(number));
        }
        let (surface, id) = text.split_once('-')?;
        if surface.is_empty() || id.is_empty() {
            return None;
        }
        Some(Chat::new(surface, id))
    }

    /// `tg-123`, the name every session, history file and child record has always used.
    pub fn key(&self) -> String {
        format!("{}-{}", self.surface, self.id)
    }
}

impl fmt::Display for Chat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.surface, self.id)
    }
}

impl Serialize for Chat {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.key())
    }
}

impl<'de> Deserialize<'de> for Chat {
    /// Reads `"tg-123"` and also a bare `123`, because every goal, job and child record written before the
    /// surface existed holds the number. The old reading is the Telegram reading; nothing else existed.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Chat, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Key(String),
            Number(i64),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Key(key) => {
                let (surface, id) = key.split_once('-').unwrap_or(("tg", key.as_str()));
                Ok(Chat::new(surface, id))
            }
            Wire::Number(number) => Ok(Chat::telegram(number)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_the_name_every_existing_file_already_uses() {
        assert_eq!(Chat::telegram(42).key(), "tg-42");
        assert_eq!(Chat::telegram(42).to_string(), "tg-42");
        assert_eq!(Chat::new("discord", "1180").key(), "discord-1180");
    }

    #[test]
    fn a_plain_number_reads_as_telegram_and_a_key_round_trips() {
        let old: Chat = serde_json::from_str("42").expect("an old numeric chat");
        assert_eq!(old, Chat::telegram(42));
        let new: Chat = serde_json::from_str("\"discord-1180\"").expect("a key");
        assert_eq!(new, Chat::new("discord", "1180"));
        assert_eq!(
            serde_json::to_string(&new).expect("written"),
            "\"discord-1180\""
        );
    }

    #[test]
    fn a_number_is_only_a_number_when_the_platform_wants_one() {
        assert_eq!(Chat::telegram(-100123).telegram_id(), Some(-100123));
        assert_eq!(Chat::new("discord", "1180").telegram_id(), None);
    }
}
