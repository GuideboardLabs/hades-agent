use super::{
    app::{App, Overlay, View},
    backend::{self, Options},
    editor::Editor,
    quota, ui,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;

fn demo() -> App {
    App::new(
        Options::parse(&[
            "--demo".into(),
            "--root".into(),
            std::env::current_dir().unwrap().display().to_string(),
        ])
        .unwrap(),
    )
}
fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.event(Event::Key(KeyEvent::new(code, modifiers)));
}
fn render(app: &mut App, w: u16, h: u16, frame: u64) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| ui::draw(f, app, frame)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn renderer_survives_resizes_and_every_view_and_overlay() {
    let mut app = demo();
    for (w, h) in [
        (20, 10),
        (56, 18),
        (80, 24),
        (100, 40),
        (140, 52),
        (220, 70),
    ] {
        for view in View::ALL {
            app.switch(view);
            let text = render(&mut app, w, h, 2);
            assert!(text.contains("HADES"));
        }
        app.switch(View::Chat);
        for overlay in [
            Overlay::Help,
            Overlay::Palette,
            Overlay::Models,
            Overlay::Find,
            Overlay::Detail("Detail".into(), "a long detail\n".repeat(80)),
        ] {
            app.overlay = Some(overlay);
            render(&mut app, w, h, 3);
        }
        app.overlay = None;
    }
}

#[test]
fn banner_and_messages_are_framed_and_demo_is_explicit() {
    let mut app = demo();
    app.follow = false;
    let text = render(&mut app, 140, 52, 0);
    assert!(text.contains("HADES AGENT"));
    assert!(text.contains("DEMO DATA"));
    assert!(text.contains("  YOU "));
    assert!(text.contains("  HADES "));
    assert!(text.contains("▌ "));
    assert!(text.contains(" ▐"));
    assert!(text.contains("▙▄▄"));
    assert!(text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
}

#[test]
fn animation_moves_flames_and_preserves_the_face() {
    let source = super::mascot::pixels(None);
    let a = super::mascot::pixels(Some(0.0));
    let b = super::mascot::pixels(Some(0.9));
    assert_ne!(&a[..96 * 42], &b[..96 * 42]);
    assert_eq!(&source[96 * 58..], &a[96 * 58..]);
    assert_eq!(&source[96 * 58..], &b[96 * 58..]);
    // Protect the non-blue forehead as well as the lower face across many frames.
    for frame in 0..40 {
        let animated = super::mascot::pixels(Some(frame as f64 * 0.09));
        for (i, pixel) in source.iter().enumerate().skip(96 * 42) {
            let [r, g, b] = pixel.map(i32::from);
            if b <= 95 || b <= r + 55 || g <= r + 10 {
                assert_eq!(*pixel, animated[i]);
            }
        }
    }
    let mut app = demo();
    app.motion = false;
    assert_eq!(render(&mut app, 140, 52, 0), render(&mut app, 140, 52, 6));
}

#[test]
fn imported_skull_renders_two_truecolor_pixels_per_terminal_cell() {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
        widgets::Widget,
    };
    let area = Rect::new(0, 0, 40, 20);
    let mut buffer = Buffer::empty(area);
    let background = Color::Rgb(24, 38, 52);
    buffer.set_style(area, Style::default().bg(background));
    super::mascot::Mascot {
        frame: 0,
        motion: false,
    }
    .render(area, &mut buffer);
    assert!(buffer.content.iter().all(|cell| cell.symbol() == "▀"));
    for (x, y) in [(0, 0), (39, 0), (0, 19), (39, 19)] {
        assert_eq!(buffer[(x, y)].fg, background);
        assert_eq!(buffer[(x, y)].bg, background);
    }
    // Preserve the original dark nose shading, including its subtle blue tint.
    let mut original = Buffer::empty(area);
    super::mascot::Mascot {
        frame: 0,
        motion: false,
    }
    .render(area, &mut original);
    for x in [19, 20] {
        assert_eq!(buffer[(x, 14)].fg, original[(x, 14)].fg);
        assert_eq!(buffer[(x, 14)].bg, original[(x, 14)].bg);
    }
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| matches!(cell.fg, Color::Rgb(r,g,b) if b > 150 && b > r && g > r))
    );
    assert!(buffer.content.iter().any(|cell| matches!(cell.bg, Color::Rgb(r,g,b) if r > 120 && (r as i32-g as i32).abs() < 20 && (r as i32-b as i32).abs() < 20)));
    let mut other = Buffer::empty(area);
    other.set_style(area, Style::default().bg(background));
    super::mascot::Mascot {
        frame: 42,
        motion: false,
    }
    .render(area, &mut other);
    assert_eq!(buffer, other);
    for width in [28, 40] {
        let mut terminal = Terminal::new(TestBackend::new(width, width / 2)).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    super::mascot::Mascot {
                        frame: 3,
                        motion: true,
                    },
                    f.area(),
                )
            })
            .unwrap();
    }
}

#[test]
fn unicode_editor_never_splits_a_codepoint_and_tracks_wide_columns() {
    let mut e = Editor::default();
    e.insert("a界é");
    assert_eq!(e.position(10), (0, 4));
    e.left();
    e.backspace();
    assert_eq!(e.text, "aé");
    assert_eq!(e.cursor, 1);
    e.delete();
    assert_eq!(e.text, "a");
    e.insert("\n火\nnext");
    e.line_up();
    e.home();
    e.insert("x");
    assert!(e.text.contains("x火"));
    assert!(e.text.is_char_boundary(e.cursor));
    e.set("12345");
    assert_eq!(e.position(5), (1, 0));
    e.insert("\u{1b}[31m\u{0}\r");
    assert!(!e.text.contains('\u{1b}'));
    assert!(!e.text.contains('\r'));
}

#[test]
fn quota_uses_actual_balance_and_never_treats_missing_fields_as_zero() {
    assert!(quota::parse("Ollama Cloud", &json!({})).is_err());
    let q = quota::parse(
        "Ollama Cloud",
        &json!({"included":{"balance_usd":75,"allowance_usd":100},"purchased":{"balance_usd":12}}),
    )
    .unwrap();
    assert_eq!(q.windows[0].used, 25.0);
    assert_eq!(q.windows[0].fraction(), 0.25);
    let q = quota::parse(
        "Ollama Cloud",
        &json!({"included":{"session":{"remaining_percent":80},"weekly":{"remaining_percent":20}}}),
    )
    .unwrap();
    assert_eq!(q.windows[0].used, 20.0);
    assert_eq!(q.windows[1].used, 80.0);
    assert!(
        quota::parse(
            "Ollama Cloud",
            &json!({"included":{"session":{"remaining_percent":110}}})
        )
        .is_err()
    );
    let q=quota::parse("OpenRouter",&json!({"data":{"effective_budget":{"limit_usd":20,"spend_usd":8,"reset_interval":"monthly"}}})).unwrap();
    assert_eq!(q.windows[0].fraction(), 0.4);
    let q = quota::parse(
        "OpenRouter",
        &json!({"data":{"limit":10,"limit_remaining":3,"usage":7}}),
    )
    .unwrap();
    assert_eq!(q.windows[0].used, 7.0);
}

#[test]
fn session_names_cannot_escape_state_directory() {
    for id in ["../secret", "/etc/passwd", "a/b", "..", "a\\b", ""] {
        assert!(backend::session_path(std::path::Path::new("/tmp/state"), id).is_err());
    }
    assert!(backend::session_path(std::path::Path::new("/tmp/state"), "abc-123_a").is_ok());
}

#[test]
fn cancellation_repairs_tool_protocol_and_compaction_drops_whole_turns() {
    let mut messages = vec![
        json!({"role":"system","content":"system"}),
        json!({"role":"user","content":"old".repeat(1000)}),
        json!({"role":"assistant","tool_calls":[{"id":"a"},{"id":"b"}]}),
        json!({"role":"tool","tool_call_id":"a","content":"done"}),
        json!({"role":"user","content":"new"}),
    ];
    backend::repair_tool_history(&mut messages);
    assert_eq!(messages[4]["tool_call_id"], "b");
    assert_eq!(messages[5]["role"], "user");
    crate::agent::compact_context(&mut messages, 100);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "system");
    assert!(messages[1]["content"]
        .as_str()
        .unwrap()
        .contains("trimmed from context"));
    assert_eq!(messages[2]["content"], "new");
}

#[test]
fn palette_clear_and_streaming_update_state_without_live_calls() {
    let mut app = demo();
    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(matches!(app.overlay, Some(Overlay::Palette)));
    app.event(Event::Paste("usage".into()));
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.view, View::Usage);
    app.command("/clear");
    assert_eq!(app.visible_from, app.messages.len());
    assert!(!app.messages.is_empty());
    app.switch(View::Chat);
    app.apply(backend::Event::Delta("hello".into()));
    let first = render(&mut app, 100, 40, 0);
    assert!(first.contains("hello"));
    app.apply(backend::Event::Delta(" world".into()));
    let second = render(&mut app, 100, 40, 0);
    assert!(second.contains("hello world"));
    app.command("/new");
    assert!(app.messages.is_empty());
    assert_eq!(app.visible_from, 0);
}

#[test]
fn quit_during_a_turn_waits_for_the_worker_and_keeps_the_draft() {
    let mut app = demo();
    app.busy = true;
    app.submit("keep this draft".into());
    assert_eq!(app.input.text, "keep this draft");
    key(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(app.closing);
    assert!(!app.quit);
    app.apply(backend::Event::Finished {
        answer: Ok(None),
        transcript: Vec::new(),
    });
    assert!(app.quit);
    assert!(app.messages.last().unwrap().text.contains("cancelled"));
}

#[test]
fn approval_view_round_trips_through_the_real_daemon_broker() {
    use crate::approval::ApprovalBroker;
    use std::time::{Duration, Instant};
    let state = std::env::temp_dir().join(backend::new_id());
    let daemon = crate::daemon::Daemon::bind(&state, state.join("daemon.sock")).unwrap();
    let broker = crate::approval::OnDisk::open(state.join("approvals.jsonl")).unwrap();
    let question = broker
        .request("tui-test", "shell", "Run the reviewed fixture")
        .unwrap();
    let worker = std::thread::spawn(move || daemon.serve());
    let started = Instant::now();
    while crate::daemon::live(&state).is_none() {
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut options = demo().options;
    options.state = state.clone();
    let overview = backend::overview(&options);
    assert!(overview.daemon);
    assert_eq!(overview.approvals.len(), 1);
    backend::daemon_request(
        &state,
        json!({"op":"answer","id":question.id.0,"decision":"allow","scope":"once","by":"tui-test"}),
    )
    .unwrap();
    assert!(backend::overview(&options).approvals.is_empty());
    assert_eq!(
        broker.answer(question.id).unwrap().unwrap().scope,
        crate::approval::Scope::Once
    );
    backend::daemon_request(&state, json!({"op":"shutdown","by":"tui-test"})).unwrap();
    worker.join().unwrap().unwrap();
    std::fs::remove_dir_all(state).unwrap();
}

#[test]
fn tool_results_attach_to_the_matching_card_when_delegates_emit_activity() {
    let mut app = demo();
    app.command("/new");
    let label = crate::agent::label("delegate", r#"{"tasks":[]}"#);
    app.apply(backend::Event::Tool(true, label.clone()));
    app.apply(backend::Event::Tool(true, "delegate: child task".into()));
    app.apply(backend::Event::Tool(false, label));
    app.apply(backend::Event::Finished { answer: Ok(Some("Done".into())), transcript: vec![json!({"role":"user","content":"Delegate"}),json!({"role":"assistant","tool_calls":[{"id":"outer","function":{"name":"delegate","arguments":"{\"tasks\":[]}"}}]}),json!({"role":"tool","tool_call_id":"outer","content":"the parent result"})] });
    assert_eq!(app.messages[0].detail, "the parent result");
    assert!(app.messages[1].detail.is_empty());
    assert!(app.messages.iter().all(|m| m.status != "running"));
}

#[test]
fn daemon_stream_separates_reasoning_and_requires_a_final_receipt() {
    use std::io::{BufRead, Write};
    for completed in [true, false] {
        let state = std::env::temp_dir().join(backend::new_id());
        std::fs::create_dir_all(&state).unwrap();
        let socket = state.join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::write(
            state.join("daemon.json"),
            json!({"pid":std::process::id(),"socket":socket}).to_string(),
        )
        .unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["policy"], "guarded");
            assert_eq!(request["session"], "fixture-run");
            writeln!(stream, "{}", json!({"started":true})).unwrap();
            writeln!(
                stream,
                "{}",
                json!({"progress":"reasoning","delta":"private planning"})
            )
            .unwrap();
            writeln!(
                stream,
                "{}",
                json!({"progress":"content","delta":"visible progress"})
            )
            .unwrap();
            if completed {
                writeln!(
                    stream,
                    "{}",
                    json!({"finished":true,"ended":"accepted","code":0})
                )
                .unwrap();
            }
        });
        let mut options = demo().options;
        options.state = state.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(256);
        backend::governed(options, "fixture-run".into(), "fixture goal".into(), tx);
        let mut reasoning = false;
        let mut content = false;
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                backend::Event::Reasoning(s) => reasoning = s == "private planning",
                backend::Event::Delta(s) => content = s == "visible progress",
                backend::Event::Finished { answer, .. } => {
                    assert_eq!(answer.is_ok(), completed);
                    break;
                }
                _ => {}
            }
        }
        assert!(reasoning && content);
        server.join().unwrap();
        std::fs::remove_dir_all(state).unwrap();
    }
}

/// Real HTTP/SSE transport, no provider credentials or external service.
fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in responses {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut request = vec![0; length];
            reader.read_exact(&mut request).unwrap();
            requests.push(serde_json::from_slice(&request).unwrap());
            let mut stream = reader.into_inner();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (endpoint, worker)
}
fn sse(delta: serde_json::Value, usage: serde_json::Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":delta}]}),
        json!({"choices":[],"usage":usage})
    )
}

#[test]
fn streaming_usage_only_frames_count_once_and_follow_client_clones() {
    use std::sync::{Arc, Mutex};
    let (endpoint, server) = serve(vec![
        sse(
            json!({"content":"hello"}),
            json!({"prompt_tokens":12,"completion_tokens":4}),
        ),
        sse(
            json!({"content":"child"}),
            json!({"prompt_tokens":20,"completion_tokens":8}),
        ),
        sse(json!({"content":"no usage"}), json!({})),
    ]);
    let log = Arc::new(Mutex::new(Vec::new()));
    let client = crate::model::OpenAi::new(&endpoint, "fixture").with_usage_log(log.clone());
    let reply = client.chat(&[], None, &|_, _| {}).unwrap();
    assert_eq!(reply.usage.unwrap().generated_tokens, 4);
    client.clone().chat(&[], None, &|_, _| {}).unwrap();
    assert!(client.chat(&[], None, &|_, _| {}).unwrap().usage.is_none());
    let requests = server.join().unwrap();
    assert!(
        requests
            .iter()
            .all(|v| v["stream_options"]["include_usage"] == true)
    );
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log.iter().map(|u| u.prompt_tokens).sum::<u64>(), 32);
}

#[test]
fn real_tool_loop_streams_reads_saves_and_resumes_a_new_branch() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc::sync_channel};
    let dir = std::env::temp_dir().join(backend::new_id());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fixture.txt"), "a verified fixture").unwrap();
    let (endpoint, server) = serve(vec![
        sse(
            json!({"tool_calls":[{"index":0,"id":"read-1","function":{"name":"read_file","arguments":"{\"path\":\"fixture.txt\"}"}}]}),
            json!({"prompt_tokens":100,"completion_tokens":10}),
        ),
        sse(
            json!({"content":"I read a verified fixture."}),
            json!({"prompt_tokens":140,"completion_tokens":8}),
        ),
    ]);
    let mut options = demo().options;
    options.demo = false;
    options.root = dir.clone();
    options.state = dir.join("state");
    options.model = "fixture".into();
    options.endpoint = Some(endpoint);
    let log = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = sync_channel(256);
    backend::start_chat(
        options.clone(),
        backend::ChatRequest {
            id: "fixture-session".into(),
            prompt: "Read fixture.txt".into(),
            transcript: Vec::new(),
            mode: crate::mode::Mode::Read,
        },
        log.clone(),
        Arc::new(AtomicBool::new(false)),
        tx,
    );
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(std::time::Duration::from_secs(15)).unwrap();
        let finished = matches!(&event, backend::Event::Finished { .. });
        events.push(event);
        if finished {
            break;
        }
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, backend::Event::Tool(true, _)))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e,backend::Event::Delta(s) if s.contains("verified")))
    );
    let backend::Event::Finished { answer, transcript } = events.pop().unwrap() else {
        panic!("completion expected")
    };
    assert!(answer.unwrap().unwrap().contains("verified"));
    assert!(transcript.iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .unwrap_or("")
                .contains("a verified fixture")
    }));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "shell")
    );
    let mut app = App::new(options.clone());
    app.id = "fixture-session".into();
    app.transcript = transcript;
    app.messages.push(super::app::Message::new(
        "assistant",
        "I read a verified fixture.",
    ));
    app.usage = log;
    app.save().unwrap();
    let saved = backend::session_path(&options.state, &app.id).unwrap();
    assert_eq!(
        std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut restored = App::new(options);
    restored.resume("fixture-session").unwrap();
    assert_ne!(restored.id, "fixture-session");
    assert_eq!(restored.tokens().0, 240);
    assert_eq!(restored.transcript.len(), 5);
    restored.options.root = dir.join("another-workspace");
    assert!(restored.resume("fixture-session").is_err());
    std::fs::remove_dir_all(dir).unwrap();
}
