//! The model call, tested against a stub endpoint and against the things an endpoint does wrong.
//!
//! No network here: every test that needs an answer gets it from a socket in this process that says
//! exactly what the test wants it to say. That is the only way to test the *failure* paths, which matter
//! more than the success path: a model call that returns garbage must come back as a typed fault naming
//! what happened, not as an empty program that the tool then runs.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use harness::context::{Bundle, Section};
use harness::model::{self, OpenAi};
use harness::task::{Budget, TaskSpec};
use harness::{Harness, Key};

/// A socket that answers one request with the given bytes and hands back what it was asked.
fn stub(response: impl Into<String>) -> (String, std::thread::JoinHandle<String>) {
    let response = response.into();
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("a connection");
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut headers_end = None;
        loop {
            let read = stream.read(&mut chunk).expect("a read");
            if read == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..read]);
            if headers_end.is_none()
                && let Some(at) = find(&raw, b"\r\n\r\n")
            {
                headers_end = Some(at + 4);
            }
            if let Some(end) = headers_end {
                let length = content_length(&raw[..end]);
                if raw.len() >= end + length {
                    break;
                }
            }
        }
        stream.write_all(response.as_bytes()).expect("a write");
        // Dropping the stream closes it, which is what tells the client the answer ended.
        String::from_utf8_lossy(&raw).to_string()
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    for line in text.split("\r\n") {
        if let Some(rest) = line.strip_prefix("content-length:") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// A raw reply whose length is counted rather than typed, because a hand-written Content-Length is a test
/// bug that looks like a client bug.
fn raw(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn an_answer(content: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        an_answer_body(content).len(),
        an_answer_body(content)
    )
}

fn an_answer_body(content: &str) -> String {
    serde_json::json!({
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": content } }],
        "usage": { "prompt_tokens": 1234, "completion_tokens": 56 }
    })
    .to_string()
}

fn task() -> TaskSpec {
    TaskSpec::new("write a note", "/tmp/somewhere")
}

fn bundle() -> Bundle {
    Bundle::new(vec![Section::new("notes", "the notes")], vec![], 10)
}

#[test]
fn a_program_survives_the_round_trip_and_the_log_learns_what_it_cost() {
    let (url, server) = stub(an_answer("echo hello\n"));
    let model = OpenAi::new(&url, "stub").with_max_tokens(64);
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    assert_eq!(answer.program(), "echo hello");
    let usage = answer.usage().expect("usage");
    assert_eq!(usage.prompt_tokens, 1234);
    assert_eq!(usage.generated_tokens, 56);
    assert!(usage.is_known());

    // And the request that produced it carried the goal, the model name, and nothing secret.
    let request = server.join().expect("the stub thread");
    assert!(
        request.starts_with("POST /v1/chat/completions HTTP/1.1"),
        "{request}"
    );
    assert!(request.contains("write a note"), "{request}");
    assert!(request.contains("\"model\":\"stub\""), "{request}");
    assert!(
        !request.to_ascii_lowercase().contains("authorization"),
        "{request}"
    );
}

#[test]
fn a_fenced_answer_from_a_model_arrives_unfenced() {
    // What a model actually does when nobody tells it not to fence.
    let body = "```sh\necho fenced\n```";
    let (url, server) = stub(an_answer(body));
    let model = OpenAi::new(&url, "stub");
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    assert_eq!(answer.program(), "echo fenced");
    let _ = server.join();
}

#[test]
fn a_key_is_sent_when_there_is_one_and_the_fault_never_names_it() {
    // The endpoint refuses, which is what a wrong key looks like from here. The fault says so without
    // repeating the secret into a log.
    const SENTINEL: &str = "sentinel-key-that-must-not-be-echoed";
    let (url, server) = stub(raw("401 Unauthorized", "application/json", "{}"));
    let model = OpenAi::new(&url, "stub").with_key(Key::Env("HADES_STUB_KEY".to_string()));
    // Safety: this is a single-threaded point in the test before the call, and the variable is removed
    // immediately after; no other test reads this name.
    unsafe { std::env::set_var("HADES_STUB_KEY", SENTINEL) };
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    unsafe { std::env::remove_var("HADES_STUB_KEY") };
    assert!(fault.detail().contains("401"), "{}", fault.detail());
    assert!(
        !fault.detail().contains(SENTINEL),
        "the fault leaked the key"
    );

    // And the key did reach the endpoint, in a header, which is where it belongs.
    let request = server.join().expect("the stub thread");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer sentinel-key"),
        "{request}"
    );
}

#[test]
fn a_web_page_instead_of_json_is_a_fault_rather_than_an_empty_program() {
    let (url, server) = stub(raw(
        "200 OK",
        "text/html",
        "<html><body>not a model</body></html>",
    ));
    let model = OpenAi::new(&url, "stub");
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(
        fault.detail().contains("not with JSON"),
        "{}",
        fault.detail()
    );
    let _ = server.join();
}

#[test]
fn an_error_object_behind_a_200_is_still_an_error() {
    // Some gateways answer 200 with an error body. Reading `choices` would give a missing field, which is a
    // worse fault than the one the endpoint actually sent.
    let (url, server) = stub(raw(
        "200 OK",
        "application/json",
        "{\"error\":{\"message\":\"model not found\"}}",
    ));
    let model = OpenAi::new(&url, "stub");
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(
        fault.detail().contains("model not found"),
        "{}",
        fault.detail()
    );
    let _ = server.join();
}

#[test]
fn an_answer_with_no_program_in_it_is_a_fault_that_says_what_came_back() {
    let (url, server) = stub(raw(
        "200 OK",
        "application/json",
        &serde_json::json!({ "choices": [{ "message": { "content": "  " } }] }).to_string(),
    ));
    let model = OpenAi::new(&url, "stub");
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(fault.detail().contains("no program"), "{}", fault.detail());
    let _ = server.join();
}

#[test]
fn nothing_listening_is_a_fault_naming_the_address_and_not_a_hang() {
    // Port 9 is the discard port and nothing here listens on it.
    let model = OpenAi::new("http://127.0.0.1:9/v1/chat/completions", "stub")
        .with_timeout(Duration::from_secs(2));
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(fault.detail().contains("127.0.0.1:9"), "{}", fault.detail());
}

#[test]
fn a_key_that_is_missing_is_refused_by_name_before_anything_is_sent() {
    let (url, server) = stub(an_answer("true"));
    let model = OpenAi::new(&url, "stub").with_key(Key::Env("HADES_NO_SUCH_KEY".to_string()));
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(
        fault.detail().contains("HADES_NO_SUCH_KEY"),
        "{}",
        fault.detail()
    );
    // Nothing was sent, so the stub never sees a request. It is dropped rather than joined.
    drop(server);
}

#[test]
fn a_model_that_thinks_and_says_nothing_is_a_fault_that_names_the_thinking() {
    // This is the first thing a real local model did through this client: 1349 characters of reasoning in
    // `reasoning_content`, an empty `content`, and `finish_reason: length`. The fault has to say that,
    // because "no program" without the reason sends the reader to the wrong file.
    let (url, server) = stub(raw(
        "200 OK",
        "application/json",
        &serde_json::json!({
            "choices": [{
                "finish_reason": "length",
                "message": { "content": "", "reasoning_content": "let me think about what files means" }
            }]
        })
        .to_string(),
    ));
    let model = OpenAi::new(&url, "stub");
    let fault = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect_err("refused");
    assert!(fault.detail().contains("reasoning"), "{}", fault.detail());
    assert!(
        fault.detail().contains("--thinking off"),
        "{}",
        fault.detail()
    );
    let _ = server.join();
}

#[test]
fn a_preflight_names_the_model_and_the_provider_when_the_key_is_missing() {
    // A strict request fails on the name it was given, not on a fall-through. The key's *source* is named
    // and the key itself is not, which is the same rule every other key fault follows.
    let model = OpenAi::new("https://ollama.com/v1/chat/completions", "strict-ghost")
        .with_key(Key::Env("HADES_PREFLIGHT_KEY_THAT_IS_NOT_SET".to_string()));
    let fault = model.preflight().expect_err("refused");
    assert!(
        fault.detail().contains("strict-ghost"),
        "the fault does not name the model: {}",
        fault.detail()
    );
    assert!(
        fault.detail().contains("ollama-cloud"),
        "the fault does not name the provider: {}",
        fault.detail()
    );
    assert!(
        fault
            .detail()
            .contains("HADES_PREFLIGHT_KEY_THAT_IS_NOT_SET"),
        "{}",
        fault.detail()
    );
}

#[test]
fn a_preflight_refuses_an_endpoint_that_is_not_configured() {
    let model = OpenAi::new("", "strict-ghost");
    let fault = model.preflight().expect_err("refused");
    assert!(
        fault.detail().contains("strict-ghost"),
        "{}",
        fault.detail()
    );
    assert!(
        fault.detail().contains("endpoint"),
        "the fault does not say what is missing: {}",
        fault.detail()
    );

    // An endpoint with no scheme is the same class of mistake: it would spend a call to find out, and a
    // preflight exists so it does not.
    let model = OpenAi::new("127.0.0.1:8110/v1/chat/completions", "strict-ghost");
    let fault = model.preflight().expect_err("refused");
    assert!(
        fault.detail().contains("strict-ghost"),
        "{}",
        fault.detail()
    );
    assert!(fault.detail().contains("scheme"), "{}", fault.detail());
}

#[test]
fn a_preflight_that_passes_spends_no_call() {
    // The listener is never accepted from. A preflight that dialled it would leave a connection in the
    // backlog, and the check below would find one.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("an address").port();
    listener
        .set_nonblocking(true)
        .expect("a nonblocking listener");
    let model = OpenAi::new(
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        "strict-ghost",
    );
    model.preflight().expect("a local endpoint needs no key");
    assert!(
        listener.accept().is_err(),
        "the preflight dialled the endpoint"
    );
}

#[test]
fn asking_for_no_thinking_is_a_field_in_the_request_and_asking_for_nothing_is_no_field() {
    let (url, server) = stub(an_answer("true"));
    let model = OpenAi::new(&url, "stub").with_thinking(false);
    let body: serde_json::Value =
        serde_json::from_str(&model.body(&task(), &bundle(), &Budget::new(4096), false))
            .expect("valid JSON");
    assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);

    // A model nobody said anything about sends nothing, because a cloud endpoint that does not know this
    // field will refuse the whole request rather than ignore it.
    let quiet = OpenAi::new(&url, "stub");
    let body: serde_json::Value =
        serde_json::from_str(&quiet.body(&task(), &bundle(), &Budget::new(4096), false))
            .expect("valid JSON");
    assert!(body.get("chat_template_kwargs").is_none());

    // And the field does go out on the wire, not just into a string this test built. One real call, so the
    // stub has something to answer and this test does not join a server that never heard from anyone.
    let answer = quiet
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    assert_eq!(answer.program(), "true");
    let request = server.join().expect("the stub thread");
    assert!(!request.contains("chat_template_kwargs"), "{request}");
}

#[test]
fn a_preset_that_names_a_cloud_model_carries_its_key_by_name() {
    let preset = model::preset("deepseek-v4.1-flash").expect("the preset");
    assert!(preset.endpoint.starts_with("https://"));
    assert_eq!(preset.key, Some(Key::Home("OLLAMA_API_KEY".to_string())));
    let built = model::from_preset(&preset);
    // What a person may see never contains a key, because there is no key in the value to leak.
    assert!(!built.display().contains("Bearer"));
    assert!(built.display().contains("deepseek-v4.1-flash"));
}

#[test]
fn an_edit_object_arrives_as_edits_not_as_a_program() {
    let body = r#"{"edits":[{"path":"a.txt","old":"x","new":"y"}]}"#;
    let (url, server) = stub(an_answer(body));
    let model = OpenAi::new(&url, "stub");
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    let edits = answer.edits().expect("the answer is edits");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].path, "a.txt");
    assert_eq!(answer.program(), "", "an edit answer has no program");
    assert!(
        answer.rendered().contains("--- a.txt"),
        "{}",
        answer.rendered()
    );
    server.join().expect("the stub thread");
}

#[test]
fn a_fenced_edit_object_is_read_as_edits() {
    let body = "```json\n{\"edits\":[{\"path\":\"a.txt\",\"old\":\"x\",\"new\":\"y\"}]}\n```";
    let (url, server) = stub(an_answer(body));
    let model = OpenAi::new(&url, "stub");
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    assert_eq!(answer.edits().expect("edits").len(), 1);
    server.join().expect("the stub thread");
}

#[test]
fn a_malformed_edit_object_is_a_refusal_the_next_attempt_can_read() {
    let body = r#"{"edits":[{"path":"a.txt","old":}]}"#;
    let (url, server) = stub(an_answer(body));
    let model = OpenAi::new(&url, "stub");
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer, not a fault");
    let said = answer.refusal().expect("a refusal");
    assert!(said.contains("could not be read"), "{said}");
    assert_eq!(answer.program(), "");
    server.join().expect("the stub thread");
}

#[test]
fn a_brace_group_is_still_a_program() {
    let (url, server) = stub(an_answer("{ echo hi; }"));
    let model = OpenAi::new(&url, "stub");
    let answer = model
        .run(&task(), &bundle(), &Budget::new(4096), None)
        .expect("an answer");
    assert_eq!(answer.program(), "{ echo hi; }");
    assert!(answer.edits().is_none());
    server.join().expect("the stub thread");
}
