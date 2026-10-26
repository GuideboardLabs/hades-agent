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

use harness::gateway::{Runtime, settings_for, token_env};

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
    let settings = settings_for(&config, transport::SURFACE)?;
    let token = harness::model::secret(&token_env(transport::SURFACE))?;
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
