//! Telegram's Bot API, in one implementation of the transport seam.
//!
//! Long polling, not a webhook: a poll needs no public listener, no TLS termination and no inbound port,
//! and the gateway already runs as a local process beside the daemon. The cost is a held connection and
//! a 50-second timeout, which is cheaper than the surface a webhook would add.
//!
//! The token appears in the URL by Telegram's design, so it is visible in the curl process's arguments
//! while a call is in flight. Nothing here prints it, and the URL is never put in a log line or a chat
//! message; that is the most this client can do without a proxy.

use std::time::Duration;

use serde_json::{Value, json};

use crate::http;

use super::{Button, Callback, Transport, Update};

#[derive(Clone)]
pub struct Telegram {
    token: String,
    base: String,
}

impl Telegram {
    pub fn new(token: impl Into<String>) -> Telegram {
        Telegram {
            token: token.into(),
            base: "https://api.telegram.org".to_string(),
        }
    }

    /// Point the client at another origin. Only a test uses this, and it keeps the real client free of a
    /// second code path.
    pub fn at(token: impl Into<String>, base: impl Into<String>) -> Telegram {
        Telegram {
            token: token.into(),
            base: base.into(),
        }
    }

    fn call(&self, method: &str, body: &Value, timeout: u64) -> Result<Value, String> {
        let url = format!("{}/bot{}/{}", self.base, self.token, method);
        let reply = http::post(
            &url,
            &body.to_string(),
            &[("Content-Type", "application/json".to_string())],
            Duration::from_secs(timeout),
        )
        .map_err(|fault| fault.detail().to_string())?;
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|error| format!("telegram's answer would not read: {error}"))?;
        if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Ok(value);
        }
        Err(value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("telegram refused the call without a description")
            .to_string())
    }
}

impl Transport for Telegram {
    fn updates(&mut self, offset: Option<i64>, timeout: u64) -> Result<Vec<Update>, String> {
        let body = json!({
            "offset": offset,
            "timeout": timeout,
            "allowed_updates": ["message", "callback_query"],
        });
        // The transport waits longer than the poll, or curl would time out on a healthy long poll.
        let value = self.call("getUpdates", &body, timeout + 15)?;
        let result = value
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(result.iter().filter_map(parse_update).collect())
    }

    fn send(&mut self, chat: i64, text: &str, buttons: &[Button]) -> Result<Option<i64>, String> {
        let mut body = json!({"chat_id": chat, "text": text, "parse_mode": "HTML"});
        if !buttons.is_empty() {
            body["reply_markup"] = keyboard(buttons);
        }
        let value = self.call("sendMessage", &body, 20)?;
        Ok(value.pointer("/result/message_id").and_then(Value::as_i64))
    }

    fn edit(
        &mut self,
        chat: i64,
        message: i64,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), String> {
        let mut body = json!({"chat_id": chat, "message_id": message, "text": text,
                              "parse_mode": "HTML"});
        if !buttons.is_empty() {
            body["reply_markup"] = keyboard(buttons);
        }
        self.call("editMessageText", &body, 20).map(|_| ())
    }

    fn typing(&mut self, chat: i64) -> Result<(), String> {
        self.call(
            "sendChatAction",
            &json!({"chat_id": chat, "action": "typing"}),
            15,
        )
        .map(|_| ())
    }

    fn ack(&mut self, callback: &str, text: &str) -> Result<(), String> {
        self.call(
            "answerCallbackQuery",
            &json!({"callback_query_id": callback, "text": text}),
            20,
        )
        .map(|_| ())
    }
    /// Telegram keeps the bytes; getFile names a path, and the file endpoint serves it. curl is used rather
    /// than the JSON client because the answer is binary and http::post is a string client.
    fn file(&self, file_id: &str) -> Result<Vec<u8>, String> {
        use std::process::Command;
        let meta_url = format!(
            "{}/bot{}/getFile?file_id={}",
            self.base,
            self.token,
            urlencode(file_id)
        );
        let meta = Command::new("curl")
            .arg("-sS")
            .arg("--max-time")
            .arg("30")
            .arg(&meta_url)
            .output()
            .map_err(|error| format!("could not name the file: {error}"))?;
        if !meta.status.success() {
            return Err(String::from_utf8_lossy(&meta.stderr).trim().to_string());
        }
        let parsed: Value = serde_json::from_slice(&meta.stdout)
            .map_err(|error| format!("telegram answered with something else: {error}"))?;
        let path = parsed
            .pointer("/result/file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("telegram refused the file: {}", parsed))?;
        let file_url = format!("{}/file/bot{}/{}", self.base, self.token, path);
        let file = Command::new("curl")
            .arg("-sS")
            .arg("--max-time")
            .arg("120")
            .arg(&file_url)
            .output()
            .map_err(|error| format!("could not download the file: {error}"))?;
        if !file.status.success() {
            return Err(String::from_utf8_lossy(&file.stderr).trim().to_string());
        }
        if file.stdout.is_empty() {
            return Err("telegram sent an empty file".to_string());
        }
        Ok(file.stdout)
    }
}

fn keyboard(buttons: &[Button]) -> Value {
    let row: Vec<Value> = buttons
        .iter()
        .map(|button| json!({"text": button.label, "callback_data": button.data}))
        .collect();
    json!({"inline_keyboard": [row]})
}

fn parse_update(value: &Value) -> Option<Update> {
    let id = value.get("update_id")?.as_i64()?;
    if let Some(message) = value.get("message") {
        let photo = message
            .get("photo")
            .and_then(Value::as_array)
            .and_then(|sizes| sizes.last())
            .and_then(|size| size.get("file_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let text = message
            .get("text")
            .or_else(|| message.get("caption"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if text.is_empty() && photo.is_none() {
            return None;
        }
        return Some(Update {
            id,
            from: message.pointer("/from/id")?.as_i64()?,
            chat: message.pointer("/chat/id")?.as_i64()?,
            text,
            callback: None,
            photo,
        });
    }
    let query = value.get("callback_query")?;
    Some(Update {
        id,
        from: query.pointer("/from/id")?.as_i64()?,
        chat: query.pointer("/message/chat/id")?.as_i64()?,
        text: String::new(),
        callback: Some(Callback {
            id: query.get("id")?.as_str()?.to_string(),
            data: query.get("data")?.as_str()?.to_string(),
        }),
        photo: None,
    })
}

/// Percent-encoding for a query value. File ids are short and usually URL-safe, but a percent or a slash
/// would silently truncate the request.
fn urlencode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_update;

    #[test]
    fn a_photo_is_an_update_with_the_largest_size() {
        let value = serde_json::json!({
            "update_id": 9,
            "message": {
                "from": {"id": 7},
                "chat": {"id": 7},
                "photo": [
                    {"file_id": "small", "width": 90},
                    {"file_id": "large", "width": 1440}
                ],
                "caption": "what is this"
            }
        });
        let update = parse_update(&value).expect("an update");
        assert_eq!(update.photo.as_deref(), Some("large"));
        assert_eq!(update.text, "what is this");
    }

    #[test]
    fn an_update_reads_a_message_and_a_button_press() {
        let message = serde_json::json!({
            "update_id": 7,
            "message": {"from": {"id": 11}, "chat": {"id": 11}, "text": "hello"}
        });
        let parsed = parse_update(&message).expect("a message");
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.from, 11);
        assert_eq!(parsed.text, "hello");

        let press = serde_json::json!({
            "update_id": 8,
            "callback_query": {
                "id": "cb-1",
                "from": {"id": 11},
                "data": "a:3:1",
                "message": {"chat": {"id": 11}}
            }
        });
        let parsed = parse_update(&press).expect("a press");
        assert_eq!(parsed.callback.expect("a callback").data, "a:3:1");
    }

    #[test]
    fn an_update_this_surface_does_not_read_is_skipped() {
        assert!(parse_update(&serde_json::json!({"update_id": 9})).is_none());
    }
}
