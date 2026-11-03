//! The trigger: who a message has to be from, and what it has to say, before it becomes work.
//!
//! A surface that answers everything from an allowed sender pays a model call for every sentence a group
//! says. This is the filter that decides, in code and before any model is reached, whether a message was
//! addressed to this agent at all. The allowlist is still the first gate: a stranger is refused before
//! this is asked, and a message that fails here is dropped, recorded and costs nothing.
//!
//! `Always` is the default and reproduces the behaviour that existed before the key: every message from an
//! allowed sender is a request. A surface only gains a filter by naming one, so no existing config changes
//! meaning when it is upgraded.

/// What a surface does with a message that arrives without an explicit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trigger {
    /// Answer anything from an allowed sender. Today's behaviour, and the default for a surface.
    #[default]
    Always,
    /// Answer a direct message, or a group message that names this bot or replies to it.
    MentionOnly,
    /// Answer nothing. A surface parked without removing its token.
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A person asked for something: submit it.
    Requested,
    /// Addressed at us but without a request: worth a model call to read it.
    Candidate,
    /// Not for us: drop, record, spend nothing.
    Ignore,
}

impl Trigger {
    /// The word a config file uses. One spelling per variant, so a refusal can name the words that work
    /// and a printed setting can be pasted back in.
    pub fn word(self) -> &'static str {
        match self {
            Trigger::Always => "always",
            Trigger::MentionOnly => "mention",
            Trigger::Never => "never",
        }
    }

    /// `None` for anything else, so a caller refuses by name instead of guessing at a typo.
    pub fn parse(text: &str) -> Option<Trigger> {
        match text.trim().to_ascii_lowercase().as_str() {
            "always" => Some(Trigger::Always),
            "mention" => Some(Trigger::MentionOnly),
            "never" => Some(Trigger::Never),
            _ => None,
        }
    }
}

use super::{Chat, Settings, Update};

/// The trigger a message is answered under: the channel's word, then the surface's, then the default.
/// One function, so what is enforced and what is printed come from the same resolution.
pub fn resolved(settings: &Settings, chat: &Chat) -> Trigger {
    settings
        .channels
        .get(chat.id())
        .and_then(|channel| channel.trigger)
        .unwrap_or(settings.trigger)
}

/// Whether the text opens with a command. Every surface can see this, so it is judged here rather than
/// delegated: a connector that strips mentions still passes `/status` through untouched.
fn commanded(text: &str) -> bool {
    text.trim().starts_with('/')
}

/// The names a message can use for this surface: the `username` the config gives it, and the surface's own
/// name. Compared without case because a Telegram username has none.
fn names_us(name: &str, settings: &Settings, chat: &Chat) -> bool {
    settings
        .username
        .as_deref()
        .is_some_and(|configured| configured.eq_ignore_ascii_case(name))
        || chat.surface_name().eq_ignore_ascii_case(name)
}

/// Whether the text names this surface: an `@` word that is one of its own names.
///
/// Any other `@` word is somebody else being named. "@alice the build broke again" in a group is a sentence
/// about a person, and answering it spends the model call this filter exists to save.
///
/// A *reply* to one of our own messages cannot be seen from here: `Update` carries the text, the sender
/// and the chat, and the surface does not hand over what was replied to. A surface that can see a reply
/// says so through `Update::mention`, which is why that field outranks this.
fn text_mentions(text: &str, settings: &Settings, chat: &Chat) -> bool {
    text.split_whitespace().any(|word| {
        word.strip_prefix('@').is_some_and(|name| {
            // A name in a sentence carries the punctuation after it: "@hades, look" names hades.
            let name = name.trim_end_matches(|ch: char| !(ch.is_alphanumeric() || ch == '_'));
            !name.is_empty() && names_us(name, settings, chat)
        })
    })
}

/// Whether this message names the surface, from the surface's own detection when it has one. A connector
/// that recognises its mention tokens strips them before the update is built, and then no word in the text
/// begins with `@`: reading the text alone would ignore every real mention on that surface.
fn named(update: &Update, settings: &Settings) -> bool {
    match update.mention {
        Some(seen) => seen,
        None => text_mentions(&update.text, settings, &update.chat),
    }
}

/// One sentence for the ledger, naming the setting that dropped the message. `None` when this trigger
/// cannot drop anything, so a caller never writes a reason that is not true.
pub fn why_ignored(trigger: Trigger) -> Option<&'static str> {
    match trigger {
        Trigger::Never => Some("the channel is set to never answer"),
        Trigger::MentionOnly => {
            Some("the channel is set to mention and nothing in the message names this surface")
        }
        Trigger::Always => None,
    }
}

/// What this message is, without asking a model. Pure: the same update and settings always classify the
/// same way, which is what makes the filter testable and its refusals explainable.
pub fn classify(update: &Update, settings: &Settings) -> Verdict {
    let trigger = resolved(settings, &update.chat);
    // A parked surface answers nothing, including the one-to-one chat and a command.
    if trigger == Trigger::Never {
        return Verdict::Ignore;
    }
    // A direct chat is addressed by existing: the sender wrote here and nowhere else. The surface says
    // whether this is one when it can tell the shapes apart — Discord hands over the DM channel's own
    // snowflake as the chat id, so comparing the sender with the chat would call every direct message a
    // stranger's. A surface that says nothing keeps the older reading, which is how Telegram spells a
    // private chat: the chat id is the person's own id.
    let direct = update
        .direct
        .unwrap_or_else(|| update.from == update.chat.id());
    if direct {
        return Verdict::Requested;
    }
    match trigger {
        // The behaviour before the key existed: every message from an allowed sender is a request.
        Trigger::Always => Verdict::Requested,
        Trigger::MentionOnly if commanded(&update.text) || named(update, settings) => {
            Verdict::Requested
        }
        // A channel somebody listed on purpose is where an unaddressed sentence is still worth reading: the
        // configuration already said this channel matters, and the model judges the rest. That stops applying
        // the moment the entry says what it wants for itself: a channel entry carrying its own `trigger` has
        // been answered already, so listing it for `mode: work` must not quietly turn it into a listening
        // channel that answers everything anybody says. Without this, the only way to get strict mentions was
        // to stop listing the channel at all, which also took `/run` away from it.
        Trigger::MentionOnly
            if settings.channels.contains_key(update.chat.id())
                && settings
                    .channels
                    .get(update.chat.id())
                    .is_some_and(|channel| channel.trigger.is_none()) =>
        {
            Verdict::Candidate
        }
        _ => Verdict::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Chat, Settings, Update};

    /// The narrowest settings that can answer the question: one surface, no channels, no model.
    fn settings(trigger: Trigger) -> Settings {
        Settings {
            allowed: vec!["7".to_string()],
            reasoning: None,
            operators: Vec::new(),
            trigger,
            policy: "guarded".to_string(),
            verify: "true".to_string(),
            model: "none".to_string(),
            root: "/work".to_string(),
            vault: "/work/mneme".to_string(),
            heartbeat: 900,
            mcp: crate::mcp::McpConfig::default(),
            mode: crate::mode::Mode::Act,
            username: None,
            channels: std::collections::HashMap::new(),
            prompt: None,
            loading: None,
            provider: None,
            endpoints: Default::default(),
            providers: Default::default(),
            fallbacks: Default::default(),
        }
    }

    /// A group chat has a negative id and a sender whose id is not the chat's; a direct chat has the same
    /// id in both places, which is how every surface spells a one-to-one conversation.
    fn update(from: i64, chat: i64, text: &str) -> Update {
        Update {
            id: 1,
            from: from.to_string(),
            chat: Chat::telegram(chat),
            text: text.to_string(),
            callback: None,
            photo: None,
            mention: None,
            direct: None,
        }
    }

    /// The defect this exists for: a Discord DM carries the DM channel's own snowflake as the chat id and
    /// the person's id as the sender, so `from == chat.id()` is false there and, under `mention`, a plain
    /// direct message was dropped after the connector delivered it. The surface says the chat is
    /// one-to-one, and the filter stops guessing from ids.
    #[test]
    fn a_direct_chat_the_surface_names_is_a_request_though_the_ids_differ() {
        let dir = crate::scratch::unique("trigger-direct");
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "model": "none",
                "root": "/work",
                "vault": "/work/mneme",
                "gateway": {
                    "connectors": {
                        "discord": {"allowed_users": ["7"], "trigger": "mention"}
                    }
                }
            })
            .to_string(),
        )
        .expect("a config");
        let settings = crate::gateway::settings_for(&path, "discord").expect("the surface");
        let dm = Update {
            from: "7".to_string(),
            chat: Chat::new("discord", "1300000000000000002"),
            text: "the build broke again".to_string(),
            direct: Some(true),
            ..update(0, 0, "")
        };
        assert_eq!(
            classify(&dm, &settings),
            Verdict::Requested,
            "a direct message the surface named was filtered as unaddressed"
        );

        // The surface's answer outranks the id guess in both directions: two ids that happen to match do
        // not make a channel somebody else is reading a direct chat.
        let channel = Update {
            from: "7".to_string(),
            chat: Chat::new("discord", "7"),
            text: "the build broke again".to_string(),
            direct: Some(false),
            ..update(0, 0, "")
        };
        assert_eq!(classify(&channel, &settings), Verdict::Ignore);
    }

    #[test]
    fn a_group_message_that_names_nobody_is_ignored() {
        let settings = settings(Trigger::MentionOnly);
        let update = update(42, -100123, "the build broke again");
        assert_eq!(classify(&update, &settings), Verdict::Ignore);
    }

    #[test]
    fn the_same_message_in_a_direct_chat_is_a_request() {
        let settings = settings(Trigger::MentionOnly);
        let update = update(42, 42, "the build broke again");
        assert_eq!(classify(&update, &settings), Verdict::Requested);
    }

    /// `Never` parks a surface without removing its token, so it holds even for a direct message and even
    /// for a command. A parked surface that answers the one shape nobody can mistake is not parked.
    #[test]
    fn a_surface_set_to_never_ignores_a_direct_message_and_a_command() {
        let settings = settings(Trigger::Never);
        assert_eq!(
            classify(&update(42, 42, "are you there"), &settings),
            Verdict::Ignore
        );
        assert_eq!(
            classify(&update(42, 42, "/run build it"), &settings),
            Verdict::Ignore
        );
    }

    /// Naming the surface is a request, whether the message says its name or starts with a command.
    #[test]
    fn a_group_message_that_names_the_surface_is_a_request() {
        let mut settings = settings(Trigger::MentionOnly);
        settings.username = Some("hades".to_string());
        assert_eq!(
            classify(&update(42, -100123, "@hades the build broke"), &settings),
            Verdict::Requested
        );
        assert_eq!(
            classify(&update(42, -100123, "/status"), &settings),
            Verdict::Requested
        );
    }

    /// The defect this exists for: any `@word` counted as naming the surface, so "@alice the build broke
    /// again" in a group was a request and spent the model call this filter exists to save. Only this
    /// surface's own names count, and a surface whose username the config does not name still answers to
    /// the name it is.
    #[test]
    fn a_message_that_names_somebody_else_is_not_addressed_to_us() {
        let mut named = settings(Trigger::MentionOnly);
        named.username = Some("hades_bot".to_string());
        let bare = settings(Trigger::MentionOnly);
        assert_eq!(
            classify(&update(42, -100123, "@alice the build broke again"), &bare),
            Verdict::Ignore
        );
        assert_eq!(
            classify(
                &update(42, -100123, "@telegram the build broke again"),
                &bare
            ),
            Verdict::Requested,
            "a surface answers to its own name"
        );
        assert_eq!(
            classify(&update(42, -100123, "@hades_bot, the build broke"), &named),
            Verdict::Requested,
            "the comma after a name is punctuation, not part of it"
        );
        assert_eq!(
            classify(&update(42, -100123, "@Hades_Bot look"), &named),
            Verdict::Requested,
            "a Telegram username has no case"
        );
        assert_eq!(
            classify(&update(42, -100123, "@hades_bot2 look"), &named),
            Verdict::Ignore,
            "a longer name is a different name"
        );
    }

    /// The defect this exists for: a connector recognises its own mention tokens and strips them before the
    /// update is built, so on that surface the text said "the build broke" and every real mention read as an
    /// unaddressed sentence. The surface's own answer travels on the update, because it is the only thing
    /// that saw the token.
    #[test]
    fn a_surface_that_saw_its_own_name_is_addressed_though_the_text_was_stripped() {
        let settings = settings(Trigger::MentionOnly);
        let stripped = Update {
            text: "the build broke again".to_string(),
            mention: Some(true),
            ..update(42, -100123, "")
        };
        assert_eq!(classify(&stripped, &settings), Verdict::Requested);

        // A surface that looked and found nothing is not rescued by the text, and a command is still a
        // command on a surface that does its own detection.
        let looked = Update {
            text: "@alice the build broke again".to_string(),
            mention: Some(false),
            ..update(42, -100123, "")
        };
        assert_eq!(classify(&looked, &settings), Verdict::Ignore);
        let command = Update {
            text: "/status".to_string(),
            mention: Some(false),
            ..update(42, -100123, "")
        };
        assert_eq!(classify(&command, &settings), Verdict::Requested);
    }

    /// A channel somebody listed on purpose is the one place an unaddressed sentence is worth reading:
    /// the configuration already said this channel matters, and the model is asked to judge the rest.
    #[test]
    fn a_listed_channel_reads_an_unaddressed_message_but_an_unlisted_one_is_ignored() {
        let mut settings = settings(Trigger::MentionOnly);
        settings
            .channels
            .insert("99".to_string(), crate::gateway::Channel::default());
        assert_eq!(
            classify(&update(42, 99, "the build broke again"), &settings),
            Verdict::Candidate
        );
        assert_eq!(
            classify(&update(42, 98, "the build broke again"), &settings),
            Verdict::Ignore
        );
    }

    /// Listing a channel for work must not answer everything anyone says there, as long as the entry also
    /// says how it is addressed. The alternative was to stop listing the channel, which took `/run` away too.
    #[test]
    fn a_channel_that_names_its_own_trigger_is_not_a_listening_channel() {
        let mut settings = settings(Trigger::MentionOnly);
        settings.channels.insert(
            "99".to_string(),
            crate::gateway::Channel {
                trigger: Some(Trigger::MentionOnly),
                ..Default::default()
            },
        );
        assert_eq!(
            classify(&update(42, 99, "the build broke again"), &settings),
            Verdict::Ignore,
            "an explicit mention trigger on the channel must beat the listed-channel rule"
        );
        let mut named = update(42, 99, "the build broke again");
        named.mention = Some(true); // what the connector reports once it has stripped its own mention token
        assert_eq!(
            classify(&named, &settings),
            Verdict::Requested,
            "a real mention still reaches it"
        );
    }

    /// The default has to be exactly the behaviour that existed before the key, or an upgrade silently
    /// stops answering people.
    #[test]
    fn always_answers_everything_from_an_allowed_sender_as_it_does_today() {
        let settings = settings(Trigger::Always);
        assert_eq!(
            classify(&update(42, -100123, "the build broke again"), &settings),
            Verdict::Requested
        );
        assert_eq!(
            classify(&update(42, 42, "the build broke again"), &settings),
            Verdict::Requested
        );
    }

    /// A channel's word overrides the surface's in both directions: it can filter a surface that answers
    /// everything, and it can open a channel on a surface that answers nothing.
    #[test]
    fn a_channels_trigger_beats_the_surfaces() {
        let mut wide = settings(Trigger::Always);
        wide.channels.insert(
            "99".to_string(),
            crate::gateway::Channel {
                trigger: Some(Trigger::MentionOnly),
                ..Default::default()
            },
        );
        assert_eq!(
            classify(&update(42, 99, "the build broke again"), &wide),
            Verdict::Ignore,
            "the channel said mention, so a sentence that names nobody is not a request"
        );
        assert_eq!(
            classify(&update(42, 98, "the build broke again"), &wide),
            Verdict::Requested
        );

        let mut narrow = settings(Trigger::Never);
        narrow.channels.insert(
            "99".to_string(),
            crate::gateway::Channel {
                trigger: Some(Trigger::Always),
                ..Default::default()
            },
        );
        assert_eq!(
            classify(&update(42, 99, "the build broke again"), &narrow),
            Verdict::Requested
        );
    }

    /// The words round-trip through the config, and a word nobody defined does not.
    #[test]
    fn a_trigger_has_one_word_and_an_unknown_one_is_refused() {
        for trigger in [Trigger::Always, Trigger::MentionOnly, Trigger::Never] {
            assert_eq!(Trigger::parse(trigger.word()), Some(trigger));
        }
        assert_eq!(Trigger::parse("mentionable"), None);
        assert_eq!(Trigger::parse(""), None);
    }
}
