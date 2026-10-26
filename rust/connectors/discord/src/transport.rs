//! The Discord surface: a gateway websocket in, a REST call out.
//!
//! Discord has no long poll. `updates` therefore waits on a channel that the websocket thread fills,
//! which keeps the harness's poller shape unchanged. Every REST call carries the bot token in a header
//! and never in a URL, so it does not appear in a process listing.
//!
//! Buttons are message components. A press arrives as an interaction with the component's `custom_id`,
//! which is exactly the data the harness expects back, so the approval flow works without the harness
//! knowing what a Discord interaction is.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use harness::gateway::{Button, Callback, Chat, Transport, Update};
use serde_json::{Value, json};
use tungstenite::Message;

pub const SURFACE: &str = "discord";
/// Discord accepts 2000 characters. The margin is the one the gateway uses for the tags it adds.
const LIMIT: usize = 1_900;
/// GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
const API: &str = "https://discord.com/api/v10";

/// Percent-encode the emoji in a reaction path. Discord wants the code points escaped, and an
/// unescaped surrogate or a stray space comes back 400 rather than reacting.
fn quote(text: &str) -> String {
    let mut out = String::new();
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

struct Inner {
    token: String,
    agent: ureq::Agent,
    incoming: Mutex<Receiver<Update>>,
    guilds: Vec<String>,
    require_mention: bool,
    me: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct Discord {
    inner: Arc<Inner>,
}

impl Discord {
    /// Read the two keys the surface itself needs (guild and mention policy) from the same config file,
    /// then start the websocket thread. Everything else in that section is the harness's.
    pub fn new(token: String, config: &Path) -> Result<Discord, String> {
        let text = std::fs::read_to_string(config)
            .map_err(|error| format!("{}: {error}", config.display()))?;
        let value: Value =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", config.display()))?;
        let section = value
            .pointer("/gateway/connectors/discord")
            .cloned()
            .unwrap_or(Value::Null);
        let guilds = section
            .get("allowed_guilds")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let require_mention = section
            .get("require_mention")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let (sender, receiver) = channel();
        let inner = Arc::new(Inner {
            token,
            // Discord's edge closes idle keep-alive sockets without telling anyone. A pooled
            // connection that sat through a quiet afternoon is dead, and the next POST writes
            // into it and waits out the whole timeout, which is exactly how a working bot goes
            // silent for hours. One fresh connection per call costs a handshake and removes it.
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .max_idle_connections(0)
                .max_idle_connections_per_host(0)
                .build(),
            incoming: Mutex::new(receiver),
            guilds,
            require_mention,
            me: Mutex::new(None),
        });
        {
            let inner = Arc::clone(&inner);
            std::thread::spawn(move || gateway_loop(inner, sender));
        }
        Ok(Discord { inner })
    }

    fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let url = format!("{API}{path}");
        let request = self
            .inner
            .agent
            .request(method, &url)
            .set("Authorization", &format!("Bot {}", self.inner.token))
            .set("User-Agent", "Hades (https://github.com/GuideboardLabs, 0.1)");
        let outcome = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        };
        match outcome {
            Ok(response) => {
                let text = response.into_string().unwrap_or_default();
                serde_json::from_str(&text).or_else(|_| Ok(Value::Null))
            }
            Err(ureq::Error::Status(code, response)) => {
                let text = response.into_string().unwrap_or_default();
                let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let why = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message in the answer");
                eprintln!("discord: {method} {path}: HTTP {code}: {why}");
                Err(format!("discord {method} {path}: HTTP {code}: {why}"))
            }
            Err(error) => {
                // A timed out POST is not retried: it may have landed, and a second one would
                // post the same reply twice. It is logged, because silence in a chat surface is
                // the symptom and the journal is the only place the cause can show up.
                eprintln!("discord: {method} {path} failed: {error}");
                Err(format!("discord {method} {path}: {error}"))
            }
        }
    }
}

impl Transport for Discord {
    fn updates(&mut self, _offset: Option<i64>, timeout: u64) -> Result<Vec<Update>, String> {
        let mut batch = Vec::new();
        let receiver = self
            .inner
            .incoming
            .lock()
            .unwrap_or_else(|held| held.into_inner());
        match receiver.recv_timeout(Duration::from_secs(timeout.max(1))) {
            Ok(update) => batch.push(update),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(batch),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the discord gateway thread stopped".to_string());
            }
        }
        while let Ok(update) = receiver.try_recv() {
            batch.push(update);
            if batch.len() >= 20 {
                break;
            }
        }
        Ok(batch)
    }

    fn send(&mut self, chat: &Chat, text: &str, buttons: &[Button]) -> Result<Option<i64>, String> {
        let mut body = json!({"content": text, "allowed_mentions": {"parse": []}});
        if !buttons.is_empty() {
            let row: Vec<Value> = buttons
                .iter()
                .take(5)
                .map(|button| {
                    json!({
                        "type": 2,
                        "style": 2,
                        "label": button.label,
                        "custom_id": button.data,
                    })
                })
                .collect();
            body["components"] = json!([{ "type": 1, "components": row }]);
        }
        let answer = self.rest(
            "POST",
            &format!("/channels/{}/messages", chat.id()),
            Some(body),
        )?;
        Ok(answer
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .map(|id| id as i64))
    }

    fn edit(
        &mut self,
        chat: &Chat,
        message: i64,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), String> {
        let mut body = json!({"content": text, "allowed_mentions": {"parse": []}});
        if buttons.is_empty() {
            body["components"] = json!([]);
        } else {
            let row: Vec<Value> = buttons
                .iter()
                .take(5)
                .map(|button| {
                    json!({
                        "type": 2,
                        "style": 2,
                        "label": button.label,
                        "custom_id": button.data,
                    })
                })
                .collect();
            body["components"] = json!([{ "type": 1, "components": row }]);
        }
        self.rest(
            "PATCH",
            &format!("/channels/{}/messages/{message}", chat.id()),
            Some(body),
        )?;
        Ok(())
    }

    fn typing(&mut self, chat: &Chat) -> Result<(), String> {
        self.rest("POST", &format!("/channels/{}/typing", chat.id()), None)?;
        Ok(())
    }

    /// A component press is an interaction that has to be acknowledged, and the acknowledgement needs the
    /// interaction token as well as its id. The harness passes back the `id` it was given, so the
    /// transport packs `id:token` into it here and unpacks it again.
    fn ack(&mut self, callback: &str, text: &str) -> Result<(), String> {
        let (id, token) = callback
            .split_once(':')
            .ok_or_else(|| "a discord callback needs its interaction token".to_string())?;
        self.rest(
            "POST",
            &format!("/interactions/{id}/{token}/callback"),
            Some(json!({"type": 4, "data": {"content": text, "flags": 64}})),
        )?;
        Ok(())
    }

    fn react(&mut self, chat: &Chat, message: i64, emoji: &str) -> Result<(), String> {
        // PUT is idempotent: reacting twice with the same face is one face, so a retry is safe.
        self.rest(
            "PUT",
            &format!(
                "/channels/{}/messages/{}/reactions/{}/@me",
                chat.id(),
                message,
                quote(emoji)
            ),
            None,
        )
        .map(|_| ())
    }

    fn remove(&mut self, chat: &Chat, message: i64) -> Result<(), String> {
        self.rest(
            "DELETE",
            &format!("/channels/{}/messages/{}", chat.id(), message),
            None,
        )
        .map(|_| ())
    }

    fn limit(&self) -> usize {
        LIMIT
    }

    fn buttons(&self) -> bool {
        true
    }

    /// An attachment arrives as a URL. Fetching it here keeps the harness's file path (an id the surface
    /// resolves) working without teaching it what a CDN is.
    fn file(&self, file_id: &str) -> Result<Vec<u8>, String> {
        let response = self
            .inner
            .agent
            .get(file_id)
            .call()
            .map_err(|error| format!("could not fetch the attachment: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 24 * 1024 * 1024 {
            return Err("the attachment is larger than 24 MB".to_string());
        }
        Ok(bytes)
    }
}

/// Connect, identify, read. A dropped socket is not an error a person needs to see; it is a reconnect.
fn gateway_loop(inner: Arc<Inner>, sender: Sender<Update>) {
    loop {
        if let Err(said) = read_gateway(&inner, &sender) {
            eprintln!("discord: gateway: {said}");
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn read_gateway(inner: &Arc<Inner>, sender: &Sender<Update>) -> Result<(), String> {
    let (mut socket, _) = tungstenite::connect("wss://gateway.discord.gg/?v=10&encoding=json")
        .map_err(|error| format!("connect: {error}"))?;
    let hello: Value = serde_json::from_str(&text_of(socket.read())?)
        .map_err(|error| format!("hello: {error}"))?;
    let interval = hello
        .pointer("/d/heartbeat_interval")
        .and_then(Value::as_u64)
        .unwrap_or(41_250);
    identify(&mut socket, inner)?;

    let started = std::time::Instant::now();
    let mut last_beat = started;
    loop {
        // A read timeout is the heartbeat's clock: the socket is only read when something arrives, so
        // the cadence has to come from the read itself rather than from a second writer thread.
        // SO_RCVTIMEO on the TCP stream under the TLS layer is what lets the read loop keep its own
        // heartbeat clock without a second writer thread.
        match socket.get_mut() {
            tungstenite::stream::MaybeTlsStream::Plain(tcp) => {
                tcp.set_read_timeout(Some(Duration::from_millis(500)))
                    .map_err(|error| format!("timeout: {error}"))?;
            }
            tungstenite::stream::MaybeTlsStream::Rustls(tls) => {
                tls.get_ref()
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .map_err(|error| format!("timeout: {error}"))?;
            }
            _ => {}
        }
        match socket.read() {
            Ok(Message::Close(frame)) => {
                let (code, reason) = frame
                    .map(|frame| (frame.code.to_string(), frame.reason.to_string()))
                    .unwrap_or_else(|| ("no code".to_string(), String::new()));
                return Err(format!("gateway closed the connection: {code} {reason}"));
            }
            Ok(message) => {
                let text = text_of(Ok::<Message, tungstenite::Error>(message))?;
                if let Some(value) = dispatch(inner, sender, &text)? {
                    let op = value.get("op").and_then(Value::as_u64).unwrap_or(0);
                    if op == 10 {
                        // A second hello after a resume: the interval still applies.
                        last_beat = std::time::Instant::now();
                    }
                }
            }
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if last_beat.elapsed() >= Duration::from_millis(interval) {
                    let beat = json!({"op": 1, "d": Value::Null});
                    socket
                        .send(Message::Text(beat.to_string().into()))
                        .map_err(|error| format!("heartbeat: {error}"))?;
                    last_beat = std::time::Instant::now();
                }
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(error) => return Err(format!("read: {error}")),
        }
    }
}

fn identify(
    socket: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    inner: &Arc<Inner>,
) -> Result<(), String> {
    let payload = json!({
        "op": 2,
        "d": {
            "token": inner.token,
            "intents": INTENTS,
            "properties": {"os": "linux", "browser": "hades", "device": "hades"},
        }
    });
    socket
        .send(Message::Text(payload.to_string().into()))
        .map_err(|error| format!("identify: {error}"))?;
    Ok(())
}

fn text_of(message: Result<Message, tungstenite::Error>) -> Result<String, String> {
    match message.map_err(|error| format!("read: {error}"))? {
        Message::Text(text) => Ok(text.to_string()),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .map_err(|error| format!("binary frame: {error}")),
        other => Ok(other.to_string()),
    }
}

/// One dispatch. Returns the frame so the caller can notice a hello after a resume.
fn dispatch(
    inner: &Arc<Inner>,
    sender: &Sender<Update>,
    text: &str,
) -> Result<Option<Value>, String> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Ok(None);
    };
    if value.get("op").and_then(Value::as_u64) != Some(0) {
        return Ok(Some(value));
    }
    match value.get("t").and_then(Value::as_str).unwrap_or("") {
        "READY" => {
            if let Some(id) = value.pointer("/d/user/id").and_then(Value::as_str) {
                *inner.me.lock().unwrap_or_else(|held| held.into_inner()) = Some(id.to_string());
            }
            let name = value
                .pointer("/d/user/username")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let guilds = value
                .pointer("/d/guilds")
                .and_then(Value::as_array)
                .map(|list| list.len())
                .unwrap_or(0);
            eprintln!("discord: connected as {name} in {guilds} guild(s)");
        }
        "MESSAGE_CREATE" => {
            if let Some(update) = message_update(inner, value.get("d").unwrap_or(&Value::Null)) {
                let _ = sender.send(update);
            }
        }
        "INTERACTION_CREATE" => {
            if let Some(update) = press_update(value.get("d").unwrap_or(&Value::Null)) {
                let _ = sender.send(update);
            }
        }
        _ => {}
    }
    Ok(Some(value))
}

fn message_update(inner: &Arc<Inner>, message: &Value) -> Option<Update> {
    if message
        .pointer("/author/bot")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let author = message.pointer("/author/id")?.as_str()?.to_string();
    let channel = message.get("channel_id")?.as_str()?.to_string();
    let guild = message.get("guild_id").and_then(Value::as_str);
    if let Some(guild) = guild
        && !inner.guilds.is_empty()
        && !inner.guilds.iter().any(|allowed| allowed == guild)
    {
        return None;
    }
    let mut text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let me = inner
        .me
        .lock()
        .unwrap_or_else(|held| held.into_inner())
        .clone();
    // Answering a message of ours is addressing us. A reply is how a person continues a thread
    // without typing the name again, and it is the only mention-free way in a guild.
    let answering_us = message
        .pointer("/message_reference/message_id")
        .is_some()
        && message
            .pointer("/referenced_message/author/id")
            .and_then(Value::as_str)
            .is_some_and(|author| me.as_deref() == Some(author));
    if guild.is_some() && inner.require_mention && !answering_us {
        let me = me?;
        let plain = format!("<@{me}>");
        let nick = format!("<@!{me}>");
        if !text.contains(&plain) && !text.contains(&nick) {
            return None;
        }
        text = text.replace(&plain, "").replace(&nick, "");
        text = text.trim().to_string();
    }
    let photo = message
        .get("attachments")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .and_then(|first| first.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if text.trim().is_empty() && photo.is_none() {
        return None;
    }
    Some(Update {
        id: message
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(0) as i64,
        from: author,
        chat: Chat::new(SURFACE, channel),
        text,
        callback: None,
        photo,
    })
}

fn press_update(interaction: &Value) -> Option<Update> {
    // Type 3 is a message component. Slash commands are a different permission model and are not part
    // of this pack.
    if interaction.get("type").and_then(Value::as_u64) != Some(3) {
        return None;
    }
    let data = interaction.get("data")?;
    let custom = data.get("custom_id")?.as_str()?.to_string();
    let id = interaction.get("id")?.as_str()?.to_string();
    let token = interaction.get("token")?.as_str()?.to_string();
    let author = interaction
        .pointer("/member/user/id")
        .or_else(|| interaction.pointer("/user/id"))?
        .as_str()?
        .to_string();
    let channel = interaction.get("channel_id")?.as_str()?.to_string();
    Some(Update {
        id: 0,
        from: author,
        chat: Chat::new(SURFACE, channel),
        text: String::new(),
        callback: Some(Callback {
            id: format!("{id}:{token}"),
            data: custom,
        }),
        photo: None,
    })
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    fn inner() -> Arc<Inner> {
        let (_, receiver) = channel();
        let inner = Arc::new(Inner {
            token: "unused".to_string(),
            agent: ureq::AgentBuilder::new().build(),
            incoming: Mutex::new(receiver),
            guilds: Vec::new(),
            require_mention: true,
            me: Mutex::new(Some("42".to_string())),
        });
        inner
    }

    fn message(author: &str, content: &str, guild: Option<&str>) -> Value {
        json!({
            "id": "1300000000000000001",
            "channel_id": "1300000000000000002",
            "guild_id": guild,
            "author": {"id": author, "bot": false},
            "content": content,
        })
    }

    #[test]
    fn a_guild_message_needs_a_mention_and_the_mention_is_stripped() {
        let inner = inner();
        assert!(message_update(&inner, &message("7", "hello", Some("9"))).is_none());
        let update = message_update(&inner, &message("7", "<@42> hello", Some("9")))
            .expect("a mention reaches the chat");
        assert_eq!(update.text, "hello");
        assert_eq!(update.chat, Chat::new(SURFACE, "1300000000000000002"));
    }

    #[test]
    fn a_reply_to_the_bot_is_answered_without_a_mention() {
        let inner = inner();
        let mut reply = message("7", "and then what", Some("9"));
        reply["message_reference"] = json!({"message_id": "1300000000000000009"});
        reply["referenced_message"] = json!({"id": "1300000000000000009", "author": {"id": "42", "bot": true}});
        let update = message_update(&inner, &reply).expect("a reply to us reaches the chat");
        assert_eq!(update.text, "and then what");

        // replying to somebody else is still not addressing us
        let mut other = message("7", "and then what", Some("9"));
        other["message_reference"] = json!({"message_id": "1300000000000000008"});
        other["referenced_message"] = json!({"id": "1300000000000000008", "author": {"id": "8", "bot": false}});
        assert!(message_update(&inner, &other).is_none());
    }

    #[test]
    fn an_emoji_is_escaped_for_the_reaction_path() {
        assert_eq!(quote("✅"), "%E2%9C%85");
        assert_eq!(quote("👀"), "%F0%9F%91%80");
    }

    #[test]
    fn a_direct_message_needs_no_mention() {
        let inner = inner();
        let update = message_update(&inner, &message("7", "hello", None)).expect("a dm");
        assert_eq!(update.text, "hello");
    }

    #[test]
    fn a_bot_author_and_a_foreign_guild_are_ignored() {
        let inner = inner();
        let mut bot = message("7", "<@42> hi", Some("9"));
        bot["author"]["bot"] = json!(true);
        assert!(message_update(&inner, &bot).is_none());

        let (_, receiver) = channel();
        let strict = Arc::new(Inner {
            token: "unused".to_string(),
            agent: ureq::AgentBuilder::new().build(),
            incoming: Mutex::new(receiver),
            guilds: vec!["100".to_string()],
            require_mention: false,
            me: Mutex::new(Some("42".to_string())),
        });
        assert!(message_update(&strict, &message("7", "hi", Some("9"))).is_none());
        assert!(message_update(&strict, &message("7", "hi", Some("100"))).is_some());
    }

    #[test]
    fn a_component_press_carries_the_interaction_token_back_to_ack() {
        let press = json!({
            "id": "1300000000000000003",
            "type": 3,
            "token": "interaction-token",
            "channel_id": "1300000000000000002",
            "member": {"user": {"id": "7"}},
            "data": {"custom_id": "a:9:1"}
        });
        let update = press_update(&press).expect("a press");
        let callback = update.callback.expect("a callback");
        assert_eq!(callback.data, "a:9:1");
        assert_eq!(callback.id, "1300000000000000003:interaction-token");
    }

    #[test]
    fn a_slash_command_is_not_a_press() {
        let command = json!({
            "id": "1", "type": 2, "token": "t",
            "channel_id": "2", "member": {"user": {"id": "7"}},
            "data": {"custom_id": "x"}
        });
        assert!(press_update(&command).is_none());
    }
}
