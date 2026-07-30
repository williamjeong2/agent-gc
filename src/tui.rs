use crate::scanner::{
    human_size, remove_artifacts, scan_to_channel, total_size, Artifact, RiskLevel, ScanEvent,
    ScanProgress,
};
use anyhow::Result;
use chrono::Utc;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Terminal;
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const SEARCH_BAR_WIDTH: usize = 25;
const SEARCH_BAR_LABEL: &str = "Scanning ";
const SEARCH_BAR_PULSE_WIDTH: usize = 7;
const UI_TICK_MS: u64 = 80;

const MIN_SIZE_STEPS: [Option<u64>; 4] = [
    None,
    Some(10_000_000),
    Some(100_000_000),
    Some(1_000_000_000),
];

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub fn run(paths: Vec<PathBuf>) -> Result<()> {
    let mut guard = TerminalGuard::new()?;
    let result = run_app(guard.terminal_mut(), paths);
    guard.restore();
    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, paths: Vec<PathBuf>) -> Result<()> {
    let mut app = App::new(paths)?;

    loop {
        app.drain_scan_events();
        terminal.draw(|frame| app.draw(frame))?;
        if !event::poll(Duration::from_millis(UI_TICK_MS))? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if app.confirm_delete {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('d') => {
                        app.delete_selected();
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                        app.confirm_delete = false
                    }
                    _ => {}
                }
                continue;
            }

            if app.detail {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => app.detail = false,
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break;
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    if app.confirm_delete || app.detail {
                        app.confirm_delete = false;
                        app.detail = false;
                    } else {
                        break;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => app.next(),
                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                KeyCode::Char(' ') => app.toggle_selected(),
                KeyCode::Char('a') => app.select_all_safe(),
                KeyCode::Char('f') => app.next_filter(),
                KeyCode::Char('t') => app.next_risk_filter(),
                KeyCode::Char('m') => app.next_min_size(),
                KeyCode::Char('s') => app.next_sort(),
                KeyCode::Char('r') => app.rescan(),
                KeyCode::Enter => app.detail = !app.detail,
                KeyCode::Char('d') => {
                    if app.scanning {
                        app.message =
                            Some("Wait for the scan to finish before deleting.".to_string());
                    } else if !app.selected.is_empty() {
                        app.confirm_delete = true;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

struct App {
    paths: Vec<PathBuf>,
    artifacts: Vec<Artifact>,
    selected: HashSet<PathBuf>,
    deleted: HashSet<PathBuf>,
    deleted_size: u64,
    state: TableState,
    scan_receiver: Option<Receiver<ScanEvent>>,
    scanning: bool,
    scan_started_at: Instant,
    scanned_dirs: u64,
    completed_search_tasks: u64,
    pending_search_tasks: u64,
    completed_stats_calculations: u64,
    pending_stats_calculations: u64,
    current_path: Option<PathBuf>,
    detail: bool,
    confirm_delete: bool,
    message: Option<String>,
    filter: CategoryFilter,
    risk_filter: RiskFilter,
    min_size: Option<u64>,
    min_size_index: usize,
    sort_mode: SortMode,
    focus_pinned_by_user: bool,
    scan_errors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CategoryFilter {
    All,
    Agent,
    AgentCache,
    Node,
    Python,
    Rust,
    Cache,
    Other,
}

impl CategoryFilter {
    fn label(self) -> &'static str {
        match self {
            CategoryFilter::All => "ALL",
            CategoryFilter::Agent => "AGENT",
            CategoryFilter::AgentCache => "AGENT_CACHE",
            CategoryFilter::Node => "NODE",
            CategoryFilter::Python => "PYTHON",
            CategoryFilter::Rust => "RUST",
            CategoryFilter::Cache => "CACHE",
            CategoryFilter::Other => "OTHER",
        }
    }

    fn next(self) -> Self {
        match self {
            CategoryFilter::All => CategoryFilter::Agent,
            CategoryFilter::Agent => CategoryFilter::AgentCache,
            CategoryFilter::AgentCache => CategoryFilter::Node,
            CategoryFilter::Node => CategoryFilter::Python,
            CategoryFilter::Python => CategoryFilter::Rust,
            CategoryFilter::Rust => CategoryFilter::Cache,
            CategoryFilter::Cache => CategoryFilter::Other,
            CategoryFilter::Other => CategoryFilter::All,
        }
    }

    fn matches(self, artifact: &Artifact) -> bool {
        match self {
            CategoryFilter::All => true,
            CategoryFilter::Agent => artifact.category == "AGENT",
            CategoryFilter::AgentCache => artifact.category == "AGENT_CACHE",
            CategoryFilter::Node => artifact.category == "NODE",
            CategoryFilter::Python => artifact.category == "PY",
            CategoryFilter::Rust => artifact.category == "RUST",
            CategoryFilter::Cache => artifact.category == "CACHE",
            CategoryFilter::Other => !matches!(
                artifact.category.as_str(),
                "AGENT" | "AGENT_CACHE" | "NODE" | "PY" | "RUST" | "CACHE"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RiskFilter {
    All,
    Safe,
    Caution,
    Danger,
}

impl RiskFilter {
    fn label(self) -> &'static str {
        match self {
            RiskFilter::All => "ALL",
            RiskFilter::Safe => "SAFE",
            RiskFilter::Caution => "CAUTION",
            RiskFilter::Danger => "DANGER",
        }
    }

    fn next(self) -> Self {
        match self {
            RiskFilter::All => RiskFilter::Safe,
            RiskFilter::Safe => RiskFilter::Caution,
            RiskFilter::Caution => RiskFilter::Danger,
            RiskFilter::Danger => RiskFilter::All,
        }
    }

    fn matches(self, artifact: &Artifact) -> bool {
        match self {
            RiskFilter::All => true,
            RiskFilter::Safe => artifact.risk_level == RiskLevel::Safe,
            RiskFilter::Caution => artifact.risk_level == RiskLevel::Caution,
            RiskFilter::Danger => artifact.risk_level == RiskLevel::Danger,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortMode {
    SizeDesc,
    SizeAsc,
    AgeDesc,
    AgeAsc,
    Path,
    Risk,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            SortMode::SizeDesc => "SIZE ↓",
            SortMode::SizeAsc => "SIZE ↑",
            SortMode::AgeDesc => "AGE ↓",
            SortMode::AgeAsc => "AGE ↑",
            SortMode::Path => "PATH",
            SortMode::Risk => "RISK",
        }
    }

    fn next(self) -> Self {
        match self {
            SortMode::SizeDesc => SortMode::SizeAsc,
            SortMode::SizeAsc => SortMode::AgeDesc,
            SortMode::AgeDesc => SortMode::AgeAsc,
            SortMode::AgeAsc => SortMode::Path,
            SortMode::Path => SortMode::Risk,
            SortMode::Risk => SortMode::SizeDesc,
        }
    }
}

impl App {
    fn new(paths: Vec<PathBuf>) -> Result<Self> {
        let mut state = TableState::default();
        state.select(None);
        let mut app = Self {
            paths,
            artifacts: Vec::new(),
            selected: HashSet::new(),
            deleted: HashSet::new(),
            deleted_size: 0,
            state,
            scan_receiver: None,
            scanning: false,
            scan_started_at: Instant::now(),
            scanned_dirs: 0,
            completed_search_tasks: 0,
            pending_search_tasks: 0,
            completed_stats_calculations: 0,
            pending_stats_calculations: 0,
            current_path: None,
            detail: false,
            confirm_delete: false,
            message: Some("Scanning... results will appear as they are found.".to_string()),
            filter: CategoryFilter::All,
            risk_filter: RiskFilter::All,
            min_size: None,
            min_size_index: 0,
            sort_mode: SortMode::SizeDesc,
            focus_pinned_by_user: false,
            scan_errors: 0,
        };
        app.start_scan();
        Ok(app)
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8),
                Constraint::Min(5),
                Constraint::Length(6),
            ])
            .split(area);

        let selected_size = self.selected_size();
        let visible_indices = self.visible_indices();
        self.draw_header(frame, chunks[0], selected_size, visible_indices.len());

        let rows: Vec<Row> = self
            .visible_artifacts(&visible_indices)
            .into_iter()
            .map(|(_, artifact)| {
                let deleted = self.is_deleted(artifact);
                let sel_marker = self.selection_marker_for(artifact);
                let git = match artifact.git_status_clean {
                    Some(true) => "clean",
                    Some(false) => "dirty",
                    None => "no-git",
                };
                Row::new(vec![
                    Cell::from(sel_marker),
                    Cell::from(artifact.risk_level.label()),
                    Cell::from(artifact.size_human.clone()),
                    Cell::from(age_label(artifact)),
                    Cell::from(artifact.category.clone()),
                    Cell::from(git),
                    Cell::from(artifact.path.display().to_string()),
                ])
                .style(row_style_for(artifact.risk_level, deleted))
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(9),
                Constraint::Length(10),
                Constraint::Length(7),
                Constraint::Length(13),
                Constraint::Length(8),
                Constraint::Min(24),
            ],
        )
        .header(
            Row::new(vec![
                "Sel", "Risk", "Size", "Age", "Category", "Git", "Path",
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1),
        )
        .block(
            Block::default()
                .title(format!(
                    " Cleanup candidates  cat:{}  risk:{}  min:{}  sort:{}  showing:{}/{} ",
                    self.filter.label(),
                    self.risk_filter.label(),
                    min_size_label(self.min_size),
                    self.sort_mode.label(),
                    visible_indices.len(),
                    self.artifacts.len()
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");
        frame.render_stateful_widget(table, chunks[1], &mut self.state);

        let footer = if self.artifacts.is_empty() && self.scanning {
            "Scanning default paths. Large dependency folders may take a moment to size."
                .to_string()
        } else {
            self.message.clone().unwrap_or_else(|| {
                "Space selects SAFE/CAUTION. a adds only SAFE. DANGER is locked.".to_string()
            })
        };
        self.draw_footer(frame, chunks[2], footer);

        if self.detail {
            self.draw_detail(frame);
        }
        if self.confirm_delete {
            self.draw_confirm(frame);
        }
    }

    fn draw_detail(&self, frame: &mut ratatui::Frame) {
        let Some(visible_index) = self.state.selected() else {
            return;
        };
        let visible_indices = self.visible_indices();
        let Some(index) = visible_indices.get(visible_index).copied() else {
            return;
        };
        let Some(artifact) = self.artifacts.get(index) else {
            return;
        };
        let area = centered_rect(74, 70, frame.area());
        let danger = if artifact.dangerous_files_detected.is_empty() {
            "none".to_string()
        } else {
            artifact
                .dangerous_files_detected
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let modified = artifact
            .last_modified
            .map(|date| date.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_string());
        let text = format!(
            "Path: {}\nType: {}\nProject: {}\nSize: {}\nLast modified: {}\nGit status clean: {}\nRisk: {}\nReason: {}\nDangerous files: {}\nRecovery: reinstall dependencies or rebuild the project if needed",
            artifact.path.display(),
            artifact.category,
            artifact.project_name.as_deref().unwrap_or("unknown"),
            artifact.size_human,
            modified,
            artifact
                .git_status_clean
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            artifact.risk_level.label(),
            artifact.reason,
            danger
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::default().title("Details").borders(Borders::ALL)),
            area,
        );
    }

    fn draw_header(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        selected_size: u64,
        visible_count: usize,
    ) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);

        let elapsed = self.scan_started_at.elapsed().as_secs();
        let title = if self.scanning {
            format!("Searching {}", self.spinner())
        } else {
            "Search completed".to_string()
        };
        let status = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "agent-gc",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    title,
                    Style::default()
                        .fg(if self.scanning {
                            Color::Cyan
                        } else {
                            Color::Green
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    format!("{} checked", self.scanned_dirs),
                    Style::default().fg(Color::Gray),
                ),
                Span::raw("   "),
                Span::styled(
                    format!("{} queued", self.pending_search_tasks),
                    Style::default().fg(Color::Gray),
                ),
                Span::raw("   "),
                Span::styled(
                    format!("{} candidates", self.artifacts.len()),
                    Style::default().fg(Color::Gray),
                ),
            ]),
            self.search_bar(area.width.saturating_sub(4) as usize),
        ])
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
        frame.render_widget(status, outer[0]);

        let metrics = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
                Constraint::Percentage(20),
            ])
            .split(outer[1]);
        self.render_metric(
            frame,
            metrics[0],
            "RELEASABLE",
            &human_size(self.releasable_size()),
            Color::Green,
        );
        self.render_metric(
            frame,
            metrics[1],
            "SELECTED",
            &human_size(selected_size),
            Color::Yellow,
        );
        self.render_metric(
            frame,
            metrics[2],
            "DELETED",
            &human_size(self.deleted_size),
            Color::LightGreen,
        );
        self.render_metric(
            frame,
            metrics[3],
            "ITEMS",
            &format!("{}/{}", visible_count, self.artifacts.len()),
            Color::Cyan,
        );
        self.render_metric(
            frame,
            metrics[4],
            "ELAPSED",
            &format!("{}s", elapsed),
            Color::Magenta,
        );

        let current_path = self
            .current_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "Preparing scan...".to_string());
        let label = if self.scanning {
            format!(
                "Current: {}   cat:{} risk:{} showing:{}",
                truncate_middle(&current_path, area.width.saturating_sub(52) as usize),
                self.filter.label(),
                self.risk_filter.label(),
                visible_count
            )
        } else {
            "Ready. Review candidates and select what to remove.".to_string()
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Center)
                .style(Style::default().fg(if self.scanning {
                    Color::Cyan
                } else {
                    Color::Green
                }))
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(Color::DarkGray)),
                ),
            outer[2],
        );
    }

    fn render_metric(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        label_text: &str,
        value: &str,
        color: Color,
    ) {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    label_text.to_string(),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(Span::styled(
                    value.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::LEFT | Borders::RIGHT)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            area,
        );
    }

    fn draw_confirm(&self, frame: &mut ratatui::Frame) {
        let selected: Vec<Artifact> = self
            .artifacts
            .iter()
            .filter(|artifact| self.is_active_selected(artifact))
            .cloned()
            .collect();
        let caution = selected
            .iter()
            .filter(|artifact| artifact.risk_level == RiskLevel::Caution)
            .count();
        let text = format!(
            "Delete {} selected items and reclaim {}?\n\nSAFE and manually selected CAUTION items will be deleted.\nDANGER items stay locked. CAUTION selected: {}\n\nPress y to delete, n to cancel.",
            selected.len(),
            human_size(total_size(&selected)),
            caution
        );
        let area = centered_rect(60, 28, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::default()
                    .title("Confirm delete")
                    .borders(Borders::ALL),
            ),
            area,
        );
    }

    fn draw_footer(
        &self,
        frame: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        message: String,
    ) {
        let block = Block::default()
            .title(" Controls ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = inset(area, 1, 1);
        frame.render_widget(block, area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(Color::Gray))
                .alignment(Alignment::Left),
            rows[0],
        );

        let key_line = Line::from(vec![
            key("↑↓/jk"),
            label(" Move  "),
            key("Space"),
            label(" Select  "),
            key("a"),
            label(" SAFE  "),
            key("d"),
            label(" Del  "),
            key("f"),
            label(" Cat  "),
            key("t"),
            label(" Risk  "),
            key("m"),
            label(" Min  "),
            key("s"),
            label(" Sort  "),
            key("r"),
            label(" Rescan  "),
            key("q"),
            label(" Quit"),
        ]);
        let safety_line = Line::from(vec![
            Span::styled(
                "SAFE",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            label(" Space/a delete   "),
            Span::styled(
                "CAUTION",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            label(" Space delete   "),
            Span::styled(
                "DANGER",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            label(" cannot delete"),
        ]);
        frame.render_widget(Paragraph::new(vec![key_line, safety_line]), rows[1]);

        let selected_hint = if self.selected.is_empty() {
            "Nothing selected"
        } else {
            "Press d to review delete confirmation"
        };
        frame.render_widget(
            Paragraph::new(selected_hint)
                .style(Style::default().fg(Color::Gray))
                .alignment(Alignment::Right),
            rows[2],
        );
    }

    fn next(&mut self) {
        let visible_len = self.visible_indices().len();
        if visible_len == 0 {
            return;
        }
        let selected = self.state.selected().unwrap_or(0);
        self.state.select(Some((selected + 1) % visible_len));
        self.focus_pinned_by_user = true;
    }

    fn previous(&mut self) {
        let visible_len = self.visible_indices().len();
        if visible_len == 0 {
            return;
        }
        let selected = self.state.selected().unwrap_or(0);
        let next = if selected == 0 {
            visible_len - 1
        } else {
            selected - 1
        };
        self.state.select(Some(next));
        self.focus_pinned_by_user = true;
    }

    fn toggle_selected(&mut self) {
        let Some(visible_index) = self.state.selected() else {
            return;
        };
        let visible_indices = self.visible_indices();
        let Some(index) = visible_indices.get(visible_index).copied() else {
            return;
        };
        let Some(artifact) = self.artifacts.get(index) else {
            return;
        };
        if self.is_deleted(artifact) {
            self.message = Some("Deleted items are already removed from disk.".to_string());
            return;
        }
        if artifact.risk_level == RiskLevel::Danger {
            self.message = Some("DANGER is locked and cannot be deleted.".to_string());
            return;
        }
        if !self.selected.insert(artifact.path.clone()) {
            self.selected.remove(&artifact.path);
        }
    }

    fn select_all_safe(&mut self) {
        let safe_paths: Vec<PathBuf> = self
            .visible_indices()
            .into_iter()
            .filter_map(|index| {
                self.artifacts
                    .get(index)
                    .filter(|artifact| {
                        artifact.risk_level == RiskLevel::Safe && !self.is_deleted(artifact)
                    })
                    .map(|artifact| artifact.path.clone())
            })
            .collect();
        for path in safe_paths {
            self.selected.insert(path);
        }
        self.message = Some(format!(
            "Added visible SAFE items in {} filter. CAUTION needs Space. Total selected: {}.",
            self.filter.label(),
            self.selected.len()
        ));
    }

    fn next_filter(&mut self) {
        self.filter = self.filter.next();
        self.align_selection_to_filter();
        self.message = Some(format!("Category filter: {}", self.filter.label()));
    }

    fn next_risk_filter(&mut self) {
        self.risk_filter = self.risk_filter.next();
        self.align_selection_to_filter();
        self.message = Some(format!("Risk filter: {}", self.risk_filter.label()));
    }

    fn next_min_size(&mut self) {
        self.min_size_index = (self.min_size_index + 1) % MIN_SIZE_STEPS.len();
        self.min_size = MIN_SIZE_STEPS[self.min_size_index];
        self.align_selection_to_filter();
        self.message = Some(format!("Min size: {}", min_size_label(self.min_size)));
    }

    fn next_sort(&mut self) {
        let focused_path = self.focused_path();
        self.sort_mode = self.sort_mode.next();
        self.sort_artifacts();
        self.restore_focus(focused_path);
        self.message = Some(format!("Sort: {}", self.sort_mode.label()));
    }

    fn rescan(&mut self) {
        if self.scanning {
            self.message = Some("Scan already in progress.".to_string());
            return;
        }
        self.artifacts.clear();
        self.selected.clear();
        self.deleted.clear();
        self.deleted_size = 0;
        self.scan_errors = 0;
        self.state.select(None);
        self.focus_pinned_by_user = false;
        self.message = Some("Scanning... results will appear as they are found.".to_string());
        self.start_scan();
    }

    fn delete_selected(&mut self) {
        let selected: Vec<Artifact> = self
            .artifacts
            .iter()
            .filter(|artifact| self.is_active_selected(artifact))
            .cloned()
            .collect();
        let report = remove_artifacts(&selected);
        self.confirm_delete = false;
        self.deleted.extend(report.deleted.iter().cloned());
        self.deleted_size = self.deleted_size.saturating_add(report.bytes_removed);
        for path in &report.deleted {
            self.selected.remove(path);
        }
        self.align_selection_to_filter();
        if report.failed.is_empty() {
            self.message = Some(format!(
                "Deleted {} from {} items. Marked as DEL.",
                human_size(report.bytes_removed),
                report.deleted.len()
            ));
        } else {
            let first = &report.failed[0];
            self.message = Some(format!(
                "Deleted {} item(s) ({}). Failed {}: {} ({})",
                report.deleted.len(),
                human_size(report.bytes_removed),
                report.failed.len(),
                first.0.display(),
                first.1
            ));
        }
    }

    fn selected_size(&self) -> u64 {
        self.artifacts
            .iter()
            .filter(|artifact| self.is_active_selected(artifact))
            .map(|artifact| artifact.size)
            .sum()
    }

    fn releasable_size(&self) -> u64 {
        self.artifacts
            .iter()
            .filter(|artifact| !self.is_deleted(artifact))
            .map(|artifact| artifact.size)
            .sum()
    }

    fn is_deleted(&self, artifact: &Artifact) -> bool {
        self.deleted.contains(&artifact.path)
    }

    fn is_active_selected(&self, artifact: &Artifact) -> bool {
        self.selected.contains(&artifact.path) && !self.is_deleted(artifact)
    }

    fn selection_marker_for(&self, artifact: &Artifact) -> &'static str {
        if self.is_deleted(artifact) {
            "DEL"
        } else if self.selected.contains(&artifact.path) {
            "●"
        } else {
            " "
        }
    }

    fn start_scan(&mut self) {
        let (sender, receiver) = mpsc::channel();
        let paths = self.paths.clone();
        self.scan_receiver = Some(receiver);
        self.scanning = true;
        self.scan_started_at = Instant::now();
        self.scanned_dirs = 0;
        self.completed_search_tasks = 0;
        self.pending_search_tasks = 0;
        self.completed_stats_calculations = 0;
        self.pending_stats_calculations = 0;
        self.current_path = None;
        self.deleted.clear();
        self.deleted_size = 0;
        self.scan_errors = 0;
        self.focus_pinned_by_user = false;
        thread::spawn(move || scan_to_channel(paths, sender));
    }

    fn drain_scan_events(&mut self) {
        let Some(receiver) = self.scan_receiver.take() else {
            return;
        };
        let mut keep_receiver = true;
        while let Ok(event) = receiver.try_recv() {
            match event {
                ScanEvent::Progress(progress) => {
                    self.apply_scan_progress(progress);
                    self.message = Some(format!(
                        "Scanning... {} directories checked, {} artifacts found.",
                        self.scanned_dirs,
                        self.artifacts.len()
                    ));
                }
                ScanEvent::Artifact(artifact) => {
                    self.add_scan_artifact(artifact);
                    self.message =
                        Some(format!("Scanning... found {} items.", self.artifacts.len()));
                }
                ScanEvent::Error(message) => {
                    self.scan_errors = self.scan_errors.saturating_add(1);
                    self.message = Some(format!("Scan warning: {message}"));
                }
                ScanEvent::Done => {
                    self.scanning = false;
                    if self.scan_errors > 0 {
                        self.message = Some(format!(
                            "Scan complete. Found {} items ({} warnings).",
                            self.artifacts.len(),
                            self.scan_errors
                        ));
                    } else {
                        self.message = Some(format!(
                            "Scan complete. Found {} items.",
                            self.artifacts.len()
                        ));
                    }
                    keep_receiver = false;
                }
            }
        }
        if keep_receiver {
            self.scan_receiver = Some(receiver);
        }
    }

    fn apply_scan_progress(&mut self, progress: ScanProgress) {
        self.scanned_dirs = progress.scanned_dirs();
        self.completed_search_tasks = progress.completed_search_tasks;
        self.pending_search_tasks = progress.pending_search_tasks;
        self.completed_stats_calculations = progress.completed_stats_calculations;
        self.pending_stats_calculations = progress.pending_stats_calculations;
        self.current_path = Some(progress.current_path);
    }

    fn add_scan_artifact(&mut self, artifact: Artifact) {
        let focused_path = self.focused_path();
        self.artifacts.push(artifact);
        self.sort_artifacts();
        if self.focus_pinned_by_user {
            self.restore_focus(focused_path);
        } else {
            self.focus_first_visible();
        }
    }

    fn sort_artifacts(&mut self) {
        match self.sort_mode {
            SortMode::SizeDesc => self
                .artifacts
                .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path))),
            SortMode::SizeAsc => self
                .artifacts
                .sort_by(|a, b| a.size.cmp(&b.size).then_with(|| a.path.cmp(&b.path))),
            SortMode::AgeDesc => self.artifacts.sort_by(|a, b| {
                b.last_modified
                    .cmp(&a.last_modified)
                    .then_with(|| a.path.cmp(&b.path))
            }),
            SortMode::AgeAsc => self.artifacts.sort_by(|a, b| {
                a.last_modified
                    .cmp(&b.last_modified)
                    .then_with(|| a.path.cmp(&b.path))
            }),
            SortMode::Path => self.artifacts.sort_by(|a, b| a.path.cmp(&b.path)),
            SortMode::Risk => self.artifacts.sort_by(|a, b| {
                a.risk_level
                    .rank()
                    .cmp(&b.risk_level.rank())
                    .then_with(|| b.size.cmp(&a.size))
                    .then_with(|| a.path.cmp(&b.path))
            }),
        }
    }

    fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠀⢀", "⠀⡀", "⠀⠄", "⢀⠀", "⡀⠀", "⠄⠀", "⠂⠀", "⠀⠂", "⠀⠄", "⠀⡀"];
        FRAMES[self.time_tick() % FRAMES.len()]
    }

    fn time_tick(&self) -> usize {
        (self.scan_started_at.elapsed().as_millis() / UI_TICK_MS as u128) as usize
    }

    fn search_bar(&self, available_width: usize) -> Line<'static> {
        let inner_width = available_width
            .saturating_sub(SEARCH_BAR_LABEL.len() + 2)
            .clamp(10, SEARCH_BAR_WIDTH);
        if !self.scanning {
            return Line::from(vec![
                Span::styled(SEARCH_BAR_LABEL, Style::default().fg(Color::Gray)),
                Span::styled("[", Style::default().fg(Color::Gray)),
                Span::styled("▀".repeat(inner_width), Style::default().fg(Color::Green)),
                Span::styled("]", Style::default().fg(Color::Gray)),
            ]);
        }

        let (before_pulse, pulse_width, after_pulse) =
            Self::search_bar_pulse_segments(inner_width, self.time_tick());

        Line::from(vec![
            Span::styled(SEARCH_BAR_LABEL, Style::default().fg(Color::Gray)),
            Span::styled("[", Style::default().fg(Color::Gray)),
            Span::styled("·".repeat(before_pulse), Style::default().fg(Color::Gray)),
            Span::styled("█".repeat(pulse_width), Style::default().fg(Color::Cyan)),
            Span::styled("·".repeat(after_pulse), Style::default().fg(Color::Gray)),
            Span::styled("]", Style::default().fg(Color::Gray)),
        ])
    }

    fn search_bar_pulse_segments(inner_width: usize, tick: usize) -> (usize, usize, usize) {
        let pulse_width = SEARCH_BAR_PULSE_WIDTH.min(inner_width);
        if inner_width == pulse_width {
            return (0, pulse_width, 0);
        }

        let travel = inner_width - pulse_width;
        let cycle = travel * 2;
        let phase = tick % cycle;
        let start = if phase <= travel {
            phase
        } else {
            cycle - phase
        };
        (start, pulse_width, inner_width - start - pulse_width)
    }

    fn visible_indices(&self) -> Vec<usize> {
        self.artifacts
            .iter()
            .enumerate()
            .filter_map(|(index, artifact)| {
                if self.filter.matches(artifact)
                    && self.risk_filter.matches(artifact)
                    && self.min_size.is_none_or(|min| artifact.size >= min)
                {
                    Some(index)
                } else {
                    None
                }
            })
            .collect()
    }

    fn visible_artifacts<'a>(&'a self, indices: &'a [usize]) -> Vec<(usize, &'a Artifact)> {
        indices
            .iter()
            .filter_map(|index| {
                self.artifacts
                    .get(*index)
                    .map(|artifact| (*index, artifact))
            })
            .collect()
    }

    fn align_selection_to_filter(&mut self) {
        let visible_len = self.visible_indices().len();
        match (visible_len, self.state.selected()) {
            (0, _) => self.state.select(None),
            (_, None) => self.state.select(Some(0)),
            (len, Some(index)) if index >= len => self.state.select(Some(len - 1)),
            _ => {}
        }
    }

    fn focus_first_visible(&mut self) {
        if self.visible_indices().is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
    }

    fn focused_path(&self) -> Option<PathBuf> {
        let visible_index = self.state.selected()?;
        let visible_indices = self.visible_indices();
        let artifact_index = *visible_indices.get(visible_index)?;
        self.artifacts
            .get(artifact_index)
            .map(|artifact| artifact.path.clone())
    }

    fn restore_focus(&mut self, focused_path: Option<PathBuf>) {
        let visible_indices = self.visible_indices();
        if visible_indices.is_empty() {
            self.state.select(None);
            return;
        }
        if let Some(path) = focused_path {
            if let Some(visible_index) = visible_indices.iter().position(|artifact_index| {
                self.artifacts
                    .get(*artifact_index)
                    .is_some_and(|artifact| artifact.path == path)
            }) {
                self.state.select(Some(visible_index));
                return;
            }
        }
        if self.state.selected().is_none() {
            self.state.select(Some(0));
        } else {
            self.align_selection_to_filter();
        }
    }
}

fn min_size_label(min_size: Option<u64>) -> String {
    match min_size {
        None => "off".to_string(),
        Some(10_000_000) => "10MB".to_string(),
        Some(100_000_000) => "100MB".to_string(),
        Some(1_000_000_000) => "1GB".to_string(),
        Some(other) => human_size(other),
    }
}

fn row_style_for(risk: RiskLevel, deleted: bool) -> Style {
    if deleted {
        return Style::default().fg(Color::DarkGray);
    }
    match risk {
        RiskLevel::Safe => Style::default().fg(Color::Green),
        RiskLevel::Caution => Style::default().fg(Color::Yellow),
        RiskLevel::Danger => Style::default().fg(Color::Red),
    }
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
}

fn label(value: &'static str) -> Span<'static> {
    Span::styled(value, Style::default().fg(Color::White))
}

fn age_label(artifact: &Artifact) -> String {
    let Some(last_modified) = artifact.last_modified else {
        return "...".to_string();
    };
    let age = Utc::now().signed_duration_since(last_modified);
    if age.num_days() >= 1 {
        return format!("{}d", age.num_days().min(999));
    }
    if age.num_hours() >= 1 {
        return format!("{}h", age.num_hours());
    }
    if age.num_minutes() >= 1 {
        return format!("{}m", age.num_minutes());
    }
    "now".to_string()
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    area: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn inset(area: ratatui::layout::Rect, horizontal: u16, vertical: u16) -> ratatui::layout::Rect {
    ratatui::layout::Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y.saturating_add(vertical),
        width: area.width.saturating_sub(horizontal.saturating_mul(2)),
        height: area.height.saturating_sub(vertical.saturating_mul(2)),
    }
}

fn truncate_middle(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    if max_len <= 5 {
        return value.chars().take(max_len).collect();
    }
    let left = (max_len - 3) / 2;
    let right = max_len - 3 - left;
    let prefix: String = value.chars().take(left).collect();
    let suffix: String = value
        .chars()
        .rev()
        .take(right)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{}...{}", prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn search_bar_pulse_segments_preserve_width() {
        for width in 1..=SEARCH_BAR_WIDTH {
            for tick in 0..100 {
                let (before, pulse, after) = App::search_bar_pulse_segments(width, tick);

                assert_eq!(before + pulse + after, width);
                assert!(pulse <= SEARCH_BAR_PULSE_WIDTH);
                assert!(pulse > 0);
            }
        }
    }

    #[test]
    fn search_bar_pulse_bounces_between_edges() {
        let width = 12;
        let pulse = SEARCH_BAR_PULSE_WIDTH.min(width);
        let travel = width - pulse;

        assert_eq!(App::search_bar_pulse_segments(width, 0), (0, pulse, travel));
        assert_eq!(
            App::search_bar_pulse_segments(width, travel),
            (travel, pulse, 0)
        );
        assert_eq!(
            App::search_bar_pulse_segments(width, travel + 1),
            (travel - 1, pulse, 1)
        );
    }

    #[test]
    fn scan_updates_keep_default_focus_on_first_row() {
        let mut app = test_app();

        app.add_scan_artifact(test_artifact(PathBuf::from("/tmp/small"), 10));
        assert_eq!(app.state.selected(), Some(0));

        app.add_scan_artifact(test_artifact(PathBuf::from("/tmp/large"), 100));
        assert_eq!(app.state.selected(), Some(0));
        assert_eq!(app.artifacts[0].path, PathBuf::from("/tmp/large"));
    }

    #[test]
    fn scan_updates_preserve_user_moved_focus() {
        let mut app = test_app();
        let small = PathBuf::from("/tmp/small");

        app.add_scan_artifact(test_artifact(small.clone(), 10));
        app.focus_pinned_by_user = true;
        app.add_scan_artifact(test_artifact(PathBuf::from("/tmp/large"), 100));

        assert_eq!(app.state.selected(), Some(1));
        assert_eq!(app.focused_path(), Some(small));
    }

    #[test]
    fn delete_selected_marks_items_without_rescan_or_removing_rows() {
        let root = temp_root("delete-list");
        let keep_path = root.join("keep/node_modules");
        let delete_path = root.join("delete/node_modules");
        fs::create_dir_all(&keep_path).unwrap();
        fs::create_dir_all(&delete_path).unwrap();
        fs::write(keep_path.join("index.js"), "keep").unwrap();
        fs::write(delete_path.join("index.js"), "delete").unwrap();

        let mut app = test_app();
        app.artifacts = vec![
            test_artifact(keep_path.clone(), 10),
            test_artifact(delete_path.clone(), 20),
        ];
        app.sort_artifacts();
        app.state.select(Some(0));
        app.selected.insert(delete_path.clone());

        app.delete_selected();

        assert!(!delete_path.exists());
        assert!(keep_path.exists());
        assert_eq!(app.artifacts.len(), 2);
        assert!(app.deleted.contains(&delete_path));
        assert!(app.selected.is_empty());
        assert!(!app.scanning);
        assert_eq!(app.releasable_size(), 10);
        // bytes_removed uses on-disk remeasure, not the fixture size field
        assert!(app.deleted_size >= 1);
        assert!(!app.deleted.contains(&keep_path));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn select_all_safe_skips_caution_and_deleted_items() {
        let safe_path = PathBuf::from("/tmp/safe");
        let caution_path = PathBuf::from("/tmp/caution");
        let deleted_safe_path = PathBuf::from("/tmp/deleted-safe");
        let mut app = test_app();
        app.artifacts = vec![
            test_artifact(safe_path.clone(), 10),
            test_artifact_with_risk(caution_path.clone(), 20, RiskLevel::Caution),
            test_artifact(deleted_safe_path.clone(), 30),
        ];
        app.deleted.insert(deleted_safe_path.clone());

        app.select_all_safe();

        assert!(app.selected.contains(&safe_path));
        assert!(!app.selected.contains(&caution_path));
        assert!(!app.selected.contains(&deleted_safe_path));
    }

    #[test]
    fn space_can_select_caution_but_not_danger_or_deleted_rows() {
        let caution_path = PathBuf::from("/tmp/caution");
        let danger_path = PathBuf::from("/tmp/danger");
        let deleted_path = PathBuf::from("/tmp/deleted");
        let mut app = test_app();
        app.artifacts = vec![
            test_artifact_with_risk(caution_path.clone(), 30, RiskLevel::Caution),
            test_artifact_with_risk(danger_path.clone(), 20, RiskLevel::Danger),
            test_artifact(deleted_path.clone(), 10),
        ];
        app.sort_artifacts();

        app.state.select(Some(0));
        app.toggle_selected();
        assert!(app.selected.contains(&caution_path));

        app.state.select(Some(1));
        app.toggle_selected();
        assert!(!app.selected.contains(&danger_path));

        app.deleted.insert(deleted_path.clone());
        app.state.select(Some(2));
        app.toggle_selected();
        assert!(!app.selected.contains(&deleted_path));
    }

    #[test]
    fn category_filter_separates_agent_and_agent_cache() {
        let mut app = test_app();
        app.artifacts = vec![
            test_artifact_with_category(PathBuf::from("/a"), 10, "AGENT"),
            test_artifact_with_category(PathBuf::from("/b"), 20, "AGENT_CACHE"),
            test_artifact_with_category(PathBuf::from("/c"), 30, "CACHE"),
        ];
        app.filter = CategoryFilter::Agent;
        assert_eq!(app.visible_indices().len(), 1);
        app.filter = CategoryFilter::AgentCache;
        assert_eq!(app.visible_indices().len(), 1);
        app.filter = CategoryFilter::Cache;
        assert_eq!(app.visible_indices().len(), 1);
    }

    fn test_app() -> App {
        let mut state = TableState::default();
        state.select(None);
        App {
            paths: Vec::new(),
            artifacts: Vec::new(),
            selected: HashSet::new(),
            deleted: HashSet::new(),
            deleted_size: 0,
            state,
            scan_receiver: None,
            scanning: false,
            scan_started_at: Instant::now(),
            scanned_dirs: 0,
            completed_search_tasks: 0,
            pending_search_tasks: 0,
            completed_stats_calculations: 0,
            pending_stats_calculations: 0,
            current_path: None,
            detail: false,
            confirm_delete: false,
            message: None,
            filter: CategoryFilter::All,
            risk_filter: RiskFilter::All,
            min_size: None,
            min_size_index: 0,
            sort_mode: SortMode::SizeDesc,
            focus_pinned_by_user: false,
            scan_errors: 0,
        }
    }

    fn test_artifact(path: PathBuf, size: u64) -> Artifact {
        test_artifact_with_risk(path, size, RiskLevel::Safe)
    }

    fn test_artifact_with_risk(path: PathBuf, size: u64, risk_level: RiskLevel) -> Artifact {
        Artifact {
            path,
            size,
            size_human: human_size(size),
            last_modified: None,
            category: "NODE".to_string(),
            risk_level,
            reason: "test".to_string(),
            project_name: None,
            is_agent_worktree: false,
            git_status_clean: None,
            dangerous_files_detected: Vec::new(),
        }
    }

    fn test_artifact_with_category(path: PathBuf, size: u64, category: &str) -> Artifact {
        let mut artifact = test_artifact(path, size);
        artifact.category = category.to_string();
        artifact
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("agent-gc-tui-{name}-{nanos}"))
    }
}
