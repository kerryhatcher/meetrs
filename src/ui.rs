//! Ratatui terminal UI.
//!
//! CONTRACT (do not change):
//!   pub fn run(rx: mpsc::Receiver<Status>, dir: &Path, info: CaptureInfo, stop: &StopFlag) -> Result<()>
//!
//! Owns the terminal: enters raw mode + alternate screen, runs the event loop
//! until the user quits or `stop` is set, and restores the terminal on EVERY
//! exit path including panic.

use crate::chunk::StopFlag;
use crate::types::{CaptureInfo, Status};
use anyhow::Result;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Wrap};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Redraw/poll cadence. Can't reach `main::FRAME` (private), so this is our
/// own copy at the same 50ms/20fps the writer thread targets.
const TICK: Duration = Duration::from_millis(50);

/// Peaks decay to zero over this long so a transient spike stays visible
/// instead of vanishing on the very next frame.
const PEAK_HOLD: Duration = Duration::from_millis(1500);

/// How many closed chunks to remember for the durability list.
const CHUNK_HISTORY: usize = 50;

/// How many transcript lines to keep on screen. Bounded because a long meeting
/// would otherwise grow this without limit — transcript.md is the full record.
const TRANSCRIPT_HISTORY: usize = 200;

const FLOOR_DB: f32 = -60.0;

fn to_dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        FLOOR_DB
    } else {
        (20.0 * rms.log10()).max(FLOOR_DB)
    }
}

/// Maps a dBFS value to a 0..=100 gauge percent against the [FLOOR_DB, 0] range.
fn db_to_percent(db: f32) -> u16 {
    (((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0) * 100.0).round() as u16
}

fn level_color(db: f32) -> Color {
    if db >= -0.5 {
        Color::Red
    } else if db >= -6.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

struct ClosedChunk {
    name: String,
    duration: Duration,
    bytes: u64,
}

/// One decaying peak-hold value.
#[derive(Default, Clone, Copy)]
struct Peak {
    value: f32,
    set_at: Option<Instant>,
}

impl Peak {
    fn update(&mut self, sample: f32, now: Instant) {
        if sample >= self.value {
            self.value = sample;
            self.set_at = Some(now);
        }
    }

    /// Linearly decay the held value to zero over PEAK_HOLD, then reset.
    fn decayed(&mut self, now: Instant) -> f32 {
        match self.set_at {
            Some(t) => {
                let age = now.duration_since(t);
                if age >= PEAK_HOLD {
                    self.value = 0.0;
                    self.set_at = None;
                    0.0
                } else {
                    let frac = 1.0 - age.as_secs_f32() / PEAK_HOLD.as_secs_f32();
                    self.value * frac
                }
            }
            None => 0.0,
        }
    }
}

struct App {
    started: Instant,
    system_rms: f32,
    mic_rms: f32,
    system_peak: Peak,
    mic_peak: Peak,
    in_silence: bool,
    current_index: u32,
    current_started: Instant,
    chunks: VecDeque<ClosedChunk>,
    warnings: u32,
    last_warning: Option<String>,
    finished: bool,
    /// Index of the chunk currently being transcribed, if any.
    transcribing: Option<u32>,
    /// Chunks closed but not yet transcribed. Growing means falling behind.
    backlog: usize,
    transcribed: VecDeque<String>,
    /// Live transcript lines, newest first — same order as every other pane.
    transcript: VecDeque<String>,
}

impl App {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            system_rms: 0.0,
            mic_rms: 0.0,
            system_peak: Peak::default(),
            mic_peak: Peak::default(),
            in_silence: false,
            current_index: 0,
            current_started: now,
            chunks: VecDeque::with_capacity(CHUNK_HISTORY),
            warnings: 0,
            last_warning: None,
            finished: false,
            transcribing: None,
            backlog: 0,
            transcribed: VecDeque::with_capacity(CHUNK_HISTORY),
            transcript: VecDeque::with_capacity(TRANSCRIPT_HISTORY),
        }
    }

    fn apply(&mut self, status: Status, now: Instant) {
        match status {
            Status::Level {
                system_rms,
                system_peak,
                mic_rms,
                mic_peak,
            } => {
                self.system_rms = system_rms;
                self.mic_rms = mic_rms;
                self.system_peak.update(system_peak, now);
                self.mic_peak.update(mic_peak, now);
            }
            Status::ChunkOpened { index, .. } => {
                self.current_index = index;
                self.current_started = now;
            }
            Status::ChunkClosed {
                path,
                duration,
                bytes,
                ..
            } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.chunks.push_front(ClosedChunk {
                    name,
                    duration,
                    bytes,
                });
                while self.chunks.len() > CHUNK_HISTORY {
                    self.chunks.pop_back();
                }
            }
            Status::ChunkDiscarded { .. } => {}
            Status::Silence { in_silence } => self.in_silence = in_silence,
            Status::Overrun { dropped_samples } => {
                self.warnings += 1;
                self.last_warning = Some(format!("OVERRUN: {dropped_samples} samples dropped"));
            }
            Status::Warning(msg) => {
                self.warnings += 1;
                self.last_warning = Some(msg);
            }
            Status::Finished { .. } => self.finished = true,
            Status::TranscribeStarted { index } => self.transcribing = Some(index),
            Status::TranscribeDone {
                index,
                took,
                audio,
                words,
            } => {
                self.transcribing = None;
                // The realtime factor is the number that matters: below 1.0x and
                // transcription can never catch up with recording.
                let xrt = if took.as_secs_f32() > 0.0 {
                    audio.as_secs_f32() / took.as_secs_f32()
                } else {
                    0.0
                };
                self.transcribed.push_front(format!(
                    "chunk-{index:03} {words} words  {xrt:.0}x realtime"
                ));
                while self.transcribed.len() > CHUNK_HISTORY {
                    self.transcribed.pop_back();
                }
            }
            Status::Transcript { lines, .. } => {
                // Newest first, so reverse this chunk's lines as they go in —
                // otherwise a chunk's own segments would read back-to-front.
                for line in lines.into_iter().rev() {
                    self.transcript.push_front(line);
                }
                while self.transcript.len() > TRANSCRIPT_HISTORY {
                    self.transcript.pop_back();
                }
            }
            Status::TranscribeFailed { index, err } => {
                self.transcribing = None;
                self.warnings += 1;
                self.last_warning = Some(format!("transcribe chunk-{index:03} failed: {err}"));
            }
            Status::TranscribeBacklog { pending } => self.backlog = pending,
        }
    }
}

pub fn run(rx: Receiver<Status>, dir: &Path, _info: CaptureInfo, stop: &StopFlag) -> Result<()> {
    // ratatui::init() installs a panic hook that restores the terminal before
    // unwinding, and try_restore() below is idempotent (safe to call even if
    // already restored) — no need to hand-roll either.
    let mut terminal = ratatui::try_init()?;
    let dir_display = dir.display().to_string();
    let mut app = App::new();

    let result = event_loop(&mut terminal, &mut app, &rx, &dir_display, stop);

    ratatui::try_restore()?;
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &Receiver<Status>,
    dir_display: &str,
    stop: &StopFlag,
) -> Result<()> {
    loop {
        let now = Instant::now();

        // Drain every pending status update; never block on the channel.
        loop {
            match rx.try_recv() {
                Ok(status) => app.apply(status, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.finished = true;
                    break;
                }
            }
        }

        terminal.draw(|frame| draw(frame, app, dir_display, now))?;

        if crossterm::event::poll(TICK)? {
            use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
            if let Event::Key(key) = crossterm::event::read()?
                && key.kind == KeyEventKind::Press
                && (matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                stop.stop();
                return Ok(());
            }
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App, dir_display: &str, now: Instant) {
    let area = frame.area();

    // ponytail: below this, don't even try to lay out sub-widgets — a single
    // line is the ceiling for a terminal too small to render meters legibly.
    if area.height < 5 || area.width < 20 {
        frame.render_widget(
            Paragraph::new("meetrs: terminal too small (need >=20x5)"),
            area,
        );
        return;
    }

    let has_warning = app.last_warning.is_some();
    let warning_h: u16 = if has_warning { 2 } else { 0 };

    // Header(1) + meters(4) + status(1) + footer(1) are non-negotiable; the
    // chunk list gets whatever is left, and is dropped first under pressure.
    let fixed = 1 + 4 + 1 + warning_h + 1;
    let list_h = area.height.saturating_sub(fixed);

    let mut constraints = vec![
        Constraint::Length(1), // header
        Constraint::Length(4), // meters
    ];
    if has_warning {
        constraints.push(Constraint::Length(warning_h));
    }
    constraints.push(Constraint::Length(1)); // status line
    if list_h > 0 {
        constraints.push(Constraint::Min(0)); // chunk list
    }
    constraints.push(Constraint::Length(1)); // footer

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;
    draw_header(frame, rows[idx], app, dir_display);
    idx += 1;
    draw_meters(frame, rows[idx], app, now);
    idx += 1;
    if has_warning {
        draw_warning(frame, rows[idx], app);
        idx += 1;
    }
    draw_status(frame, rows[idx], app);
    idx += 1;
    if list_h > 0 {
        draw_bottom(frame, rows[idx], app);
        idx += 1;
    }
    draw_footer(frame, rows[idx]);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, dir_display: &str) {
    let elapsed = app.started.elapsed();
    let mins = elapsed.as_secs() / 60;
    let secs = elapsed.as_secs() % 60;
    let prefix = format!("meetrs · recording · {mins:02}:{secs:02} · ");
    let budget = (area.width as usize).saturating_sub(prefix.len());
    let dir_shown = if dir_display.len() > budget && budget > 1 {
        // Truncate from the left — the tail (session id) is the useful part.
        format!("…{}", &dir_display[dir_display.len() - (budget - 1)..])
    } else {
        dir_display.to_string()
    };
    frame.render_widget(Paragraph::new(format!("{prefix}{dir_shown}")), area);
}

fn draw_meters(frame: &mut Frame, area: Rect, app: &mut App, now: Instant) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let sys_peak_db = to_dbfs(app.system_peak.decayed(now));
    let mic_peak_db = to_dbfs(app.mic_peak.decayed(now));
    render_leg(frame, cols[0], "System", app.system_rms, sys_peak_db);
    render_leg(frame, cols[1], "Mic", app.mic_rms, mic_peak_db);
}

fn render_leg(frame: &mut Frame, area: Rect, label: &str, rms: f32, peak_db: f32) {
    let db = to_dbfs(rms);
    let title = format!("{label} {db:>5.1} dB (peak {peak_db:>5.1})");
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(title))
        .gauge_style(Style::default().fg(level_color(db)))
        .percent(db_to_percent(db))
        .label("");
    frame.render_widget(gauge, area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let listening = if app.in_silence {
        Span::styled("○ paused", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("● listening", Style::default().fg(Color::Green))
    };
    let chunk_elapsed = app.current_started.elapsed();
    let asr = match (app.transcribing, app.backlog) {
        (Some(i), 0) => Span::styled(
            format!("   ⠿ transcribing #{i}"),
            Style::default().fg(Color::Cyan),
        ),
        // A growing backlog is the signal that transcription cannot keep up,
        // so show it in yellow rather than burying it.
        (Some(i), n) => Span::styled(
            format!("   ⠿ transcribing #{i} (+{n} queued)"),
            Style::default().fg(Color::Yellow),
        ),
        (None, 0) => Span::styled("   transcribe idle", Style::default().fg(Color::DarkGray)),
        (None, n) => Span::styled(
            format!("   {n} queued to transcribe"),
            Style::default().fg(Color::Yellow),
        ),
    };
    let line = Line::from(vec![
        listening,
        Span::raw(format!(
            "   chunk #{} · {:>3.0}s",
            app.current_index,
            chunk_elapsed.as_secs_f32()
        )),
        asr,
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_warning(frame: &mut Frame, area: Rect, app: &App) {
    let msg = app.last_warning.as_deref().unwrap_or("");
    let text = format!("⚠ {msg} (x{})", app.warnings);
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Black).bg(Color::Red)),
        area,
    );
}

/// Chunks on the left, live transcript on the right. Below MIN_SPLIT_WIDTH two
/// bordered columns leave too few usable characters for either to be readable,
/// so the transcript takes the whole pane — it's the half worth reading.
const MIN_SPLIT_WIDTH: u16 = 60;

fn draw_bottom(frame: &mut Frame, area: Rect, app: &App) {
    if area.width < MIN_SPLIT_WIDTH {
        draw_transcript(frame, area, app);
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    draw_chunks(frame, cols[0], app);
    draw_transcript(frame, cols[1], app);
}

fn draw_transcript(frame: &mut Frame, area: Rect, app: &App) {
    let text: Vec<Line> = app
        .transcript
        .iter()
        .map(|l| Line::raw(l.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title("transcript (newest first)"),
        ),
        area,
    );
}

fn draw_chunks(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .chunks
        .iter()
        .map(|c| {
            ListItem::new(format!(
                "{}  {:>5.1}s  {:>7} bytes",
                c.name,
                c.duration.as_secs_f32(),
                c.bytes
            ))
        })
        .collect();
    let mut items = items;
    for t in app.transcribed.iter() {
        items.push(ListItem::new(format!("  ↳ {t}")));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("chunks (newest first)"),
    );
    frame.render_widget(list, area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    frame.render_widget(Paragraph::new("q quit"), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(app: &App) -> Vec<&str> {
        app.transcript.iter().map(String::as_str).collect()
    }

    #[test]
    fn transcript_is_newest_first_but_keeps_each_chunk_in_order() {
        let mut app = App::new();
        let now = Instant::now();
        app.apply(
            Status::Transcript {
                index: 0,
                lines: vec!["a1".into(), "a2".into()],
            },
            now,
        );
        app.apply(
            Status::Transcript {
                index: 1,
                lines: vec!["b1".into(), "b2".into()],
            },
            now,
        );
        // Newest chunk on top, and within a chunk the segments stay in the
        // order they were spoken.
        assert_eq!(transcript(&app), vec!["b1", "b2", "a1", "a2"]);
    }

    #[test]
    fn transcript_is_bounded() {
        let mut app = App::new();
        let now = Instant::now();
        for i in 0..TRANSCRIPT_HISTORY + 10 {
            app.apply(
                Status::Transcript {
                    index: i as u32,
                    lines: vec![format!("line {i}")],
                },
                now,
            );
        }
        assert_eq!(app.transcript.len(), TRANSCRIPT_HISTORY);
        // The oldest lines are the ones dropped.
        assert_eq!(app.transcript.front().unwrap(), "line 209");
    }
}
