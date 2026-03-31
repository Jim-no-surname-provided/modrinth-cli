use std::cmp;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::minecraft::DetectedEnvironment;
use crate::modrinth::{CacheKind, CacheSettings, ModrinthClient, SearchHit};
use crate::search::{SearchCandidate, rank_candidates};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallerSelection {
    pub lookup_key: String,
    pub slug: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicInstallerOutcome {
    Cancelled,
    Install(Vec<InstallerSelection>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Search,
    Results,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchResult {
    lookup_key: String,
    slug: String,
    title: String,
    downloads: u64,
    installed_label: Option<String>,
}

struct SearchUpdate {
    query: String,
    result: std::result::Result<Vec<SearchResult>, String>,
}

pub fn run_dynamic_installer(
    client: &ModrinthClient,
    environment: &DetectedEnvironment,
    installed_project_labels: HashMap<String, String>,
) -> Result<DynamicInstallerOutcome> {
    let mut terminal = ratatui::try_init()?;
    let result = run_app(&mut terminal, client, environment, installed_project_labels);
    ratatui::try_restore()?;
    result
}

pub fn run_cache_settings_editor(initial: &CacheSettings) -> Result<Option<CacheSettings>> {
    let mut terminal = ratatui::try_init()?;
    let result = run_cache_settings_app(&mut terminal, initial);
    ratatui::try_restore()?;
    result
}

fn run_app(
    terminal: &mut DefaultTerminal,
    client: &ModrinthClient,
    environment: &DetectedEnvironment,
    installed_project_labels: HashMap<String, String>,
) -> Result<DynamicInstallerOutcome> {
    let mut app = DynamicInstallerApp::new(client, environment, installed_project_labels);

    loop {
        app.tick();
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(EVENT_POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if is_actionable_key(&key) => {
                    if let Some(outcome) = app.handle_key(key) {
                        return Ok(outcome);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn is_actionable_key(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn run_cache_settings_app(
    terminal: &mut DefaultTerminal,
    initial: &CacheSettings,
) -> Result<Option<CacheSettings>> {
    let mut app = CacheSettingsApp::new(initial.clone());

    loop {
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(EVENT_POLL_INTERVAL)? {
            match event::read()? {
                Event::Key(key) if is_actionable_key(&key) => {
                    if let Some(outcome) = app.handle_key(key) {
                        return Ok(match outcome {
                            CacheSettingsOutcome::Cancelled => None,
                            CacheSettingsOutcome::Saved(settings) => Some(settings),
                        });
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheSettingsOutcome {
    Cancelled,
    Saved(CacheSettings),
}

struct CacheSettingsApp {
    settings: CacheSettings,
    list_state: ListState,
    status: String,
}

impl CacheSettingsApp {
    fn new(settings: CacheSettings) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            settings,
            list_state,
            status: "Space toggles the highlighted cache group. Enter saves and exits. Esc cancels.".to_string(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<CacheSettingsOutcome> {
        match key.code {
            KeyCode::Esc => Some(CacheSettingsOutcome::Cancelled),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(CacheSettingsOutcome::Cancelled)
            }
            KeyCode::Up => {
                self.select_previous();
                None
            }
            KeyCode::Down => {
                self.select_next();
                None
            }
            KeyCode::Home => {
                self.list_state.select(Some(0));
                None
            }
            KeyCode::End => {
                self.list_state.select(Some(CacheKind::ALL.len().saturating_sub(1)));
                None
            }
            KeyCode::Char(' ') => {
                self.toggle_current();
                None
            }
            KeyCode::Enter => Some(CacheSettingsOutcome::Saved(self.settings.clone())),
            _ => None,
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let layout = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(10),
            Constraint::Length(5),
        ])
        .split(frame.area());

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Cache Settings",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "Choose exactly which Modrinth responses stay on disk.",
                    Style::default().fg(Color::Gray),
                ),
            ]),
            Line::from(Span::styled(
                "Each row controls one cache category. Disabled rows stop future reads and writes for that category.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title("Policy"));
        frame.render_widget(header, layout[0]);

        let items: Vec<ListItem> = CacheKind::ALL
            .iter()
            .map(|kind| {
                let checked = self.settings.is_enabled(*kind);
                let marker = if checked { "[x]" } else { "[ ]" };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            marker,
                            Style::default().fg(if checked { Color::Green } else { Color::DarkGray }),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            kind.label(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(Span::styled(
                        kind.description(),
                        Style::default().fg(Color::Gray),
                    )),
                ])
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Cached Endpoints")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .highlight_symbol(">> ")
            .highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, layout[1], &mut self.list_state);

        let footer = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Up/Down", Style::default().fg(Color::Cyan)),
                Span::raw(" move   "),
                Span::styled("Space", Style::default().fg(Color::Cyan)),
                Span::raw(" toggle   "),
                Span::styled("Enter", Style::default().fg(Color::Cyan)),
                Span::raw(" save and quit   "),
                Span::styled("Esc", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel"),
            ]),
            Line::from(self.status.as_str()),
        ])
        .block(Block::default().borders(Borders::ALL).title("Controls"))
        .wrap(Wrap { trim: true });
        frame.render_widget(Clear, layout[2]);
        frame.render_widget(footer, layout[2]);
    }

    fn toggle_current(&mut self) {
        let Some(kind) = self.current_kind() else {
            return;
        };
        let next = !self.settings.is_enabled(kind);
        self.settings.set_enabled(kind, next);
        self.status = format!(
            "{} caching {}.",
            kind.label(),
            if next { "enabled" } else { "disabled" }
        );
    }

    fn current_kind(&self) -> Option<CacheKind> {
        self.list_state
            .selected()
            .and_then(|index| CacheKind::ALL.get(index).copied())
    }

    fn select_next(&mut self) {
        let next = match self.list_state.selected() {
            Some(index) => cmp::min(index + 1, CacheKind::ALL.len().saturating_sub(1)),
            None => 0,
        };
        self.list_state.select(Some(next));
    }

    fn select_previous(&mut self) {
        let previous = self.list_state.selected().unwrap_or(0).saturating_sub(1);
        self.list_state.select(Some(previous));
    }
}

struct DynamicInstallerApp<'a> {
    client: &'a ModrinthClient,
    environment: &'a DetectedEnvironment,
    focus: Focus,
    query: String,
    results: Vec<SearchResult>,
    selected_projects: Vec<InstallerSelection>,
    list_state: ListState,
    status: String,
    search_cache: HashMap<String, Vec<SearchResult>>,
    installed_project_labels: HashMap<String, String>,
    in_flight_queries: HashSet<String>,
    pending_query: Option<String>,
    search_tx: Sender<SearchUpdate>,
    search_rx: Receiver<SearchUpdate>,
    last_query_edit_at: Option<Instant>,
}

impl<'a> DynamicInstallerApp<'a> {
    fn new(
        client: &'a ModrinthClient,
        environment: &'a DetectedEnvironment,
        installed_project_labels: HashMap<String, String>,
    ) -> Self {
        let (search_tx, search_rx) = mpsc::channel();
        Self {
            client,
            environment,
            focus: Focus::Search,
            query: String::new(),
            results: Vec::new(),
            selected_projects: Vec::new(),
            list_state: ListState::default(),
            status: default_status().to_string(),
            search_cache: HashMap::new(),
            installed_project_labels,
            in_flight_queries: HashSet::new(),
            pending_query: None,
            search_tx,
            search_rx,
            last_query_edit_at: None,
        }
    }

    fn tick(&mut self) {
        self.receive_search_update();
        self.maybe_start_search();
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<DynamicInstallerOutcome> {
        match key.code {
            KeyCode::Esc => return Some(DynamicInstallerOutcome::Cancelled),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(DynamicInstallerOutcome::Cancelled);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Search => Focus::Results,
                    Focus::Results => Focus::Search,
                };
                self.update_focus_status();
                return None;
            }
            KeyCode::Enter => {
                if self.selected_projects.is_empty() {
                    self.status =
                        "Pick at least one mod with Space before pressing Enter.".to_string();
                    return None;
                }

                return Some(DynamicInstallerOutcome::Install(
                    self.selected_projects.clone(),
                ));
            }
            _ => {}
        }

        match self.focus {
            Focus::Search => self.handle_search_key(key),
            Focus::Results => self.handle_results_key(key),
        }

        None
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(ch);
                self.schedule_search_refresh();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.schedule_search_refresh();
            }
            _ => {}
        }
    }

    fn handle_results_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.select_previous(),
            KeyCode::Down => self.select_next(),
            KeyCode::Home => self.select_first(),
            KeyCode::End => self.select_last(),
            KeyCode::Char(' ') => self.toggle_current_selection(),
            _ => {}
        }
    }

    fn schedule_search_refresh(&mut self) {
        let query = self.query.trim().to_string();
        if query.is_empty() {
            self.results.clear();
            self.list_state.select(None);
            self.status = default_status().to_string();
            self.pending_query = None;
            self.last_query_edit_at = None;
            return;
        }

        if let Some(results) = self.search_cache.get(&query).cloned() {
            self.pending_query = None;
            self.last_query_edit_at = None;
            self.apply_search_results(query, Ok(results));
            return;
        }

        let reused_cached_results = self.apply_cached_prefix_results(&query);

        if self.in_flight_queries.contains(&query) {
            self.pending_query = None;
            self.last_query_edit_at = None;
            if !reused_cached_results {
                self.status = format!("Searching `{query}`...");
            }
            return;
        }

        self.pending_query = Some(query.clone());
        self.last_query_edit_at = Some(Instant::now());
        if !reused_cached_results {
            self.status = format!("Searching `{query}`...");
        }
    }

    fn maybe_start_search(&mut self) {
        let Some(query) = self.pending_query.clone() else {
            return;
        };

        if self.search_cache.contains_key(&query) {
            self.pending_query = None;
            self.last_query_edit_at = None;
            return;
        }

        if self.in_flight_queries.contains(&query) {
            self.pending_query = None;
            self.last_query_edit_at = None;
            return;
        }

        let Some(last_edit_at) = self.last_query_edit_at else {
            return;
        };

        if last_edit_at.elapsed() < SEARCH_DEBOUNCE {
            return;
        }

        let client = self.client.clone();
        let environment = self.environment.clone();
        let installed_project_labels = self.installed_project_labels.clone();
        let worker_query = query.clone();
        let tx = self.search_tx.clone();
        self.in_flight_queries.insert(query);

        thread::spawn(move || {
            let result = client
                .search_projects(&worker_query, Some(&environment))
                .map(|hits| {
                    rank_search_hits_with_installed(
                        &worker_query,
                        &hits,
                        &installed_project_labels,
                    )
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(SearchUpdate {
                query: worker_query,
                result,
            });
        });

        self.pending_query = None;
        self.last_query_edit_at = None;
    }

    fn receive_search_update(&mut self) {
        loop {
            match self.search_rx.try_recv() {
                Ok(update) => {
                    self.in_flight_queries.remove(&update.query);
                    let current_query = self.query.trim().to_string();
                    let update_query = update.query.clone();
                    match update.result {
                        Ok(results) => {
                            self.search_cache
                                .insert(update_query.clone(), results.clone());
                            if current_query == update_query {
                                self.apply_search_results(update_query, Ok(results));
                            } else if !current_query.is_empty()
                                && current_query.starts_with(update_query.as_str())
                                && !self.search_cache.contains_key(&current_query)
                            {
                                self.apply_cached_prefix_results(&current_query);
                            }
                        }
                        Err(error) => {
                            if current_query == update_query {
                                self.apply_search_results(update_query, Err(error));
                            }
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn apply_cached_prefix_results(&mut self, query: &str) -> bool {
        let Some((cached_query, cached_results)) = self.best_cached_prefix_results(query) else {
            return false;
        };
        let cached_query = cached_query.to_string();

        let mut results_by_key: HashMap<String, SearchResult> = cached_results
            .iter()
            .cloned()
            .map(|result| (result.lookup_key.clone(), result))
            .collect();
        let candidates: Vec<SearchCandidate> = cached_results
            .iter()
            .map(|result| SearchCandidate {
                key: result.lookup_key.clone(),
                slug: result.slug.clone(),
                title: result.title.clone(),
                downloads: result.downloads,
            })
            .collect();
        let ranked_results: Vec<SearchResult> = rank_candidates(query, &candidates)
            .into_iter()
            .filter_map(|candidate| results_by_key.remove(&candidate.key))
            .collect();
        if ranked_results.is_empty() {
            return false;
        }

        self.results = ranked_results;
        self.list_state
            .select((!self.results.is_empty()).then_some(0));
        self.status = format!("Using cached `{cached_query}` results while searching `{query}`...");
        true
    }

    fn best_cached_prefix_results(&self, query: &str) -> Option<(&str, &[SearchResult])> {
        self.search_cache
            .iter()
            .filter(|(cached_query, results)| {
                !results.is_empty()
                    && query.starts_with(cached_query.as_str())
                    && cached_query.len() < query.len()
            })
            .max_by_key(|(cached_query, _)| cached_query.len())
            .map(|(cached_query, results)| (cached_query.as_str(), results.as_slice()))
    }

    fn apply_search_results(
        &mut self,
        query: String,
        result: std::result::Result<Vec<SearchResult>, String>,
    ) {
        match result {
            Ok(results) => {
                self.search_cache.insert(query.clone(), results.clone());
                if self.query.trim() != query {
                    return;
                }

                self.results = results;
                self.list_state
                    .select((!self.results.is_empty()).then_some(0));
                if self.results.is_empty() {
                    self.status = format!("No compatible mods found for `{query}` yet.");
                } else {
                    self.status = format!(
                        "{} result(s) for `{query}`. {} mod(s) queued for install.",
                        self.results.len(),
                        self.selected_projects.len()
                    );
                }
            }
            Err(error) => {
                if self.query.trim() != query {
                    return;
                }

                self.results.clear();
                self.list_state.select(None);
                self.status = format!("Search failed: {error}");
            }
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let layout = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(4),
        ])
        .split(frame.area());

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Dynamic Installer",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!(
                        "{} {} on Minecraft {}",
                        self.environment.loader,
                        self.environment
                            .loader_version
                            .as_deref()
                            .unwrap_or("unknown"),
                        self.environment.minecraft_version
                    ),
                    Style::default().fg(Color::Gray),
                ),
            ]),
            Line::from(Span::styled(
                "Live search in the top field, browse results below, then press Enter to install everything you selected.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title("Environment"));
        frame.render_widget(header, layout[0]);

        let search_border_style = if self.focus == Focus::Search {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let search_text = if self.query.is_empty() {
            Line::from(Span::styled(
                "Start typing a mod name...",
                Style::default().fg(Color::DarkGray),
            ))
        } else {
            Line::from(self.query.as_str())
        };
        let search = Paragraph::new(search_text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Search")
                    .border_style(search_border_style),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(search, layout[1]);

        let results_border_style = if self.focus == Focus::Results {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let results_block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                "Results ({})  Selected ({})",
                self.results.len(),
                self.selected_projects.len()
            ))
            .border_style(results_border_style);

        if self.results.is_empty() {
            let empty = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No results yet.",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    "Type in Search, press Tab to move here, and use Space to toggle the highlighted mod.",
                    Style::default().fg(Color::Gray),
                )),
            ])
            .block(results_block)
            .wrap(Wrap { trim: true });
            frame.render_widget(empty, layout[2]);
        } else {
            let items: Vec<ListItem> = self
                .results
                .iter()
                .map(|result| {
                    let is_selected = self.is_selected(&result.lookup_key);
                    let marker = if is_selected { "[x]" } else { "[ ]" };
                    let mut spans = vec![
                        Span::styled(
                            format!("{marker} "),
                            Style::default().fg(if is_selected {
                                Color::Green
                            } else {
                                Color::DarkGray
                            }),
                        ),
                        Span::styled(
                            result.title.as_str(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            format!("({})", result.slug),
                            Style::default().fg(Color::Gray),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            format!("{} downloads", format_download_count(result.downloads)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ];
                    if let Some(label) = result.installed_label.as_deref() {
                        spans.push(Span::raw("  "));
                        spans.push(Span::styled(label, Style::default().fg(Color::Green)));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();

            let list = List::new(items)
                .block(results_block)
                .highlight_symbol(">> ")
                .highlight_style(Style::default().bg(Color::DarkGray));
            frame.render_stateful_widget(list, layout[2], &mut self.list_state);
        }

        let mut footer_lines = vec![
            Line::from(vec![
                Span::styled("Tab", Style::default().fg(Color::Cyan)),
                Span::raw(" switch focus   "),
                Span::styled("Up/Down", Style::default().fg(Color::Cyan)),
                Span::raw(" move   "),
                Span::styled("Space", Style::default().fg(Color::Cyan)),
                Span::raw(" select   "),
                Span::styled("Enter", Style::default().fg(Color::Cyan)),
                Span::raw(" install   "),
                Span::styled("Esc", Style::default().fg(Color::Cyan)),
                Span::raw(" cancel"),
            ]),
            Line::from(self.status.as_str()),
        ];

        if !self.selected_projects.is_empty() {
            let preview = self
                .selected_projects
                .iter()
                .take(3)
                .map(|project| project.title.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if self.selected_projects.len() > 3 {
                format!(" +{} more", self.selected_projects.len() - 3)
            } else {
                String::new()
            };
            footer_lines.push(Line::from(vec![
                Span::styled("Queued: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{preview}{suffix}")),
            ]));
        }

        let footer = Paragraph::new(footer_lines)
            .block(Block::default().borders(Borders::ALL).title("Controls"))
            .wrap(Wrap { trim: true });
        frame.render_widget(Clear, layout[3]);
        frame.render_widget(footer, layout[3]);
    }

    fn is_selected(&self, lookup_key: &str) -> bool {
        self.selected_projects
            .iter()
            .any(|project| project.lookup_key == lookup_key)
    }

    fn toggle_current_selection(&mut self) {
        let Some(result) = self.current_result().cloned() else {
            return;
        };

        if let Some(index) = self
            .selected_projects
            .iter()
            .position(|project| project.lookup_key == result.lookup_key)
        {
            self.selected_projects.remove(index);
            self.status = format!(
                "Removed {} from the install queue. {} mod(s) still selected.",
                result.title,
                self.selected_projects.len()
            );
            return;
        }

        self.selected_projects.push(InstallerSelection {
            lookup_key: result.lookup_key,
            slug: result.slug,
            title: result.title.clone(),
        });
        self.status = format!(
            "Queued {} for install. {} mod(s) selected.",
            result.title,
            self.selected_projects.len()
        );
    }

    fn current_result(&self) -> Option<&SearchResult> {
        self.list_state
            .selected()
            .and_then(|index| self.results.get(index))
    }

    fn select_next(&mut self) {
        if self.results.is_empty() {
            self.list_state.select(None);
            return;
        }

        let next = match self.list_state.selected() {
            Some(index) => cmp::min(index + 1, self.results.len() - 1),
            None => 0,
        };
        self.list_state.select(Some(next));
    }

    fn select_previous(&mut self) {
        if self.results.is_empty() {
            self.list_state.select(None);
            return;
        }

        let previous = match self.list_state.selected() {
            Some(index) => index.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(previous));
    }

    fn select_first(&mut self) {
        self.list_state
            .select((!self.results.is_empty()).then_some(0));
    }

    fn select_last(&mut self) {
        self.list_state
            .select((!self.results.is_empty()).then_some(self.results.len() - 1));
    }

    fn update_focus_status(&mut self) {
        self.status = match self.focus {
            Focus::Search => "Search is focused. Type to update the mod list live.".to_string(),
            Focus::Results => {
                "Results are focused. Use Up/Down to scroll and Space to toggle mods.".to_string()
            }
        };
    }
}

fn default_status() -> &'static str {
    "Type a mod name to search. Press Tab to switch panes, Space to pick mods, Enter to install, and Esc to cancel."
}

fn rank_search_hits_with_installed(
    query: &str,
    hits: &[SearchHit],
    installed_project_labels: &HashMap<String, String>,
) -> Vec<SearchResult> {
    let mut hits_by_key = hits
        .iter()
        .map(|hit| {
            (
                hit.project_id.clone().unwrap_or_else(|| hit.slug.clone()),
                hit.clone(),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    let candidates: Vec<SearchCandidate> = hits
        .iter()
        .map(|hit| SearchCandidate {
            key: hit.project_id.clone().unwrap_or_else(|| hit.slug.clone()),
            slug: hit.slug.clone(),
            title: hit.title.clone(),
            downloads: hit.downloads,
        })
        .collect();

    rank_candidates(query, &candidates)
        .into_iter()
        .filter_map(|candidate| {
            hits_by_key.remove(&candidate.key).map(|hit| SearchResult {
                lookup_key: candidate.key,
                installed_label: lookup_installed_label(
                    installed_project_labels,
                    hit.project_id.as_deref(),
                    &hit.slug,
                ),
                slug: hit.slug,
                title: hit.title,
                downloads: hit.downloads,
            })
        })
        .collect()
}

fn lookup_installed_label(
    installed_project_labels: &HashMap<String, String>,
    project_id: Option<&str>,
    slug: &str,
) -> Option<String> {
    project_id
        .and_then(|id| installed_project_labels.get(id))
        .or_else(|| installed_project_labels.get(slug))
        .cloned()
}

fn format_download_count(downloads: u64) -> String {
    let digits = downloads.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);

    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(',');
        }
        reversed.push(ch);
    }

    reversed.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::minecraft::Loader;
    use httpmock::Method::GET;
    use httpmock::MockServer;
    use serde_json::json;

    fn environment() -> DetectedEnvironment {
        DetectedEnvironment {
            loader: Loader::Fabric,
            loader_version: Some("0.16.10".to_string()),
            minecraft_version: "1.21.1".to_string(),
        }
    }

    #[test]
    fn ranking_keeps_best_matches_first() {
        let hits = vec![
            SearchHit {
                project_id: Some("1".to_string()),
                slug: "tooltipfix".to_string(),
                title: "Tooltip Fix".to_string(),
                downloads: 3_000,
            },
            SearchHit {
                project_id: Some("2".to_string()),
                slug: "shulkerboxtooltips".to_string(),
                title: "ShulkerBox Tooltips".to_string(),
                downloads: 1_000,
            },
        ];

        let ranked = rank_search_hits_with_installed("shulker tooltips", &hits, &HashMap::new());

        assert_eq!(ranked[0].slug, "shulkerboxtooltips");
        assert_eq!(ranked[1].slug, "tooltipfix");
    }

    #[test]
    fn narrowing_query_reuses_cached_prefix_results_immediately() {
        let server = MockServer::start();
        let client = ModrinthClient::new(server.base_url()).unwrap();
        let environment = environment();
        let mut app = DynamicInstallerApp::new(&client, &environment, HashMap::new());
        app.search_cache.insert(
            "sod".to_string(),
            vec![
                SearchResult {
                    lookup_key: "1".to_string(),
                    slug: "sodium-extra".to_string(),
                    title: "Sodium Extra".to_string(),
                    downloads: 200_000,
                    installed_label: None,
                },
                SearchResult {
                    lookup_key: "2".to_string(),
                    slug: "sodium".to_string(),
                    title: "Sodium".to_string(),
                    downloads: 100_000,
                    installed_label: None,
                },
            ],
        );
        app.query = "sodium".to_string();

        app.schedule_search_refresh();

        assert_eq!(app.results[0].slug, "sodium");
        assert_eq!(app.pending_query.as_deref(), Some("sodium"));
        assert!(app.status.contains("Using cached `sod` results"));
    }

    #[test]
    fn new_query_can_start_while_another_search_is_in_flight() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search");
            then.status(200).json_body(json!({ "hits": [] }));
        });

        let client = ModrinthClient::new(server.base_url()).unwrap();
        let environment = environment();
        let mut app = DynamicInstallerApp::new(&client, &environment, HashMap::new());
        app.in_flight_queries.insert("sod".to_string());
        app.pending_query = Some("sodium".to_string());
        app.last_query_edit_at = Some(Instant::now() - SEARCH_DEBOUNCE);

        app.maybe_start_search();

        assert!(app.in_flight_queries.contains("sod"));
        assert!(app.in_flight_queries.contains("sodium"));
        assert!(app.pending_query.is_none());
    }

    #[test]
    fn stale_prefix_search_results_seed_a_faster_preview_for_newer_queries() {
        let server = MockServer::start();
        let client = ModrinthClient::new(server.base_url()).unwrap();
        let environment = environment();
        let mut app = DynamicInstallerApp::new(&client, &environment, HashMap::new());
        app.query = "sodium extra".to_string();
        app.results = vec![SearchResult {
            lookup_key: "live".to_string(),
            slug: "sodium-extra".to_string(),
            title: "Sodium Extra".to_string(),
            downloads: 200_000,
            installed_label: None,
        }];

        app.search_tx
            .send(SearchUpdate {
                query: "sodium".to_string(),
                result: Ok(vec![SearchResult {
                    lookup_key: "cached".to_string(),
                    slug: "sodium".to_string(),
                    title: "Sodium".to_string(),
                    downloads: 100_000,
                    installed_label: None,
                }]),
            })
            .unwrap();
        app.receive_search_update();

        assert_eq!(app.results[0].slug, "sodium");
        assert!(app.search_cache.contains_key("sodium"));
        assert!(app.status.contains("Using cached `sodium` results"));
    }

    #[test]
    fn ranking_marks_results_that_are_already_downloaded() {
        let hits = vec![SearchHit {
            project_id: Some("proj-sodium".to_string()),
            slug: "sodium".to_string(),
            title: "Sodium".to_string(),
            downloads: 100_000,
        }];
        let installed = HashMap::from([(
            "proj-sodium".to_string(),
            "Already downloaded (2.0.0)".to_string(),
        )]);

        let ranked = rank_search_hits_with_installed("sodium", &hits, &installed);

        assert_eq!(
            ranked[0].installed_label.as_deref(),
            Some("Already downloaded (2.0.0)")
        );
    }

    #[test]
    fn cache_settings_app_toggles_current_row_with_space() {
        let mut app = CacheSettingsApp::new(CacheSettings::default());

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        assert!(!app.settings.game_versions);
        assert!(app.status.contains("Game version tags"));
    }

    #[test]
    fn cache_settings_app_moves_and_saves() {
        let mut app = CacheSettingsApp::new(CacheSettings::default());

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let outcome = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match outcome {
            Some(CacheSettingsOutcome::Saved(settings)) => {
                assert!(!settings.projects);
                assert!(settings.game_versions);
            }
            _ => panic!("expected saved cache settings"),
        }
    }
}
