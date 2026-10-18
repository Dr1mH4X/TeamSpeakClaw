use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Local;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::UiMode;

const STATUS_TICK: Duration = Duration::from_secs(1);
const SPINNER_TICK: Duration = Duration::from_millis(120);
const ACTIVITY_REVEAL_DELAY: Duration = Duration::from_millis(250);
const ACTIVITY_TARGET_WIDTH: usize = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Disabled,
    Down,
    Connecting,
    Reconnecting,
    Connected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Ts,
    Nc,
    Voice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Color {
    Green,
    Red,
    Dim,
}

struct UiState {
    ts: LinkState,
    nc: LinkState,
    voice: LinkState,
    listening: bool,
    active: usize,
    queued: usize,
    turns: u64,
    tools: u64,
    failures: u64,
    warnings: u64,
    errors: u64,
    started: Instant,
    finished: bool,
}

#[derive(Clone)]
pub(crate) struct Ui {
    inner: Arc<UiInner>,
}

struct UiInner {
    multi: MultiProgress,
    status: ProgressBar,
    rich: bool,
    color: bool,
    state: Mutex<UiState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivityLabel {
    platform: &'static str,
    target: String,
}

impl ActivityLabel {
    pub(crate) fn new(platform: &'static str, target: impl Into<String>) -> Self {
        let target = target.into();
        Self {
            platform,
            target: terminal_safe(target.trim()),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Activity {
    inner: Arc<ActivityInner>,
}

pub(crate) struct ActivityGuard {
    activity: Activity,
}

struct ActivityInner {
    ui: Ui,
    label: ActivityLabel,
    started: Instant,
    bar: ProgressBar,
    state: Mutex<ActivityState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityPhase {
    Queued,
    Active,
    Finished,
}

struct ActivityState {
    phase: ActivityPhase,
    visible: bool,
    tool_count: usize,
}

pub(crate) struct Phase {
    ui: Ui,
    inner: Option<PhaseInner>,
}

struct PhaseInner {
    label: String,
    bar: ProgressBar,
}

impl Ui {
    pub(crate) fn new(mode: UiMode) -> Self {
        let rich = match mode {
            UiMode::Plain => false,
            UiMode::Auto => {
                io::stdout().is_terminal()
                    && io::stderr().is_terminal()
                    && std::env::var("TERM").map_or(true, |term| term != "dumb")
            }
        };
        let color = rich && std::env::var_os("NO_COLOR").is_none();
        let multi = if rich {
            MultiProgress::new()
        } else {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        };
        let status = ProgressBar::new(0);
        status.set_draw_target(ProgressDrawTarget::hidden());
        status.set_style(
            ProgressStyle::with_template("{msg}  {elapsed_precise}")
                .expect("valid status template"),
        );
        multi.add(status.clone());
        if rich {
            status.enable_steady_tick(STATUS_TICK);
        }

        let ui = Self {
            inner: Arc::new(UiInner {
                multi,
                status,
                rich,
                color,
                state: Mutex::new(UiState {
                    ts: LinkState::Connecting,
                    nc: LinkState::Disabled,
                    voice: LinkState::Disabled,
                    listening: false,
                    active: 0,
                    queued: 0,
                    turns: 0,
                    tools: 0,
                    failures: 0,
                    warnings: 0,
                    errors: 0,
                    started: Instant::now(),
                    finished: false,
                }),
            }),
        };
        ui.refresh_status();
        ui
    }

    pub(crate) fn rich(&self) -> bool {
        self.inner.rich
    }

    pub(crate) fn color_enabled(&self) -> bool {
        self.inner.color
    }

    pub(crate) fn suppress_console_logs(&self) -> bool {
        self.rich() && !self.inner.multi.is_hidden()
    }

    /// Write a normal newline-terminated line above the dynamic bars.
    ///
    /// `MultiProgress::println` keeps its input inside the progress redraw canvas and
    /// does not emit a regular LF. Logs and status lines must stay in normal scrollback
    /// so they survive copying and terminal recording.
    pub(crate) fn print_line(&self, line: &str) {
        if self.rich() && !self.inner.multi.is_hidden() {
            self.inner.multi.suspend(|| {
                let mut stderr = io::stderr().lock();
                let _ = writeln!(stderr, "{line}");
            });
        } else {
            let mut stdout = io::stdout().lock();
            let _ = writeln!(stdout, "{line}");
        }
    }

    pub(crate) fn write_console_lines(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        if self.rich() && !self.inner.multi.is_hidden() {
            self.inner.multi.suspend(|| {
                let mut stderr = io::stderr().lock();
                for line in lines {
                    if writeln!(stderr, "{line}").is_err() {
                        break;
                    }
                }
            });
        } else {
            let mut stdout = io::stdout().lock();
            for line in lines {
                if writeln!(stdout, "{line}").is_err() {
                    break;
                }
            }
        }
    }

    pub(crate) fn step_ok(&self, label: &str, detail: impl AsRef<str>) {
        let mark = self.mark("✓", "ok");
        let line = format!(
            "{} {:<18} {}",
            self.paint(mark, Color::Green),
            label,
            detail.as_ref()
        );
        self.print_line(&line);
    }

    pub(crate) fn listening(&self, detail: impl AsRef<str>) {
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            state.listening = true;
        }
        self.refresh_status();
        let mark = self.mark("●", "*");
        let line = format!(
            "{} {:<18} {}",
            self.paint(mark, Color::Green),
            "listening",
            detail.as_ref()
        );
        self.print_line(&line);
    }

    pub(crate) fn ts_connecting(&self) {
        self.set_link(Link::Ts, LinkState::Connecting);
    }

    pub(crate) fn ts_connected(&self) {
        self.set_link(Link::Ts, LinkState::Connected);
    }

    pub(crate) fn reconnect_started(&self) {
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            state.listening = false;
            state.ts = LinkState::Reconnecting;
            if state.nc != LinkState::Disabled {
                state.nc = LinkState::Down;
            }
            if state.voice != LinkState::Disabled {
                state.voice = LinkState::Down;
            }
        }
        self.refresh_status();
    }

    pub(crate) fn nc_connecting(&self) {
        self.set_link(Link::Nc, LinkState::Connecting);
    }

    pub(crate) fn nc_connected(&self) {
        self.set_link(Link::Nc, LinkState::Connected);
    }

    pub(crate) fn nc_down(&self) {
        self.set_link(Link::Nc, LinkState::Down);
    }

    pub(crate) fn nc_disabled(&self) {
        self.set_link(Link::Nc, LinkState::Disabled);
    }

    pub(crate) fn voice_connecting(&self) {
        self.set_link(Link::Voice, LinkState::Connecting);
    }

    pub(crate) fn voice_connected(&self) {
        self.set_link(Link::Voice, LinkState::Connected);
    }

    pub(crate) fn voice_down(&self) {
        self.set_link(Link::Voice, LinkState::Down);
    }

    pub(crate) fn voice_disabled(&self) {
        self.set_link(Link::Voice, LinkState::Disabled);
    }

    pub(crate) fn phase(&self, label: &str, action: &str) -> Phase {
        if !self.rich() {
            return Phase {
                ui: self.clone(),
                inner: None,
            };
        }
        let bar = ProgressBar::new_spinner();
        bar.set_draw_target(ProgressDrawTarget::hidden());
        bar.set_style(spinner_style(self.color_enabled()));
        bar.set_message(format!("{label:<18} {action}"));
        self.inner.multi.add(bar.clone());
        bar.enable_steady_tick(SPINNER_TICK);
        Phase {
            ui: self.clone(),
            inner: Some(PhaseInner {
                label: label.to_string(),
                bar,
            }),
        }
    }

    pub(crate) fn shutdown(&self, ok: bool) {
        let (uptime, turns, tools, failures, warnings, errors) = {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            if state.finished {
                return;
            }
            state.finished = true;
            (
                state.started.elapsed(),
                state.turns,
                state.tools,
                state.failures,
                state.warnings,
                state.errors,
            )
        };
        self.inner.status.finish_and_clear();
        self.inner.multi.remove(&self.inner.status);
        let _ = self.inner.multi.clear();

        let mark = if ok {
            self.paint(self.mark("✓", "ok"), Color::Green)
        } else {
            self.paint(self.mark("✖", "ERR"), Color::Red)
        };
        let status = if ok { "stopped" } else { "stopped with error" };
        let mut line = format!(
            "{} {} · {} · {} turns · {} tools",
            mark,
            status,
            format_duration(uptime),
            turns,
            tools
        );
        if failures > 0 {
            line.push_str(&format!(" · {failures} failed"));
        }
        if warnings > 0 {
            line.push_str(&format!(" · warn {warnings}"));
        }
        if errors > 0 {
            line.push_str(&format!(" · error {errors}"));
        }
        self.print_line(&line);
    }

    pub(crate) fn activity_queued(&self) {
        if self.is_finished() {
            return;
        }
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            state.queued += 1;
        }
        self.refresh_status();
    }

    pub(crate) fn note_warning(&self) {
        if self.is_finished() {
            return;
        }
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            state.warnings = state.warnings.saturating_add(1);
        }
        self.refresh_status();
    }

    pub(crate) fn note_error(&self) {
        if self.is_finished() {
            return;
        }
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            state.errors = state.errors.saturating_add(1);
        }
        self.refresh_status();
    }

    fn activity_activated(&self) {
        if self.is_finished() {
            return;
        }
        let mut state = self.inner.state.lock().expect("ui state poisoned");
        state.queued = state.queued.saturating_sub(1);
        state.active += 1;
        state.turns += 1;
        drop(state);
        self.refresh_status();
    }

    fn activity_finished(&self) {
        if self.is_finished() {
            return;
        }
        let mut state = self.inner.state.lock().expect("ui state poisoned");
        state.active = state.active.saturating_sub(1);
        drop(state);
        self.refresh_status();
    }

    fn queued_finished(&self) {
        if self.is_finished() {
            return;
        }
        let mut state = self.inner.state.lock().expect("ui state poisoned");
        state.queued = state.queued.saturating_sub(1);
        drop(state);
        self.refresh_status();
    }

    fn tool_started(&self) {
        let mut state = self.inner.state.lock().expect("ui state poisoned");
        state.tools += 1;
    }

    fn activity_failed(&self) {
        let mut state = self.inner.state.lock().expect("ui state poisoned");
        state.failures += 1;
    }

    fn set_link(&self, link: Link, next: LinkState) {
        {
            let mut state = self.inner.state.lock().expect("ui state poisoned");
            match link {
                Link::Ts => state.ts = next,
                Link::Nc => state.nc = next,
                Link::Voice => state.voice = next,
            }
        }
        self.refresh_status();
    }

    fn refresh_status(&self) {
        let text = {
            let state = self.inner.state.lock().expect("ui state poisoned");
            status_text(&state, self.rich(), self.color_enabled())
        };
        self.inner.status.set_message(text);
    }

    fn is_finished(&self) -> bool {
        self.inner.state.lock().expect("ui state poisoned").finished
    }

    fn mark(&self, rich: &'static str, plain: &'static str) -> &'static str {
        if self.rich() {
            rich
        } else {
            plain
        }
    }

    fn paint(&self, text: &str, color: Color) -> String {
        if !self.color_enabled() {
            return text.to_string();
        }
        let code = match color {
            Color::Green => "32",
            Color::Red => "31",
            Color::Dim => "2",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    fn activity_line(&self, line: &str) {
        if self.rich() && !self.is_finished() {
            self.print_line(line);
        }
    }
}

impl Phase {
    pub(crate) fn ok(mut self, detail: impl Into<String>) {
        self.finish("✓", Color::Green, detail.into());
    }

    pub(crate) fn fail(mut self, detail: impl Into<String>) {
        self.finish("✖", Color::Red, detail.into());
    }

    fn finish(&mut self, mark: &str, color: Color, detail: String) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        inner.bar.finish_and_clear();
        self.ui.inner.multi.remove(&inner.bar);
        if self.ui.is_finished() {
            return;
        }
        let detail = truncate_width(&terminal_safe(&detail), 55);
        let line = format!(
            "{} {:<18} {}",
            self.ui.paint(mark, color),
            inner.label,
            detail
        );
        self.ui.print_line(&line);
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.bar.finish_and_clear();
            self.ui.inner.multi.remove(&inner.bar);
        }
    }
}

impl ActivityGuard {
    pub(crate) fn new(ui: Ui, label: ActivityLabel) -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_draw_target(ProgressDrawTarget::hidden());
        bar.set_style(spinner_style(ui.color_enabled()));
        bar.set_message(activity_prefix(&label, "queued"));
        let activity = Activity {
            inner: Arc::new(ActivityInner {
                ui: ui.clone(),
                label,
                started: Instant::now(),
                bar,
                state: Mutex::new(ActivityState {
                    phase: ActivityPhase::Queued,
                    visible: false,
                    tool_count: 0,
                }),
            }),
        };
        ui.activity_queued();
        Self { activity }
    }

    pub(crate) fn handle(&self) -> Activity {
        self.activity.clone()
    }

    pub(crate) fn activate(&self) {
        self.activity.activate();
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.activity.cancelled();
    }
}

impl Activity {
    fn activate(&self) {
        {
            let mut state = self.inner.state.lock().expect("activity state poisoned");
            if state.phase != ActivityPhase::Queued {
                return;
            }
            state.phase = ActivityPhase::Active;
        }
        self.inner.ui.activity_activated();
        self.set_message("thinking");
        self.schedule_reveal();
    }

    pub(crate) fn thinking(&self) {
        self.set_message("thinking");
    }

    pub(crate) fn tool_start(&self, tool: &str) {
        let tool = truncate_width(&terminal_safe(tool), 24);
        {
            let mut state = self.inner.state.lock().expect("activity state poisoned");
            if state.phase == ActivityPhase::Finished {
                return;
            }
            state.tool_count += 1;
        }
        self.inner.ui.tool_started();
        self.set_message(&format!("tool {tool}"));
    }

    pub(crate) fn tool_end(&self, tool: &str, elapsed: Duration) {
        let tool = truncate_width(&terminal_safe(tool), 24);
        self.set_message("thinking");
        let line = format!(
            "{} {} {} · {:.1}s",
            activity_prefix(&self.inner.label, ""),
            self.inner.ui.mark("↳", "->"),
            tool,
            elapsed.as_secs_f64()
        );
        self.inner.ui.activity_line(&line);
    }

    pub(crate) fn replying(&self) {
        self.set_message("replying");
    }

    pub(crate) fn replied(&self, chars: usize) {
        let detail = if chars == 0 {
            "replied".to_string()
        } else {
            format!("replied · {chars} chars")
        };
        self.finish("✓", Color::Green, detail, false);
    }

    pub(crate) fn completed(&self, detail: &str) {
        self.finish("✓", Color::Green, detail.to_string(), false);
    }

    pub(crate) fn empty(&self) {
        self.finish("-", Color::Dim, "no reply".to_string(), false);
    }

    pub(crate) fn failed(&self, reason: &str) {
        self.finish("✖", Color::Red, format!("failed · {reason}"), true);
    }

    pub(crate) fn cancelled(&self) {
        self.finish("-", Color::Dim, "cancelled".to_string(), false);
    }

    fn set_message(&self, message: &str) {
        let rendered = {
            let state = self.inner.state.lock().expect("activity state poisoned");
            if state.phase == ActivityPhase::Finished {
                return;
            }
            activity_prefix(&self.inner.label, message)
        };
        self.inner.bar.set_message(rendered);
    }

    fn schedule_reveal(&self) {
        if !self.inner.ui.rich() || self.inner.ui.is_finished() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.reveal();
            return;
        };
        let activity = self.clone();
        handle.spawn(async move {
            tokio::time::sleep(ACTIVITY_REVEAL_DELAY).await;
            activity.reveal();
        });
    }

    fn reveal(&self) {
        if !self.inner.ui.rich() || self.inner.ui.is_finished() {
            return;
        }
        let mut state = self.inner.state.lock().expect("activity state poisoned");
        if state.phase != ActivityPhase::Active || state.visible {
            return;
        }
        state.visible = true;
        self.inner.ui.inner.multi.add(self.inner.bar.clone());
        self.inner.bar.enable_steady_tick(SPINNER_TICK);
    }

    fn finish(&self, mark: &str, color: Color, detail: String, failed: bool) {
        let (phase, visible, tool_count) = {
            let mut state = self.inner.state.lock().expect("activity state poisoned");
            if state.phase == ActivityPhase::Finished {
                return;
            }
            let phase = state.phase;
            state.phase = ActivityPhase::Finished;
            let visible = state.visible;
            state.visible = false;
            (phase, visible, state.tool_count)
        };
        if visible {
            self.inner.bar.finish_and_clear();
            self.inner.ui.inner.multi.remove(&self.inner.bar);
        }
        match phase {
            ActivityPhase::Queued => self.inner.ui.queued_finished(),
            ActivityPhase::Active => self.inner.ui.activity_finished(),
            ActivityPhase::Finished => {}
        }
        if failed {
            self.inner.ui.activity_failed();
        }
        if !self.inner.ui.rich() {
            return;
        }

        let tools = if tool_count == 0 {
            String::new()
        } else if tool_count == 1 {
            " · 1 tool".to_string()
        } else {
            format!(" · {tool_count} tools")
        };
        let line = format!(
            "{} {} {} · {:.1}s{}",
            activity_prefix(&self.inner.label, ""),
            self.inner.ui.paint(mark, color),
            detail,
            self.inner.started.elapsed().as_secs_f64(),
            tools
        );
        self.inner.ui.activity_line(&line);
    }
}

fn spinner_style(color: bool) -> ProgressStyle {
    let template = if color {
        "{spinner:.cyan} {msg} · {elapsed_precise}"
    } else {
        "{spinner} {msg} · {elapsed_precise}"
    };
    ProgressStyle::with_template(template)
        .expect("valid activity template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

fn activity_prefix(label: &ActivityLabel, phase: &str) -> String {
    let time = Local::now().format("%H:%M:%S");
    let target = truncate_width(&label.target, ACTIVITY_TARGET_WIDTH);
    let target = pad_width(&target, ACTIVITY_TARGET_WIDTH);
    if phase.is_empty() {
        format!("{time}  {:<5} {}", label.platform, target)
    } else {
        format!("{time}  {:<5} {} {}", label.platform, target, phase)
    }
}

fn truncate_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }
    let mut truncated = String::new();
    let mut width = 0;
    for ch in value.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width.saturating_sub(1) {
            break;
        }
        truncated.push(ch);
        width += char_width;
    }
    truncated.push('…');
    truncated
}

fn pad_width(value: &str, width: usize) -> String {
    let current = UnicodeWidthStr::width(value);
    format!("{value}{}", " ".repeat(width.saturating_sub(current)))
}

fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .map(|ch| if is_terminal_control(ch) { ' ' } else { ch })
        .collect()
}

fn is_terminal_control(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn status_text(state: &UiState, rich: bool, color: bool) -> String {
    let paint = |text: &str, code: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let link = |name: &str, state: LinkState| {
        let (rich_mark, plain_mark, code) = match state {
            LinkState::Disabled => ("–", "-", "2"),
            LinkState::Down => ("○", "x", "31"),
            LinkState::Connecting => ("◌", "o", "33"),
            LinkState::Reconnecting => ("↻", "r", "33"),
            LinkState::Connected => ("●", "*", "32"),
        };
        let mark = if rich { rich_mark } else { plain_mark };
        let text = format!("{mark} {name}");
        paint(&text, code)
    };
    let status = if state.listening {
        ""
    } else if state.ts == LinkState::Reconnecting {
        " · reconnecting"
    } else {
        " · starting"
    };
    let diagnostics = match (state.warnings, state.errors) {
        (0, 0) => String::new(),
        (warnings, 0) => format!("   {}", paint(&format!("! warn {warnings}"), "33")),
        (0, errors) => format!("   {}", paint(&format!("✖ error {errors}"), "31")),
        (warnings, errors) => format!(
            "   {}   {}",
            paint(&format!("! warn {warnings}"), "33"),
            paint(&format!("✖ error {errors}"), "31")
        ),
    };
    format!(
        "{}   {}   {}   {} active · {} queued{}{}",
        link("TS", state.ts),
        link("NC", state.nc),
        link("voice", state.voice),
        state.active,
        state.queued,
        diagnostics,
        status
    )
}

#[cfg(test)]
mod tests {
    use super::{
        format_duration, pad_width, status_text, terminal_safe, truncate_width, ActivityGuard,
        ActivityLabel, LinkState, Ui, UiMode, UiState,
    };
    use std::time::{Duration, Instant};

    fn activity_counts(ui: &Ui) -> (usize, usize, u64, u64, u64) {
        let state = ui.inner.state.lock().unwrap();
        (
            state.active,
            state.queued,
            state.turns,
            state.tools,
            state.failures,
        )
    }

    #[test]
    fn truncates_wide_names_by_display_width() {
        assert_eq!(truncate_width("Alice", 18), "Alice");
        assert_eq!(truncate_width("一二三四五六", 5), "一二…");
        assert_eq!(pad_width("一二", 6), "一二  ");
    }

    #[test]
    fn terminal_text_drops_control_characters() {
        assert_eq!(
            terminal_safe("Alice\nAdmin\x1b[31m\u{202E}"),
            "Alice Admin [31m "
        );
    }

    #[test]
    fn status_text_reports_disabled_links_and_work_counts() {
        let state = UiState {
            ts: LinkState::Connected,
            nc: LinkState::Disabled,
            voice: LinkState::Down,
            listening: true,
            active: 2,
            queued: 1,
            turns: 4,
            tools: 3,
            failures: 0,
            warnings: 2,
            errors: 1,
            started: Instant::now(),
            finished: false,
        };

        let text = status_text(&state, false, false);
        assert!(!text.starts_with(' '));
        assert!(text.contains("* TS"));
        assert!(text.contains("- NC"));
        assert!(text.contains("x voice"));
        assert!(text.contains("2 active · 1 queued"));
        assert!(text.contains("! warn 2"));
        assert!(text.contains("✖ error 1"));
        assert!(!text.contains("starting"));
    }

    #[test]
    fn formats_short_and_long_uptimes() {
        assert_eq!(format_duration(Duration::from_secs(63)), "01:03");
        assert_eq!(format_duration(Duration::from_secs(3_663)), "1:01:03");
    }

    #[test]
    fn activity_moves_from_queued_to_active_to_finished() {
        let ui = Ui::new(UiMode::Plain);
        let guard = ActivityGuard::new(ui.clone(), ActivityLabel::new("TS", "Alice"));
        assert_eq!(activity_counts(&ui), (0, 1, 0, 0, 0));

        guard.activate();
        assert_eq!(activity_counts(&ui), (1, 0, 1, 0, 0));

        guard.handle().tool_start("web_search");
        guard.handle().replied(12);
        assert_eq!(activity_counts(&ui), (0, 0, 1, 1, 0));

        drop(guard);
        assert_eq!(activity_counts(&ui), (0, 0, 1, 1, 0));
    }

    #[test]
    fn failed_activity_updates_the_failure_count_once() {
        let ui = Ui::new(UiMode::Plain);
        let guard = ActivityGuard::new(ui.clone(), ActivityLabel::new("NC", "Group 981"));
        guard.activate();
        guard.handle().failed("backend");
        guard.handle().failed("backend");
        assert_eq!(activity_counts(&ui), (0, 0, 1, 0, 1));
    }

    #[test]
    fn diagnostics_count_warnings_and_errors_separately() {
        let ui = Ui::new(UiMode::Plain);
        ui.note_warning();
        ui.note_warning();
        ui.note_error();

        let state = ui.inner.state.lock().unwrap();
        assert_eq!(state.warnings, 2);
        assert_eq!(state.errors, 1);
    }
}
