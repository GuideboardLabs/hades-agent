//! Native terminal client. Rendering never performs network or model work.
mod app;
mod ask;
mod backend;
mod editor;
mod mascot;
mod quota;
mod ui;

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub const HELP: &str = "Hades terminal workspace

  hades                         Open the terminal workspace
  hades tui | hades chat        Explicit terminal entry point
  hades tui --demo              Offline interactive demonstration; no provider calls
  hades tui --root PATH         Working directory (otherwise config, then current directory)
  hades tui --state PATH        Runtime state directory
  hades tui --config PATH       Hades config.json
  hades tui --model PRESET      Model preset for this session
  hades tui --model NAME --endpoint URL [--key ENV_NAME] [--temperature T]
                               Reach a local or custom model; T pins sampling for the session
  hades tui --resume ID         Resume a saved terminal conversation
  hades tui --no-motion         Disable the animated blue flame
  hades tui --no-mouse          Preserve normal terminal mouse selection
  hades tui --snapshot FILE     Render an offline demo as JSON cells, without a terminal
      [--width 140] [--height 52] [--frame 0]

Enter send / activate   Alt+Enter or Ctrl+J newline   Ctrl+P command palette
Tab / Shift+Tab view    PgUp / PgDn or mouse wheel scroll   Ctrl+F find
Ctrl+N new conversation Ctrl+E export   Ctrl+B inspector   Ctrl+T tool details
F1 help   F2 model picker   F3 sessions   F4 approvals   F5 refresh
Esc cancel current turn / close overlay   Ctrl+C cancel or quit   Ctrl+Q quit

Slash commands: /help /new /model /mode read|plan|act /sessions /resume ID /jobs
/approvals /activity /skills /usage /run GOAL /stop /export /clear /motion /quit

Account usage is fetched from the configured provider when supported. Session tokens
come from provider responses; an unavailable quota is never shown as zero usage.
";

/// Restores terminal state on ordinary errors and unwinding.
struct Screen;
impl Screen {
    fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        if mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        Ok(guard)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn run(args: &[String]) -> Result<(), String> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print!("{HELP}");
        return Ok(());
    }
    let options = backend::Options::parse(args)?;
    if let Some(path) = options.snapshot.clone() {
        let mut app = app::App::new(options.clone());
        return ui::snapshot(
            &mut app,
            &path,
            options.width,
            options.height,
            options.frame,
        );
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("the TUI needs an interactive terminal; use hades run for scripts or tui --snapshot FILE for an offline preview".into());
    }
    let mut app = app::App::new(options.clone());
    if !options.demo {
        app.restore_initial()?;
    }
    // Panic diagnostics are printed after the terminal has been restored.
    let previous_hook = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen,
                crossterm::cursor::Show
            );
            eprintln!("{info}");
        }
    }));
    let result = (|| -> io::Result<()> {
        let _screen = Screen::enter(options.mouse)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        terminal.clear()?;
        app.refresh();
        let animation_started = Instant::now();
        let mut refreshed = Instant::now();
        while !app.quit {
            app.poll();
            let frame = options
                .frame
                .wrapping_add((animation_started.elapsed().as_millis() / 42) as u64);
            terminal.draw(|f| ui::draw(f, &mut app, frame))?;
            if event::poll(Duration::from_millis(if app.motion { 42 } else { 90 }))? {
                app.event(event::read()?);
            }
            if refreshed.elapsed() > Duration::from_secs(5) {
                if app.busy
                    && let Err(error) = app.save()
                {
                    app.status = format!("Could not checkpoint the turn: {error}");
                }
                app.refresh();
                refreshed = Instant::now();
            }
        }
        terminal.show_cursor()?;
        Ok(())
    })();
    app.stop();
    if let Err(error) = app.save() {
        app.exit_error = Some(error);
    }
    std::panic::set_hook(previous_hook);
    result.map_err(|e| e.to_string())?;
    if let Some(error) = app.exit_error {
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
