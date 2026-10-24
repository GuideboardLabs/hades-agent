//! The terminal is a projection of App: no I/O, credentials, or execution in draw().
use super::{
    app::{App, Message, Overlay, View},
    ask::Focus,
    backend,
};
use ratatui::style::Stylize;
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Gauge, List, ListItem, ListState, Paragraph, Widget, Wrap},
};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const BG: Color = Color::Rgb(20, 29, 40);
const PANEL: Color = Color::Rgb(24, 38, 52);
const TILE: Color = Color::Rgb(29, 73, 100);
const SKY: Color = Color::Rgb(101, 202, 244);
const USER_GREY: Color = Color::Rgb(139, 151, 166);
const HADES_BLUE: Color = Color::Rgb(43, 104, 148);
const TEXT: Color = Color::Rgb(199, 231, 247);
const MUTED: Color = Color::Rgb(124, 159, 182);
const GREEN: Color = Color::Rgb(142, 218, 174);
const YELLOW: Color = Color::Rgb(244, 209, 132);
const RED: Color = Color::Rgb(255, 132, 148);

fn style(color: Color) -> Style {
    Style::default().fg(color).bg(BG)
}
fn span(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(backend::clean(&text.into()), Style::default().fg(color))
}
fn caption(title: &str, hint: &str, width: usize, color: Color) -> Line<'static> {
    let ink = if color == HADES_BLUE { TEXT } else { BG };
    let style = Style::default()
        .fg(ink)
        .bg(color)
        .add_modifier(Modifier::BOLD);
    if width < 4 {
        return Line::from(Span::styled(" ".repeat(width), style));
    }
    let clean = |text: &str| backend::clean(text).replace(['\n', '\t'], " ");
    let hint = clip(&clean(hint), width.saturating_sub(4) / 2);
    let title = clip(&clean(title), width.saturating_sub(hint.width() + 4));
    let gap = width.saturating_sub(title.width() + hint.width() + 4);
    Line::from(Span::styled(
        format!("  {title}{}{hint}  ", " ".repeat(gap)),
        style,
    ))
}
fn bottom_edge(width: usize, color: Color) -> Line<'static> {
    let text = if width < 2 {
        "▄".repeat(width)
    } else {
        format!("▙{}▟", "▄".repeat(width - 2))
    };
    Line::from(Span::styled(text, style(color)))
}

struct FrameBox {
    title: String,
    hint: String,
    color: Color,
}
impl FrameBox {
    fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }
    fn hint(mut self, hint: &str) -> Self {
        self.hint = hint.into();
        self
    }
}
impl Widget for FrameBox {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        Block::default().style(style(TEXT)).render(area, buffer);
        buffer.set_line(
            area.x,
            area.y,
            &caption(&self.title, &self.hint, area.width as usize, self.color),
            area.width,
        );
        if area.height < 2 {
            return;
        }
        for y in area.y + 1..area.bottom() - 1 {
            // The filled halves face outward; the empty halves face the content.
            buffer[(area.x, y)]
                .set_symbol("▌")
                .set_style(style(self.color));
            if area.width > 1 {
                buffer[(area.right() - 1, y)]
                    .set_symbol("▐")
                    .set_style(style(self.color));
            }
        }
        buffer.set_line(
            area.x,
            area.bottom() - 1,
            &bottom_edge(area.width as usize, self.color),
            area.width,
        );
    }
}
fn block(title: impl Into<String>) -> FrameBox {
    FrameBox {
        title: title.into(),
        hint: String::new(),
        color: SKY,
    }
}
fn inset(area: Rect, x: u16, y: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(x),
        area.y.saturating_add(y),
        area.width.saturating_sub(x * 2),
        area.height.saturating_sub(y * 2),
    )
}
fn paragraph(f: &mut Frame, text: impl Into<String>, rect: Rect, color: Color) {
    f.render_widget(
        Paragraph::new(backend::clean(&text.into()))
            .style(Style::default().fg(color))
            .wrap(Wrap { trim: false }),
        rect,
    );
}
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    let mut out = String::new();
    let mut columns = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if columns + w + 1 > width {
            break;
        }
        out.push(ch);
        columns += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

fn path_lines(path: &Path, width: usize) -> Vec<Line<'static>> {
    let text = backend::clean(&path.display().to_string());
    let mut lines = Vec::new();
    let mut line = String::new();
    for part in text.split_inclusive(['/', '\\']) {
        if !line.is_empty() && line.width() + part.width() > width {
            lines.extend(wrap(vec![span(std::mem::take(&mut line), TEXT)], width));
        }
        line.push_str(part);
    }
    if !line.is_empty() {
        lines.extend(wrap(vec![span(line, TEXT)], width));
    }
    lines
}

fn header(f: &mut Frame, app: &App, area: Rect, frame: u64) {
    f.render_widget(Block::default().style(Style::default().bg(PANEL)), area);
    if area.height < 12 {
        let line = Line::from(vec![
            span(" HADES AGENT  ", SKY).add_modifier(Modifier::BOLD),
            span(
                format!("v{}  ·  {}", env!("CARGO_PKG_VERSION"), app.update),
                MUTED,
            ),
        ]);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y, area.width, 1),
        );
        paragraph(
            f,
            format!(" {}", app.options.root.display()),
            Rect::new(area.x, area.y + 1, area.width, 1),
            TEXT,
        );
        paragraph(
            f,
            format!(" {}", app.title),
            Rect::new(area.x, area.y + 2, area.width, 1),
            TEXT,
        );
    } else {
        let size = area.height.saturating_sub(3) * 2;
        let tile = Rect::new(
            area.x + area.width.saturating_sub(size) / 2,
            area.y,
            size,
            size / 2,
        );
        f.render_widget(
            super::mascot::Mascot {
                frame,
                motion: app.motion,
            },
            tile,
        );
        let side_width = tile.x.saturating_sub(area.x + 3);
        let text_y = area.y + tile.height.saturating_sub(8) / 2;
        let left = Rect::new(area.x + 2, text_y, side_width.saturating_sub(2), 10);
        let right = Rect::new(
            tile.right() + 3,
            text_y,
            area.right().saturating_sub(tile.right() + 5),
            10,
        );
        let mut lines = vec![
            Line::from(span("HADES AGENT", SKY).add_modifier(Modifier::BOLD)),
            Line::default(),
        ];
        lines.extend(path_lines(&app.options.root, left.width as usize));
        lines.extend([
            Line::default(),
            Line::from(span(
                format!(
                    "v{}  /  {}",
                    env!("CARGO_PKG_VERSION"),
                    if app.options.demo {
                        "OFFLINE DEMO"
                    } else {
                        "TERMINAL WORKSPACE"
                    }
                ),
                MUTED,
            )),
            Line::from(span(
                app.update.clone(),
                if app.update.contains("differs") {
                    YELLOW
                } else {
                    MUTED
                },
            )),
        ]);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), left);
        let mut lines = vec![
            Line::from(
                span(
                    if app.busy {
                        "CURRENT WORK"
                    } else {
                        "WORK BRIEF"
                    },
                    SKY,
                )
                .add_modifier(Modifier::BOLD),
            ),
            Line::default(),
        ];
        lines.extend(wrap(
            vec![span(app.title.clone(), TEXT)],
            right.width as usize,
        ));
        lines.push(Line::default());
        lines.push(Line::from(vec![
            span(
                if app.busy { "● WORKING" } else { "● READY" },
                if app.busy { YELLOW } else { GREEN },
            ),
            span(
                if app.overview.daemon {
                    "   daemon online"
                } else {
                    "   daemon offline"
                },
                MUTED,
            ),
        ]));
        f.render_widget(Paragraph::new(lines), right);
    }
    let row = area.bottom().saturating_sub(2);
    f.render_widget(
        Paragraph::new("▄".repeat(area.width as usize)).style(Style::default().fg(SKY).bg(PANEL)),
        Rect::new(area.x, row.saturating_sub(1), area.width, 1),
    );
    let mut spans = vec![span(format!(" {}  ", app.quota.provider), SKY)];
    for window in &app.quota.windows {
        let count = (window.fraction() * 10.0).round() as usize;
        let color = if window.fraction() >= 0.9 {
            RED
        } else if window.fraction() >= 0.7 {
            YELLOW
        } else {
            GREEN
        };
        spans.extend([
            span(format!("{} ", window.description()), TEXT),
            span("━".repeat(count), color),
            span("━".repeat(10 - count), MUTED),
            span("   ", TEXT),
        ]);
    }
    if app.quota.windows.is_empty() {
        spans.push(span(
            app.quota.note.clone(),
            if app.quota.available { MUTED } else { YELLOW },
        ));
    } else if app.options.demo {
        spans.push(span("DEMO DATA", YELLOW));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, row, area.width, 1),
    );
    let (input, output, _) = app.tokens();
    let tokens = if input + output > 0 {
        format!("{} in · {} out", number(input), number(output))
    } else {
        "not reported yet".into()
    };
    paragraph(
        f,
        format!(
            " Session tokens: {tokens}  ·  {}  ·  tool mode: {}",
            app.options.model,
            app.mode.name()
        ),
        Rect::new(area.x, row + 1, area.width, 1),
        MUTED,
    );
}

pub fn draw(f: &mut Frame, app: &mut App, frame: u64) {
    let area = f.area();
    f.render_widget(Block::default().style(style(TEXT)), area);
    if area.width < 56 || area.height < 18 {
        paragraph(
            f,
            "HADES AGENT\n\nEnlarge the terminal to at least 56 × 18.\n\nCtrl+Q saves and closes.",
            inset(area, 1, 1),
            SKY,
        );
        return;
    }
    let banner = if area.height >= 48 && area.width >= 128 {
        19
    } else if area.height >= 40 && area.width >= 96 {
        15
    } else {
        6
    };
    let ask_rows = if app.view == View::Chat {
        app.ask.rows()
    } else {
        0
    };
    let composer_base = if app.view == View::Chat {
        (app.input.position(area.width.saturating_sub(6) as usize).0 as u16 + 3).clamp(3, 8)
    } else {
        0
    };
    // The banner, tabs, a three-row transcript, two spacers and the footer are fixed; the question may
    // only use what is left, so a short terminal clips the bar rather than pushing the input off screen.
    let max_composer = area
        .height
        .saturating_sub(if ask_rows > 0 { 9 } else { 11 });
    let ask_rows = ask_rows.min(max_composer.saturating_sub(composer_base));
    let composer = composer_base + ask_rows;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner),
            Constraint::Length(3),
            Constraint::Min(if ask_rows > 0 { 1 } else { 3 }),
            Constraint::Length(u16::from(app.view == View::Chat)),
            Constraint::Length(composer),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(area);
    header(f, app, layout[0], frame);
    tabs(f, app, layout[1]);
    app.content_area = layout[2];
    let mut footer = layout[6];
    match app.view {
        View::Chat => {
            let sidebar = app.inspector && area.width >= 120;
            let parts = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Min(50),
                    Constraint::Length(if sidebar { 2 } else { 0 }),
                    Constraint::Length(if sidebar { 30 } else { 0 }),
                ])
                .split(inset(layout[2], 1, 0));
            let chat_height = conversation(f, app, parts[0]);
            let sidebar_height = if sidebar {
                inspector(f, app, parts[2], frame)
            } else {
                0
            };
            // Short conversations flow straight into the input; a full transcript
            // uses all available height and keeps the input at the bottom. A question takes the
            // bottom-anchored slot, so the option rows grow upward and the input stays put.
            let composer_area = if ask_rows > 0 {
                layout[4]
            } else {
                Rect {
                    y: layout[2].y + chat_height.max(sidebar_height) + 1,
                    ..layout[4]
                }
            };
            composer_widget(f, app, composer_area);
            footer.y = composer_area.bottom() + 1;
        }
        View::Usage => usage(f, app, layout[2]),
        _ => rows(f, app, layout[2]),
    }
    let notice = if !app.overview.errors.is_empty() {
        format!("{} · {}", app.status, app.overview.errors.join(" · "))
    } else {
        app.status.clone()
    };
    paragraph(
        f,
        format!(" {}", backend::clean(&notice)),
        Rect::new(footer.x, footer.y, footer.width, 1),
        if app.closing { YELLOW } else { MUTED },
    );
    let keys = if app.view == View::Chat {
        " Enter send   Ctrl+P commands   Tab views   F2 model   Ctrl+T tools   F1 help"
    } else {
        " ↑↓ select   Enter open   Tab views   Esc conversation   F5 refresh   F1 help"
    };
    paragraph(
        f,
        keys,
        Rect::new(footer.x, footer.y + 1, footer.width, 1),
        SKY,
    );
    if app.overlay.is_some() {
        overlay(f, app);
    }
}

fn tabs(f: &mut Frame, app: &mut App, area: Rect) {
    // One clear row on either side of the tabs separates navigation from frames.
    let mut x = area.x + 1;
    app.tab_hits.clear();
    let short = area.width < 100;
    for (index, view) in View::ALL.into_iter().enumerate() {
        let label = if area.width < 70 {
            ["Chat", "Saved", "Jobs", "Ask", "Audit", "Skills", "Usage"][index]
        } else if short {
            [
                "Chat", "Sessions", "Jobs", "Approve", "Activity", "Skills", "Usage",
            ][index]
        } else {
            view.name()
        };
        let label = if view == View::Approvals && !app.overview.approvals.is_empty() {
            format!(" {label} {} ", app.overview.approvals.len())
        } else {
            format!(" {label} ")
        };
        let width = (label.width() as u16).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        let rect = Rect::new(x, area.y + 1, width, 1);
        f.render_widget(
            Paragraph::new(label).style(if view == app.view {
                Style::default().bg(SKY).fg(BG).add_modifier(Modifier::BOLD)
            } else {
                style(MUTED)
            }),
            rect,
        );
        app.tab_hits.push((rect, view));
        x += width + 1;
    }
    app.tabs_area = area;
}

pub fn number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Wrap styled text by terminal columns, preserving indentation and wide Unicode.
pub fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut columns = 0;
    for source in spans {
        let mut run = String::new();
        for ch in source.content.chars() {
            if ch == '\n' {
                current.push(Span::styled(std::mem::take(&mut run), source.style));
                lines.push(Line::from(std::mem::take(&mut current)));
                columns = 0;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if columns + w > width {
                current.push(Span::styled(std::mem::take(&mut run), source.style));
                lines.push(Line::from(std::mem::take(&mut current)));
                columns = 0;
            }
            run.push(ch);
            columns += w;
        }
        if !run.is_empty() {
            current.push(Span::styled(run, source.style));
        }
    }
    lines.push(Line::from(current));
    lines
}

fn syntax(line: &str, code: bool) -> Vec<Span<'static>> {
    if line.trim_start().starts_with("```") {
        return vec![span(line, MUTED)];
    }
    if !code {
        if line.starts_with("# ") || line.starts_with("## ") {
            return vec![span(line, SKY).add_modifier(Modifier::BOLD)];
        }
        let mut spans = vec![];
        for (i, part) in line.split('`').enumerate() {
            spans.push(span(part, if i % 2 == 1 { YELLOW } else { TEXT }));
        }
        return spans;
    }
    if line.trim_start().starts_with("//") || line.trim_start().starts_with('#') {
        return vec![span(line, MUTED)];
    }
    let mut spans = vec![];
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let flush = |token: &mut String, spans: &mut Vec<Span<'static>>, quoted: bool| {
        if token.is_empty() {
            return;
        }
        let color = if quoted {
            GREEN
        } else if [
            "let", "mut", "fn", "pub", "if", "else", "for", "while", "return", "use", "struct",
            "impl", "match", "async", "await", "const", "def", "class", "import", "from",
        ]
        .contains(&token.as_str())
        {
            SKY
        } else if ["true", "false", "None", "Some", "Ok", "Err"].contains(&token.as_str())
            || token.chars().all(|c| c.is_ascii_digit() || c == '.')
        {
            YELLOW
        } else {
            TEXT
        };
        spans.push(span(std::mem::take(token), color));
    };
    for c in line.chars() {
        if c == '"' && !escaped {
            if quoted {
                token.push(c);
                flush(&mut token, &mut spans, true);
                quoted = false;
            } else {
                flush(&mut token, &mut spans, false);
                token.push(c);
                quoted = true;
            }
        } else if quoted || c.is_alphanumeric() || c == '_' {
            token.push(c);
        } else {
            flush(&mut token, &mut spans, false);
            spans.push(span(
                c.to_string(),
                if "{}[]()?=:+-".contains(c) {
                    YELLOW
                } else {
                    TEXT
                },
            ));
        }
        escaped = c == '\\' && !escaped;
    }
    flush(&mut token, &mut spans, quoted);
    spans
}

fn message_lines(
    message: &Message,
    user: &str,
    width: usize,
    tools: bool,
    search: &str,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let inner = width - 4;
    let name = match message.role.as_str() {
        "user" => user,
        "assistant" => "HADES",
        "tool" => "TOOL",
        "error" => "ERROR",
        _ => "SYSTEM",
    };
    let color = match message.role.as_str() {
        "user" => USER_GREY,
        "assistant" => HADES_BLUE,
        _ => SKY,
    };
    let title = if message.status.is_empty() {
        name.to_string()
    } else {
        format!("{name} · {}", message.status)
    };
    let mut lines = vec![caption(&title, "", width, color)];
    let mut code = false;
    let text = if message.role == "tool" && !tools {
        message.text.lines().next().unwrap_or("").to_string()
    } else if message.role == "tool" && !message.detail.is_empty() {
        format!("{}\n\n{}", message.text, message.detail)
    } else {
        message.text.clone()
    };
    for source in backend::clean(&text).replace('\t', "    ").split('\n') {
        let highlighted =
            if !search.is_empty() && source.to_lowercase().contains(&search.to_lowercase()) {
                vec![Span::styled(
                    source.to_string(),
                    Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
                )]
            } else if message.role == "error" {
                vec![span(source, RED)]
            } else {
                syntax(source, code)
            };
        if source.trim_start().starts_with("```") {
            code = !code;
        }
        for line in wrap(highlighted, inner) {
            let pad = inner.saturating_sub(line.width());
            let mut spans = vec![span("▌ ", color)];
            spans.extend(line.spans);
            spans.push(span(format!("{} ▐", " ".repeat(pad)), color));
            lines.push(Line::from(spans));
        }
    }
    lines.push(bottom_edge(width, color));
    lines.push(Line::default());
    lines
}

pub struct ChatCache {
    key: (u16, bool, String, usize, usize),
    lines: Vec<Line<'static>>,
}
fn conversation(f: &mut Frame, app: &mut App, area: Rect) -> u16 {
    if app.messages.len() <= app.visible_from {
        let welcome = vec![
            Line::default(),
            Line::from(
                span("One lead agent. A clear task. A visible trail.", SKY)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::default(),
            Line::from(span("Tell Hades what to get done below.", TEXT)),
            Line::default(),
            Line::from(vec![
                span("/run ", YELLOW),
                span("submit a checked run through the daemon", MUTED),
            ]),
            Line::from(vec![
                span("F2   ", YELLOW),
                span("choose a configured model preset", MUTED),
            ]),
            Line::from(vec![
                span("F4   ", YELLOW),
                span("review decisions that need your approval", MUTED),
            ]),
            Line::default(),
            Line::from(span(
                "Ctrl+P opens all commands. F1 shows keyboard shortcuts.",
                MUTED,
            )),
        ];
        f.render_widget(
            Paragraph::new(welcome).wrap(Wrap { trim: false }),
            inset(area, 2, 1),
        );
        app.max_scroll = 0;
        return area.height.min(12);
    }
    let key = (
        area.width,
        app.show_tools,
        app.search.clone(),
        app.visible_from,
        app.messages.len(),
    );
    if app.chat_cache.as_ref().is_none_or(|cache| cache.key != key) {
        let lines = app
            .messages
            .iter()
            .skip(app.visible_from)
            .flat_map(|m| {
                message_lines(
                    m,
                    &app.options.user,
                    area.width as usize,
                    app.show_tools,
                    &app.search,
                )
            })
            .collect();
        app.chat_cache = Some(ChatCache { key, lines });
    }
    let lines = &app.chat_cache.as_ref().expect("cache").lines;
    app.max_scroll = lines.len().saturating_sub(area.height as usize);
    if app.follow {
        app.scroll = app.max_scroll;
    } else {
        app.scroll = app.scroll.min(app.max_scroll);
    }
    // Slice instead of Paragraph::scroll's u16 offset so long conversations remain navigable.
    let visible: Vec<_> = lines
        .iter()
        .skip(app.scroll)
        .take(area.height as usize)
        .cloned()
        .collect();
    // Every message carries a trailing separator. Exclude the final separator
    // from the occupied height so the layout supplies precisely one gutter.
    let height = lines.len().saturating_sub(app.scroll);
    let height = if height <= area.height as usize {
        height.saturating_sub(1) as u16
    } else {
        area.height
    };
    f.render_widget(Paragraph::new(visible).style(style(TEXT)), area);
    height
}

fn inspector(f: &mut Frame, app: &App, area: Rect, frame: u64) -> u16 {
    let (input, output, calls) = app.tokens();
    let elapsed = app.turn_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let mut lines = vec![
        Line::from(span(
            if app.busy {
                format!(
                    "{}  working · {elapsed}s",
                    ["◐", "◓", "◑", "◒"][(frame / 2 % 4) as usize]
                )
            } else {
                "●  ready".into()
            },
            if app.busy { YELLOW } else { GREEN },
        )),
        Line::default(),
        Line::from(span("MODEL", MUTED)),
        Line::from(span(app.options.model.clone(), TEXT)),
        Line::from(span(app.options.provider(), MUTED)),
        Line::default(),
        Line::from(span("EXECUTION", MUTED)),
        Line::from(span(
            if app.governed.is_some() {
                "daemon / checked run".into()
            } else {
                format!("direct chat / {}", app.mode.name())
            },
            TEXT,
        )),
        Line::from(span(
            format!("{} pending approvals", app.overview.approvals.len()),
            if app.overview.approvals.is_empty() {
                MUTED
            } else {
                YELLOW
            },
        )),
        Line::default(),
        Line::from(span("THIS CONVERSATION", MUTED)),
        Line::from(span(
            format!("{} in / {} out", number(input), number(output)),
            TEXT,
        )),
        Line::from(span(format!("{calls} usage-bearing responses"), MUTED)),
        Line::from(span(format!("{} messages", app.messages.len()), MUTED)),
    ];
    if app.busy && !app.current_tool.is_empty() {
        lines.extend([
            Line::default(),
            Line::from(span("ACTIVE TOOL", MUTED)),
            Line::from(span(app.current_tool.clone(), GREEN)),
        ]);
    }
    if !app.reasoning.is_empty() {
        lines.extend([
            Line::default(),
            Line::from(span("LATEST REASONING", MUTED)),
            Line::from(span(
                app.reasoning
                    .chars()
                    .rev()
                    .take(280)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>(),
                MUTED,
            )),
        ]);
    }
    let content_width = area.width.saturating_sub(4) as usize;
    let lines: Vec<_> = lines
        .into_iter()
        .flat_map(|line| wrap(line.spans, content_width))
        .collect();
    let area = Rect {
        height: area.height.min((lines.len() + 2) as u16),
        ..area
    };
    f.render_widget(block("WORKSPACE"), area);
    f.render_widget(Paragraph::new(lines), inset(area, 2, 1));
    area.height
}

fn composer_widget(f: &mut Frame, app: &App, area: Rect) {
    let ask = app.ask.current().and_then(|_| app.ask.ask());
    // Never claim more rows than the composer actually got.
    let rows = app.ask.rows().min(area.height.saturating_sub(3));
    let title = match &ask {
        Some(_) if app.ask.collapsed => format!(
            "HADES ASKS · {} question{} · Enter to open",
            app.ask.questions.len(),
            if app.ask.questions.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
        Some(ask) => format!(
            "HADES ASKS · {} of {} · {}",
            app.ask.active + 1,
            app.ask.questions.len(),
            clip(&ask.prompt, area.width.saturating_sub(34) as usize)
        ),
        None => format!(
            "{}  /  {}",
            app.options.user,
            if app.busy {
                "draft while Hades works"
            } else {
                "message"
            }
        ),
    };
    f.render_widget(
        block(title).color(if ask.is_some() { SKY } else { USER_GREY }),
        inset(area, 1, 0),
    );
    if let Some(ask) = &ask {
        let bar = Rect {
            x: area.x + 3,
            y: area.y + 1,
            width: area.width.saturating_sub(6),
            height: rows,
        };
        ask_bar(f, app, ask, bar);
    }
    let inner = Rect {
        x: area.x + 3,
        y: area.y + 1 + rows,
        width: area.width.saturating_sub(6),
        height: area.height.saturating_sub(2 + rows),
    };
    let (row, col) = app.input.position(inner.width as usize);
    let offset = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    if app.input.text.is_empty() {
        paragraph(
            f,
            if ask.is_some() {
                "Type your own answer, or a reason to decline"
            } else {
                "What should Hades get done?"
            },
            inner,
            MUTED,
        );
    } else {
        let lines = wrap(
            vec![span(
                app.input.text.clone(),
                if app.input.text.starts_with('/') {
                    YELLOW
                } else {
                    TEXT
                },
            )],
            inner.width as usize,
        );
        f.render_widget(
            Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
            inner,
        );
    }
    if app.overlay.is_none() && inner.width > 0 && inner.height > 0 {
        f.set_cursor_position((
            inner.x + col.min(inner.width as usize - 1) as u16,
            inner.y + (row - offset).min(inner.height as usize - 1) as u16,
        ));
    }
}

/// The question anchored to the top edge of the input: a reason line, the options with the recommended
/// one first, and an `Other` row that is the automatic decline of everything above it.
fn ask_bar(f: &mut Frame, app: &App, ask: &crate::question::Ask, area: Rect) {
    if area.height == 0 {
        return;
    }
    if app.ask.collapsed {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                span("● ", RED),
                span(
                    format!(
                        "{} question{} waiting",
                        app.ask.questions.len(),
                        if app.ask.questions.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    YELLOW,
                ),
                span(" · Enter opens it · Esc stays collapsed", MUTED),
            ])),
            area,
        );
        return;
    }
    f.render_widget(
        Paragraph::new(Line::from(vec![
            span(
                if ask.blocking {
                    "NEEDS AN ANSWER · "
                } else {
                    "ADVISORY · "
                },
                if ask.blocking { RED } else { YELLOW },
            ),
            span(
                clip(&ask.why, area.width.saturating_sub(18) as usize),
                MUTED,
            ),
        ])),
        Rect { height: 1, ..area },
    );
    for (index, choice) in ask.options.iter().enumerate() {
        if 1 + index as u16 >= area.height {
            break;
        }
        let selected = app.ask.focus == Some(Focus::Options) && app.ask.option == index;
        let mut line = vec![
            span(
                if selected { "▸ " } else { "  " },
                if selected { SKY } else { MUTED },
            ),
            span(
                format!("{}. ", index + 1),
                if selected { SKY } else { MUTED },
            ),
            span(choice.label.clone(), TEXT),
        ];
        if !choice.detail.is_empty() {
            line.push(span(
                format!(
                    "  {}",
                    clip(&choice.detail, area.width.saturating_sub(26) as usize)
                ),
                MUTED,
            ));
        }
        if choice.recommended {
            line.push(span("  recommended", GREEN));
        }
        f.render_widget(
            Paragraph::new(Line::from(line)).style(if selected {
                Style::default().bg(TILE)
            } else {
                Style::default()
            }),
            Rect {
                x: area.x,
                y: area.y + 1 + index as u16,
                width: area.width,
                height: 1,
            },
        );
    }
    let selected = app.ask.focus == Some(Focus::Input);
    if (1 + ask.options.len() as u16) < area.height {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                span(
                    if selected { "▸ " } else { "  " },
                    if selected { SKY } else { MUTED },
                ),
                span("Other…", if selected { TEXT } else { MUTED }),
                span("  type an answer, or a reason to decline", MUTED),
            ]))
            .style(if selected {
                Style::default().bg(TILE)
            } else {
                Style::default()
            }),
            Rect {
                x: area.x,
                y: area.y + 1 + ask.options.len() as u16,
                width: area.width,
                height: 1,
            },
        );
    }
    if app.ask.questions.len() > 1 && area.height as usize > ask.options.len() + 2 {
        paragraph(
            f,
            format!("{} more waiting · Tab cycles", app.ask.questions.len() - 1),
            Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            },
            MUTED,
        );
    }
}

fn rows(f: &mut Frame, app: &App, area: Rect) {
    let pane = block(format!("{} · {}", app.view.name(), app.rows().len()));
    f.render_widget(pane, inset(area, 1, 0));
    let area = inset(area, 3, 1);
    if app.rows().is_empty() {
        let note = match app.view {
            View::Sessions => {
                "No saved conversations yet. Completed turns are saved automatically."
            }
            View::Jobs => "No scheduled jobs. Use Hades' existing cron commands to create one.",
            View::Approvals => "No pending approvals. A checked run may ask for a decision here.",
            View::Activity => "No recorded actions yet.",
            View::Skills => {
                "No installed SKILL.md procedures found in this workspace or Hades home."
            }
            _ => "No items.",
        };
        paragraph(f, format!("\n{note}"), area, MUTED);
        return;
    }
    let parts = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);
    let items: Vec<_> = app
        .rows()
        .iter()
        .map(|row| {
            ListItem::new(vec![
                Line::from(span(
                    clip(&row.title, parts[0].width.saturating_sub(3) as usize),
                    TEXT,
                )),
                Line::from(span(
                    clip(&row.status, parts[0].width.saturating_sub(3) as usize),
                    MUTED,
                )),
                Line::default(),
            ])
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().bg(TILE).fg(SKY))
            .highlight_symbol("▸ "),
        parts[0],
        &mut state,
    );
    if let Some(row) = app.rows().get(app.selected) {
        f.render_widget(
            Paragraph::new(vec![Line::from(span("▌", SKY)); parts[1].height as usize]),
            parts[1],
        );
        let hint = match app.view {
            View::Sessions => "Enter resumes as a new branch",
            View::Approvals => "Enter reviews this decision",
            View::Jobs => "Enter details · d cancel job",
            _ => "Enter opens full details",
        };
        let text = format!(
            "{}\n{}\n\n{}\n\n{}",
            row.title,
            row.status,
            backend::clean(&row.detail),
            hint
        );
        f.render_widget(
            Paragraph::new(backend::clean(&text))
                .wrap(Wrap { trim: false })
                .style(style(TEXT)),
            inset(parts[1], 2, 0),
        );
    }
}

fn usage(f: &mut Frame, app: &App, area: Rect) {
    f.render_widget(block("PROVIDER ACCOUNT & SESSION"), inset(area, 1, 0));
    let inner = inset(area, 3, 1);
    let mut y = inner.y;
    paragraph(
        f,
        format!("{} · {}", app.quota.provider, app.options.model),
        Rect::new(inner.x, y, inner.width, 1),
        SKY,
    );
    y += 2;
    for window in &app.quota.windows {
        if y + 3 >= inner.bottom() {
            break;
        }
        f.render_widget(
            Gauge::default()
                .ratio(window.fraction())
                .label(window.description())
                .gauge_style(
                    Style::default()
                        .fg(if window.fraction() >= 0.9 { RED } else { SKY })
                        .bg(PANEL),
                ),
            Rect::new(inner.x, y, inner.width, 1),
        );
        if !window.reset.is_empty() {
            paragraph(
                f,
                format!("Resets: {}", window.reset),
                Rect::new(inner.x, y + 1, inner.width, 1),
                MUTED,
            );
        }
        y += 3;
    }
    let (input, output, calls) = app.tokens();
    let text = format!(
        "{}\n\nConversation usage\n{} input tokens   {} output tokens\n{calls} responses with reported usage in this process\n\nAccount limits refresh once a minute. Session totals include usage returned by the provider, including delegated chat calls. Daemon run usage belongs to its run receipt. Missing usage is not estimated as a plan balance.",
        app.quota.note,
        number(input),
        number(output)
    );
    paragraph(
        f,
        text,
        Rect::new(inner.x, y, inner.width, inner.bottom().saturating_sub(y)),
        TEXT,
    );
}

fn overlay(f: &mut Frame, app: &App) {
    let screen = f.area();
    let width = screen.width.saturating_sub(6).min(90);
    let height = screen.height.saturating_sub(4).min(30);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(style(TEXT)), area);
    let Some(overlay) = &app.overlay else {
        return;
    };
    let title = match overlay {
        Overlay::Help => "HELP",
        Overlay::Palette => "COMMAND PALETTE",
        Overlay::Models => "MODELS",
        Overlay::Find => "FIND IN CONVERSATION",
        Overlay::Detail(title, _) => title,
        Overlay::Approval(_) => "REVIEW APPROVAL",
        Overlay::CancelJob(_) => "CANCEL SCHEDULED JOB",
    };
    f.render_widget(block(title).hint("Esc close"), area);
    let inner = inset(area, 2, 1);
    match overlay {
        Overlay::Palette | Overlay::Models => {
            paragraph(
                f,
                format!("> {}", app.query.text),
                Rect::new(inner.x, inner.y, inner.width, 1),
                SKY,
            );
            let items: Vec<_> = app
                .palette_items()
                .into_iter()
                .map(|(name, desc)| {
                    ListItem::new(vec![
                        Line::from(span(name, SKY)),
                        Line::from(span(desc, MUTED)),
                    ])
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(app.selected));
            f.render_stateful_widget(
                List::new(items)
                    .highlight_style(Style::default().bg(TILE))
                    .highlight_symbol("▸ "),
                Rect::new(
                    inner.x,
                    inner.y + 2,
                    inner.width,
                    inner.height.saturating_sub(3),
                ),
                &mut state,
            );
            f.set_cursor_position((
                inner.x
                    + 2
                    + (app.query.text[..app.query.cursor].width() as u16)
                        .min(inner.width.saturating_sub(3)),
                inner.y,
            ));
        }
        Overlay::Find => {
            paragraph(
                f,
                format!(
                    "> {}\n\nEnter highlights matching lines. Esc clears the search in the conversation.\nUse Page Up / Page Down to navigate.",
                    app.query.text
                ),
                inner,
                TEXT,
            );
            f.set_cursor_position((
                inner.x
                    + 2
                    + (app.query.text[..app.query.cursor].width() as u16)
                        .min(inner.width.saturating_sub(3)),
                inner.y,
            ));
        }
        Overlay::Help => {
            f.render_widget(
                Paragraph::new(super::HELP)
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                inner,
            );
        }
        Overlay::Detail(_, body) => {
            f.render_widget(
                Paragraph::new(backend::clean(body))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                inner,
            );
        }
        Overlay::Approval(row) => {
            let parts = Layout::default()
                .constraints([Constraint::Min(1), Constraint::Length(3)])
                .split(inner);
            f.render_widget(
                Paragraph::new(format!("{}\n\n{}", row.title, backend::clean(&row.detail)))
                    .wrap(Wrap { trim: false })
                    .scroll((app.overlay_scroll, 0)),
                parts[0],
            );
            paragraph(
                f,
                if app.options.demo {
                    "DEMO · decisions are disabled"
                } else {
                    "y  allow once     s  allow for session     n  deny\nPgUp / PgDn scroll the request. Decisions are sent to the daemon."
                },
                parts[1],
                YELLOW,
            );
        }
        Overlay::CancelJob(row) => paragraph(
            f,
            format!(
                "Cancel this scheduled job?\n\n{}\n\n{}\n\ny  cancel job      Esc  keep job",
                row.title, row.status
            ),
            inner,
            YELLOW,
        ),
    }
}

pub fn snapshot(
    app: &mut App,
    path: &Path,
    width: u16,
    height: u16,
    frame: u64,
) -> Result<(), String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).map_err(|e| e.to_string())?;
    terminal
        .draw(|f| draw(f, app, frame))
        .map_err(|e| e.to_string())?;
    let buffer = terminal.backend().buffer();
    let color = |c: Color| match c {
        Color::Rgb(r, g, b) => vec![r, g, b],
        _ => vec![199, 231, 247],
    };
    let cells:Vec<_>=buffer.content.iter().map(|c|serde_json::json!({"s":c.symbol(),"fg":color(c.fg),"bg":color(c.bg),"bold":c.modifier.contains(Modifier::BOLD)})).collect();
    let value = serde_json::json!({"width":width,"height":height,"cells":cells});
    std::fs::write(path, serde_json::to_vec(&value).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
