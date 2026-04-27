//! Application state, focus management, input handling, and request actions.

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::Backend};
use spark_core::{
    config::Config,
    history::{HistoryEntry, append_history, load_history},
    http::{HttpMethod, HttpRequest, HttpResponse},
    saved::{SavedRequest, load_saved_requests, remove_saved_request, upsert_saved_request},
};

use crate::input::TextInput;

/// The element that currently receives keyboard input.
#[derive(Debug, PartialEq, Eq)]
pub enum Focus {
    /// Request history sidebar.
    History,
    /// Request history search field.
    Search,
    /// HTTP method selector.
    Method,
    /// URL input field.
    Url,
    /// Headers text area.
    Headers,
    /// Body text area.
    Body,
    /// Response viewer.
    Response,
}

/// Selected tab in the response pane.
#[derive(Debug, PartialEq, Eq)]
pub enum ResponseTab {
    /// Shows status, headers, and body content.
    Body,
    /// Shows request and response byte sizes.
    Sizes,
    /// Shows response code distribution across history.
    History,
}

/// Active collection shown in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    /// Show request history.
    History,
    /// Show saved reusable requests.
    Saved,
}

/// Which text area a generic key handler should target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextAreaTarget {
    /// Headers editor.
    Headers,
    /// Body editor.
    Body,
}

/// Complete application state.
pub struct App {
    /// Application configuration.
    pub config: Config,
    /// Currently focused element.
    pub focus: Focus,
    /// Index into [`HttpMethod::all()`] for the selected method.
    pub method_index: usize,
    /// URL input.
    pub url: TextInput,
    /// Headers input (one `Key: Value` per line).
    pub headers: TextInput,
    /// Request body input.
    pub body: TextInput,
    /// Request history search input.
    pub history_search: TextInput,
    /// Loaded request history (oldest first).
    pub history: Vec<HistoryEntry>,
    /// Currently selected row in the history list.
    pub history_index: usize,
    /// Saved reusable requests.
    pub saved_requests: Vec<SavedRequest>,
    /// Currently selected row in the saved request list.
    pub saved_index: usize,
    /// Active sidebar collection.
    pub sidebar_mode: SidebarMode,
    /// Most recent HTTP response.
    pub response: Option<HttpResponse>,
    /// Request that produced the most recent response.
    pub last_request: Option<HttpRequest>,
    /// Request waiting for a painted "sending" frame before execution.
    pending_request: Option<HttpRequest>,
    /// Active tab in the response pane.
    pub response_tab: ResponseTab,
    /// Vertical scroll offset for the response viewer.
    pub response_scroll: u16,
    /// Set to `true` to exit the event loop.
    pub should_quit: bool,
    /// One-line message shown in the status bar.
    pub status_message: String,
}

impl App {
    /// Creates a new [`App`], loading history from the path in `config`.
    pub fn new(config: Config) -> Self {
        let history = load_history(&config.history_file);
        let history_index = history.len().saturating_sub(1);
        let saved_requests = load_saved_requests(&config.saved_requests_file);
        let saved_index = saved_requests.len().saturating_sub(1);
        Self {
            config,
            focus: Focus::Url,
            method_index: 0,
            url: TextInput::single_line(),
            headers: TextInput::multi_line(),
            body: TextInput::multi_line(),
            history_search: TextInput::single_line(),
            history,
            history_index,
            saved_requests,
            saved_index,
            sidebar_mode: SidebarMode::History,
            response: None,
            last_request: None,
            pending_request: None,
            response_tab: ResponseTab::Body,
            response_scroll: 0,
            should_quit: false,
            status_message: String::from(
                "Tab: cycle focus | Ctrl+S: send | Ctrl+P: save | Ctrl+O: saved/history",
            ),
        }
    }

    /// Runs the event loop, drawing a frame after every key event.
    ///
    /// # Errors
    /// Propagates terminal I/O errors.
    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        loop {
            terminal.draw(|f| crate::ui::render(f, self))?;

            if event::poll(std::time::Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
            {
                self.handle_key(key);
            }

            if self.pending_request.is_some() {
                terminal.draw(|f| crate::ui::render(f, self))?;
                self.execute_pending_request();
            }

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// Dispatches a key event to the appropriate handler.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Global shortcuts regardless of focus.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('s') => {
                    self.send_request();
                    return;
                }
                KeyCode::Char('p') => {
                    self.save_current_request();
                    return;
                }
                KeyCode::Char('o') => {
                    self.toggle_sidebar_mode();
                    return;
                }
                _ => {}
            }
        }

        match self.focus {
            Focus::History => self.handle_history_key(key),
            Focus::Search => self.handle_search_key(key),
            Focus::Method => self.handle_method_key(key),
            Focus::Url => self.handle_url_key(key),
            Focus::Headers => self.handle_text_area_key(key, TextAreaTarget::Headers),
            Focus::Body => self.handle_text_area_key(key, TextAreaTarget::Body),
            Focus::Response => self.handle_response_key(key),
        }
    }

    /// Returns the currently selected [`HttpMethod`].
    pub fn current_method(&self) -> &HttpMethod {
        &HttpMethod::all()[self.method_index]
    }

    /// Returns whether a request is queued or currently being started.
    #[must_use]
    pub fn is_sending(&self) -> bool {
        self.pending_request.is_some()
    }

    /// Returns indexes of history entries matching the active search query.
    #[must_use]
    pub fn filtered_history_indices(&self) -> Vec<usize> {
        let query = self.history_search.text();
        let query = query.trim();

        self.history
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| history_matches(entry, query).then_some(idx))
            .collect()
    }

    /// Returns indexes of saved requests matching the active search query.
    #[must_use]
    pub fn filtered_saved_indices(&self) -> Vec<usize> {
        let query = self.history_search.text();
        let query = query.trim();

        self.saved_requests
            .iter()
            .enumerate()
            .filter_map(|(idx, request)| saved_request_matches(request, query).then_some(idx))
            .collect()
    }

    // ── Focus cycling ────────────────────────────────────────────────────────
    /// Moves focus to the next pane in tab order.
    fn next_focus(&mut self) {
        self.focus = match self.focus {
            Focus::History => Focus::Search,
            Focus::Search => Focus::Method,
            Focus::Method => Focus::Url,
            Focus::Url => Focus::Headers,
            Focus::Headers => Focus::Body,
            Focus::Body => Focus::Response,
            Focus::Response => Focus::History,
        };
    }

    /// Moves focus to the previous pane in tab order.
    fn prev_focus(&mut self) {
        self.focus = match self.focus {
            Focus::History => Focus::Response,
            Focus::Search => Focus::History,
            Focus::Method => Focus::Search,
            Focus::Url => Focus::Method,
            Focus::Headers => Focus::Url,
            Focus::Body => Focus::Headers,
            Focus::Response => Focus::Body,
        };
    }

    // ── Per-pane key handlers ────────────────────────────────────────────────

    /// Handles key input while the request history list is focused.
    fn handle_history_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next_visible_sidebar_item();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_previous_visible_sidebar_item();
            }
            KeyCode::Left | KeyCode::Char('h' | 'l') | KeyCode::Right => {
                self.toggle_sidebar_mode();
            }
            KeyCode::Enter => self.load_from_sidebar(),
            KeyCode::Delete | KeyCode::Backspace => self.remove_selected_saved_request(),
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    /// Handles key input while the request history search field is focused.
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                self.next_focus();
            }
            KeyCode::BackTab => {
                self.prev_focus();
            }
            KeyCode::Left => self.history_search.move_left(),
            KeyCode::Right => self.history_search.move_right(),
            KeyCode::Home => self.history_search.move_to_line_start(),
            KeyCode::End => self.history_search.move_to_line_end(),
            KeyCode::Backspace => {
                self.history_search.backspace();
                self.select_latest_visible_sidebar_item();
            }
            KeyCode::Char(c) => {
                self.history_search.insert_char(c);
                self.select_latest_visible_sidebar_item();
            }
            _ => {}
        }
    }

    /// Handles key input while the HTTP method selector is focused.
    fn handle_method_key(&mut self, key: KeyEvent) {
        let count = HttpMethod::all().len();
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
            KeyCode::Left | KeyCode::Char('h') => {
                self.method_index = if self.method_index == 0 {
                    count - 1
                } else {
                    self.method_index - 1
                };
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.method_index = (self.method_index + 1) % count;
            }
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    /// Handles key input while the URL field is focused.
    fn handle_url_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                self.next_focus();
                return;
            }
            KeyCode::BackTab => {
                self.prev_focus();
                return;
            }
            KeyCode::Enter => {
                self.send_request();
                return;
            }
            _ => {}
        }
        match key.code {
            KeyCode::Left => self.url.move_left(),
            KeyCode::Right => self.url.move_right(),
            KeyCode::Home => self.url.move_to_line_start(),
            KeyCode::End => self.url.move_to_line_end(),
            KeyCode::Backspace => self.url.backspace(),
            KeyCode::Char(c) => self.url.insert_char(c),
            _ => {}
        }
    }

    /// Handles key input for the headers or body text area.
    fn handle_text_area_key(&mut self, key: KeyEvent, target: TextAreaTarget) {
        // Handle focus-change keys before borrowing the target area.
        match key.code {
            KeyCode::Tab => {
                self.next_focus();
                return;
            }
            KeyCode::BackTab => {
                self.prev_focus();
                return;
            }
            _ => {}
        }

        let area = match target {
            TextAreaTarget::Headers => &mut self.headers,
            TextAreaTarget::Body => &mut self.body,
        };

        match key.code {
            KeyCode::Enter => area.insert_newline(),
            KeyCode::Up => area.move_up(),
            KeyCode::Down => area.move_down(),
            KeyCode::Left => area.move_left(),
            KeyCode::Right => area.move_right(),
            KeyCode::Home => area.move_to_line_start(),
            KeyCode::End => area.move_to_line_end(),
            KeyCode::Backspace => area.backspace(),
            KeyCode::Char(c) => area.insert_char(c),
            _ => {}
        }
    }

    /// Handles key input while the response viewer is focused.
    fn handle_response_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
            KeyCode::Left | KeyCode::Char('h') => self.previous_response_tab(),
            KeyCode::Right | KeyCode::Char('l') => self.next_response_tab(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.response_scroll = self.response_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.response_scroll = self.response_scroll.saturating_sub(1);
            }
            KeyCode::Char('g') => self.response_scroll = 0,
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────────

    /// Toggles the sidebar between history and saved requests.
    fn toggle_sidebar_mode(&mut self) {
        self.sidebar_mode = match self.sidebar_mode {
            SidebarMode::History => SidebarMode::Saved,
            SidebarMode::Saved => SidebarMode::History,
        };
        self.select_latest_visible_sidebar_item();
    }

    /// Loads the selected sidebar item into the request composer.
    fn load_from_sidebar(&mut self) {
        match self.sidebar_mode {
            SidebarMode::History => self.load_from_history(),
            SidebarMode::Saved => self.load_from_saved_request(),
        }
    }

    /// Loads the selected history entry into the request composer.
    fn load_from_history(&mut self) {
        if !self
            .filtered_history_indices()
            .contains(&self.history_index)
        {
            self.select_latest_visible_history();
        }

        let Some(entry) = self.history.get(self.history_index) else {
            return;
        };

        let query = self.history_search.text();
        if !history_matches(entry, query.trim()) {
            return;
        }

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == entry.method) {
            self.method_index = idx;
        }

        self.url.set_content(&entry.url);

        let headers_text = format_headers(&entry.headers);
        self.headers.set_content(&headers_text);

        self.body.set_content(entry.body.as_deref().unwrap_or(""));

        self.focus = Focus::Url;
        self.status_message = format!("Loaded: {} {}", entry.method, entry.url);
    }

    /// Loads the selected saved request into the request composer.
    fn load_from_saved_request(&mut self) {
        if !self.filtered_saved_indices().contains(&self.saved_index) {
            self.select_latest_visible_saved_request();
        }

        let Some(request) = self.saved_requests.get(self.saved_index) else {
            return;
        };

        let query = self.history_search.text();
        if !saved_request_matches(request, query.trim()) {
            return;
        }

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == request.method) {
            self.method_index = idx;
        }

        self.url.set_content(&request.url);

        let headers_text = format_headers(&request.headers);
        self.headers.set_content(&headers_text);

        self.body.set_content(request.body.as_deref().unwrap_or(""));

        self.focus = Focus::Url;
        self.status_message = format!("Loaded saved: {}", request.name);
    }

    /// Saves the current composer contents as a reusable saved request.
    fn save_current_request(&mut self) {
        let Some(request) = self.current_composed_request() else {
            self.status_message = "URL is empty - enter a URL before saving.".to_string();
            return;
        };

        let saved = SavedRequest::from_request(&request);
        match upsert_saved_request(
            &self.config.saved_requests_file,
            &mut self.saved_requests,
            saved,
        ) {
            Ok(idx) => {
                self.saved_index = idx;
                self.sidebar_mode = SidebarMode::Saved;
                let name = &self.saved_requests[idx].name;
                self.status_message = format!("Saved request: {name}");
            }
            Err(e) => {
                self.status_message = format!("Error saving request: {e}");
            }
        }
    }

    /// Removes the selected saved request when the saved sidebar is active.
    fn remove_selected_saved_request(&mut self) {
        if self.sidebar_mode != SidebarMode::Saved {
            return;
        }

        if !self.filtered_saved_indices().contains(&self.saved_index) {
            self.select_latest_visible_saved_request();
        }

        match remove_saved_request(
            &self.config.saved_requests_file,
            &mut self.saved_requests,
            self.saved_index,
        ) {
            Ok(Some(removed)) => {
                self.saved_index = self
                    .saved_index
                    .min(self.saved_requests.len().saturating_sub(1));
                self.select_latest_visible_saved_request();
                self.status_message = format!("Removed saved request: {}", removed.name);
            }
            Ok(None) => {}
            Err(e) => {
                self.status_message = format!("Error removing saved request: {e}");
            }
        }
    }

    /// Queues the current request for execution after the sending state is rendered.
    pub fn send_request(&mut self) {
        let Some(request) = self.current_composed_request() else {
            self.status_message = "URL is empty — enter a URL and try again.".to_string();
            return;
        };
        self.focus = Focus::Response;
        self.response_tab = ResponseTab::Body;
        self.response_scroll = 0;
        self.status_message = format!("Sending {} {}…", request.method, request.url);
        self.pending_request = Some(request);
    }

    /// Executes a queued request, writing the result to history.
    fn execute_pending_request(&mut self) {
        let Some(request) = self.pending_request.take() else {
            return;
        };
        match request.execute() {
            Ok(response) => {
                let entry = HistoryEntry::from_response(&request, response.status_code);
                let _ = append_history(&self.config.history_file, &entry);
                self.status_message = format!(
                    "✓ {} {}  —  {}",
                    request.method, request.url, response.status_code
                );
                self.history.push(entry);
                self.history_index = self.history.len() - 1;
                self.select_latest_visible_sidebar_item();
                self.last_request = Some(request);
                self.response = Some(response);
            }
            Err(e) => {
                self.status_message = format!("Error: {e}");
            }
        }
    }

    /// Builds a request from the current composer fields.
    fn current_composed_request(&self) -> Option<HttpRequest> {
        let url_text = self.url.text();
        let url = url_text.trim();
        if url.is_empty() {
            return None;
        }

        let method = *self.current_method();
        let headers_text = self.headers.text();
        let headers = parse_headers(headers_text.as_ref());
        let body_text = self.body.text();
        let body = if body_text.trim().is_empty() {
            None
        } else {
            Some(body_text.into_owned())
        };

        Some(HttpRequest {
            method,
            url: url.to_string(),
            headers,
            body,
        })
    }

    /// Selects the next entry in the currently visible sidebar list.
    fn select_next_visible_sidebar_item(&mut self) {
        match self.sidebar_mode {
            SidebarMode::History => self.select_next_visible_history(),
            SidebarMode::Saved => self.select_next_visible_saved_request(),
        }
    }

    /// Selects the previous entry in the currently visible sidebar list.
    fn select_previous_visible_sidebar_item(&mut self) {
        match self.sidebar_mode {
            SidebarMode::History => self.select_previous_visible_history(),
            SidebarMode::Saved => self.select_previous_visible_saved_request(),
        }
    }

    /// Selects the next entry in the currently visible history list.
    fn select_next_visible_history(&mut self) {
        let visible = self.filtered_history_indices();
        let Some(current_pos) = visible.iter().position(|idx| *idx == self.history_index) else {
            self.select_latest_visible_history();
            return;
        };

        if let Some(next_idx) = visible.get(current_pos + 1) {
            self.history_index = *next_idx;
        }
    }

    /// Selects the previous entry in the currently visible history list.
    fn select_previous_visible_history(&mut self) {
        let visible = self.filtered_history_indices();
        let Some(current_pos) = visible.iter().position(|idx| *idx == self.history_index) else {
            self.select_latest_visible_history();
            return;
        };

        if current_pos > 0 {
            self.history_index = visible[current_pos - 1];
        }
    }

    /// Selects the newest history entry currently visible after filtering.
    fn select_latest_visible_history(&mut self) {
        if let Some(idx) = self.filtered_history_indices().last() {
            self.history_index = *idx;
        }
    }

    /// Selects the next entry in the currently visible saved request list.
    fn select_next_visible_saved_request(&mut self) {
        let visible = self.filtered_saved_indices();
        let Some(current_pos) = visible.iter().position(|idx| *idx == self.saved_index) else {
            self.select_latest_visible_saved_request();
            return;
        };

        if let Some(next_idx) = visible.get(current_pos + 1) {
            self.saved_index = *next_idx;
        }
    }

    /// Selects the previous entry in the currently visible saved request list.
    fn select_previous_visible_saved_request(&mut self) {
        let visible = self.filtered_saved_indices();
        let Some(current_pos) = visible.iter().position(|idx| *idx == self.saved_index) else {
            self.select_latest_visible_saved_request();
            return;
        };

        if current_pos > 0 {
            self.saved_index = visible[current_pos - 1];
        }
    }

    /// Selects the newest saved request currently visible after filtering.
    fn select_latest_visible_saved_request(&mut self) {
        if let Some(idx) = self.filtered_saved_indices().last() {
            self.saved_index = *idx;
        }
    }

    /// Selects the newest item currently visible after filtering.
    fn select_latest_visible_sidebar_item(&mut self) {
        match self.sidebar_mode {
            SidebarMode::History => self.select_latest_visible_history(),
            SidebarMode::Saved => self.select_latest_visible_saved_request(),
        }
    }

    /// Selects the next response pane tab.
    fn next_response_tab(&mut self) {
        self.response_tab = match self.response_tab {
            ResponseTab::Body => ResponseTab::Sizes,
            ResponseTab::Sizes => ResponseTab::History,
            ResponseTab::History => ResponseTab::Body,
        };
        self.response_scroll = 0;
    }

    /// Selects the previous response pane tab.
    fn previous_response_tab(&mut self) {
        self.response_tab = match self.response_tab {
            ResponseTab::Body => ResponseTab::History,
            ResponseTab::Sizes => ResponseTab::Body,
            ResponseTab::History => ResponseTab::Sizes,
        };
        self.response_scroll = 0;
    }
}

/// Parses raw header text (`Key: Value` per line) into key-value pairs.
fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let colon = l.find(':')?;
            Some((
                l[..colon].trim().to_string(),
                l[colon + 1..].trim().to_string(),
            ))
        })
        .collect()
}

/// Formats header pairs as one `Key: Value` line per header.
fn format_headers(headers: &[(String, String)]) -> String {
    let capacity = headers
        .iter()
        .map(|(key, value)| key.len() + ": ".len() + value.len() + "\n".len())
        .sum::<usize>()
        .saturating_sub(usize::from(!headers.is_empty()));
    let mut text = String::with_capacity(capacity);

    for (idx, (key, value)) in headers.iter().enumerate() {
        if idx > 0 {
            text.push('\n');
        }
        text.push_str(key);
        text.push_str(": ");
        text.push_str(value);
    }

    text
}

/// Returns whether a history entry matches the search query.
fn history_matches(entry: &HistoryEntry, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    contains_case_insensitive(entry.method.as_str(), query)
        || contains_case_insensitive(&entry.url, query)
        || check_headers(&entry.headers, query)
        || entry
            .body
            .as_deref()
            .is_some_and(|body| contains_case_insensitive(body, query))
}

/// Checks entry or request headers.
fn check_headers(headers: &[(String, String)], query: &str) -> bool {
    headers.iter().any(|(key, value)| {
        contains_case_insensitive(key, query) || contains_case_insensitive(value, query)
    })
}

/// Returns whether a saved request matches the search query.
fn saved_request_matches(request: &SavedRequest, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    contains_case_insensitive(&request.name, query)
        || contains_case_insensitive(request.method.as_str(), query)
        || contains_case_insensitive(&request.url, query)
        || check_headers(&request.headers, query)
        || request
            .body
            .as_deref()
            .is_some_and(|body| contains_case_insensitive(body, query))
}

/// Returns whether `haystack` contains `needle` without regard to case.
fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    if haystack.is_ascii() && needle.is_ascii() {
        let needle = needle.as_bytes();
        return haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle));
    }

    haystack.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    //! Tests for request history filtering and selection behavior.

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    /// Creates a mostly unique suffix for test files.
    fn test_id() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("current time should be after Unix epoch")
            .as_nanos();
        format!("{}-{nanos}", std::process::id())
    }

    /// Creates an app with deterministic in-memory history.
    fn app_with_history(history: Vec<HistoryEntry>) -> App {
        let test_id = test_id();
        let mut app = App::new(Config {
            history_file: std::env::temp_dir()
                .join(format!("spark-tui-test-history-{test_id}.jsonl")),
            saved_requests_file: std::env::temp_dir()
                .join(format!("spark-tui-test-saved-{test_id}.json")),
        });
        app.history = history;
        app.history_index = app.history.len().saturating_sub(1);
        app
    }

    /// Creates an app with deterministic in-memory saved requests.
    fn app_with_saved_requests(saved_requests: Vec<SavedRequest>) -> App {
        let mut app = app_with_history(Vec::new());
        app.saved_requests = saved_requests;
        app.saved_index = app.saved_requests.len().saturating_sub(1);
        app.sidebar_mode = SidebarMode::Saved;
        app
    }

    /// Creates a request history entry for tests.
    fn history_entry(
        method: HttpMethod,
        url: &str,
        headers: Vec<(String, String)>,
        body: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry::from_request(&HttpRequest {
            method,
            url: url.to_string(),
            headers,
            body: body.map(ToString::to_string),
        })
    }

    /// Creates a saved request for tests.
    fn saved_request(
        name: &str,
        method: HttpMethod,
        url: &str,
        body: Option<&str>,
    ) -> SavedRequest {
        let request = HttpRequest {
            method,
            url: url.to_string(),
            headers: Vec::new(),
            body: body.map(ToString::to_string),
        };
        let mut saved = SavedRequest::from_request(&request);
        saved.name = name.to_string();
        saved
    }

    /// Builds a plain key event for input handler tests.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Empty search returns every history entry.
    #[test]
    fn empty_history_search_shows_all_requests() {
        let app = app_with_history(vec![
            history_entry(
                HttpMethod::Get,
                "https://example.com/users",
                Vec::new(),
                None,
            ),
            history_entry(
                HttpMethod::Post,
                "https://example.com/orders",
                Vec::new(),
                None,
            ),
        ]);

        assert_eq!(app.filtered_history_indices(), vec![0, 1]);
    }

    /// Search matches method, URL, headers, and request body.
    #[test]
    fn history_search_matches_request_parts_case_insensitively() {
        let mut app = app_with_history(vec![
            history_entry(
                HttpMethod::Post,
                "https://example.com/orders",
                vec![("Authorization".to_string(), "Bearer token".to_string())],
                Some("{\"status\":\"pending\"}"),
            ),
            history_entry(
                HttpMethod::Get,
                "https://example.com/users",
                Vec::new(),
                None,
            ),
        ]);

        app.history_search.set_content("POST");
        assert_eq!(app.filtered_history_indices(), vec![0]);

        app.history_search.set_content("USERS");
        assert_eq!(app.filtered_history_indices(), vec![1]);

        app.history_search.set_content("bearer");
        assert_eq!(app.filtered_history_indices(), vec![0]);

        app.history_search.set_content("pending");
        assert_eq!(app.filtered_history_indices(), vec![0]);
    }

    /// Typing in the search field selects the newest matching request.
    #[test]
    fn search_input_selects_latest_visible_request() {
        let mut app = app_with_history(vec![
            history_entry(
                HttpMethod::Get,
                "https://example.com/users/1",
                Vec::new(),
                None,
            ),
            history_entry(
                HttpMethod::Get,
                "https://example.com/orders",
                Vec::new(),
                None,
            ),
            history_entry(
                HttpMethod::Get,
                "https://example.com/users/2",
                Vec::new(),
                None,
            ),
        ]);
        app.focus = Focus::Search;

        for c in "users".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }

        assert_eq!(app.filtered_history_indices(), vec![0, 2]);
        assert_eq!(app.history_index, 2);
    }

    /// History navigation moves only through visible filtered requests.
    #[test]
    fn history_navigation_uses_filtered_requests() {
        let mut app = app_with_history(vec![
            history_entry(
                HttpMethod::Get,
                "https://example.com/users/1",
                Vec::new(),
                None,
            ),
            history_entry(
                HttpMethod::Get,
                "https://example.com/orders",
                Vec::new(),
                None,
            ),
            history_entry(
                HttpMethod::Get,
                "https://example.com/users/2",
                Vec::new(),
                None,
            ),
        ]);
        app.history_search.set_content("users");
        app.select_latest_visible_history();

        app.handle_history_key(key(KeyCode::Up));

        assert_eq!(app.history_index, 0);

        app.handle_history_key(key(KeyCode::Down));

        assert_eq!(app.history_index, 2);
    }

    /// Saved request search matches names and request parts.
    #[test]
    fn saved_search_matches_name_and_request_parts() {
        let mut app = app_with_saved_requests(vec![
            saved_request(
                "List users",
                HttpMethod::Get,
                "https://example.com/users",
                None,
            ),
            saved_request(
                "Create order",
                HttpMethod::Post,
                "https://example.com/orders",
                Some("{\"status\":\"pending\"}"),
            ),
        ]);

        app.history_search.set_content("list");
        assert_eq!(app.filtered_saved_indices(), vec![0]);

        app.history_search.set_content("POST");
        assert_eq!(app.filtered_saved_indices(), vec![1]);

        app.history_search.set_content("pending");
        assert_eq!(app.filtered_saved_indices(), vec![1]);
    }

    /// Saving the current composer pins a reusable request and selects saved mode.
    #[test]
    fn save_current_request_adds_saved_request() {
        let mut app = app_with_history(Vec::new());
        app.url.set_content("https://example.com/users");

        app.save_current_request();

        assert_eq!(app.sidebar_mode, SidebarMode::Saved);
        assert_eq!(app.saved_requests.len(), 1);
        assert_eq!(app.saved_requests[0].name, "GET https://example.com/users");
        let _ = std::fs::remove_file(&app.config.saved_requests_file);
    }

    /// Loading a saved request copies it into the request composer.
    #[test]
    fn load_saved_request_populates_composer() {
        let mut app = app_with_saved_requests(vec![saved_request(
            "Create order",
            HttpMethod::Post,
            "https://example.com/orders",
            Some("{\"status\":\"pending\"}"),
        )]);

        app.load_from_saved_request();

        assert_eq!(app.current_method(), &HttpMethod::Post);
        assert_eq!(app.url.content(), "https://example.com/orders");
        assert_eq!(app.body.content(), "{\"status\":\"pending\"}");
    }
}
