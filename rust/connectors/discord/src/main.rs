//! Discord, as a surface rather than a second runtime.
//!
//! This binary is a connector pack: it depends on `harness`, implements `Transport`, and hands the
//! gateway the same state directory and config file the daemon uses. Everything below the transport
//! (sessions, policy, models, tools, approvals) belongs to Hades; everything above it (auth, the
//! websocket, rate limits, components) belongs here.
//!
//! The token comes from `HADES_CONNECTOR_DISCORD_TOKEN`, resolved by the harness (environment, then
//! `.env`). It is never printed and never put in a message.

mod transport;

use std::path::PathBuf;

use harness::gateway::{Loading, Runtime, settings_for, token_env};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(said) = run(&args) {
        eprintln!("discord: {said}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut state = PathBuf::from(".hades");
    let mut config: Option<PathBuf> = None;
    let mut once = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--state" => {
                index += 1;
                state = PathBuf::from(args.get(index).ok_or("--state needs a directory")?);
            }
            "--config" => {
                index += 1;
                config = Some(PathBuf::from(
                    args.get(index).ok_or("--config needs a file")?,
                ));
            }
            "--once" => once = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
        index += 1;
    }
    let config = config.ok_or("a connector needs the same --config the daemon reads")?;
    let mut settings = settings_for(&config, transport::SURFACE)?;
    // The pack is the place, so the place supplies what it needs said and how it looks while it
    // works. Config wins over both: a `prompt` or `loading` in `gateway.connectors.discord` is the
    // person having the last word, and the pack's defaults never overwrite it.
    if let Some(home) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
    {
        if settings.prompt.is_none() {
            for candidate in [home.join("prompt.md"), home.join("../prompt.md")] {
                if let Ok(text) = std::fs::read_to_string(&candidate)
                    && !text.trim().is_empty()
                {
                    settings.prompt = Some(text.trim().to_string());
                    break;
                }
            }
        }
        if settings.loading.is_none() {
            for candidate in [home.join("connector.json"), home.join("../connector.json")] {
                if let Ok(text) = std::fs::read_to_string(&candidate)
                    && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text)
                {
                    settings.loading = manifest.get("loading").map(|loading| Loading {
                        glyph: loading
                            .get("glyph")
                            .and_then(|glyph| glyph.as_str())
                            .unwrap_or("💀")
                            .to_string(),
                        span: loading
                            .get("span")
                            .and_then(|span| span.as_u64())
                            .unwrap_or(5)
                            .clamp(1, 20) as usize,
                        step_ms: loading
                            .get("step_ms")
                            .and_then(|step| step.as_u64())
                            .unwrap_or(240)
                            .clamp(40, 5_000),
                    });
                    break;
                }
            }
        }
    }
    let token = harness::model::secret(&token_env(transport::SURFACE))?;
    // Say what this surface is actually carrying, once, where a person can see it. A pack whose
    // prompt silently failed to load looks exactly like a model ignoring its instructions.
    let opening = match &settings.prompt {
        Some(text) => format!("{} line(s)", text.lines().count()),
        None => "shared prompt only".to_string(),
    };
    let loading = settings.loading.clone().unwrap_or_default();
    eprintln!(
        "discord: opening {opening}; loading {} across {} step(s) of {} ms; model {}",
        loading.glyph, loading.span, loading.step_ms, settings.model
    );
    let socket = harness::daemon::live(&state).ok_or_else(|| {
        format!(
            "no daemon is running under {}; start one with `hades daemon --state {}`",
            state.display(),
            state.display()
        )
    })?;
    let transport = transport::Discord::new(token, &config)?;
    let mut runtime = Runtime::new(
        transport::SURFACE,
        socket,
        state,
        settings,
        Some(transport.clone()),
    )?;
    runtime.run(transport, once)
}
