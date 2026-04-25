//! Application state, focus management, input handling, and request actions.

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::Backend};
use spark_core::{
    config::Config,
    history::{HistoryEntry, append_history, load_history},
    http::{HttpMethod, HttpRequest, HttpResponse},
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseTab {
    /// Shows status, headers, and body content.
    Body,
    /// Shows request and response byte sizes.
    Sizes,
    /// Shows response code distribution across history.
    History,
}

/// Which text area a generic key handler should target.
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
    /// Most recent HTTP response.
    pub response: Option<HttpResponse>,
    /// Request that produced the most recent response.
    pub last_request: Option<HttpRequest>,
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
            response: None,
            last_request: None,
            response_tab: ResponseTab::Body,
            response_scroll: 0,
            should_quit: false,
            status_message: String::from(
                "Tab: cycle focus | Ctrl+S / Enter in URL: send | q: quit",
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

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    /// Dispatches a key event to the appropriate handler.
    pub fn handle_key(&mut self, key: KeyEvent) {
        // Global shortcuts (Ctrl+C / Ctrl+S) regardless of focus.
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

    /// Returns indexes of history entries matching the active search query.
    #[must_use]
    pub fn filtered_history_indices(&self) -> Vec<usize> {
        let query = self.history_search.content();
        let query = query.trim();

        self.history
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| history_matches(entry, query).then_some(idx))
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
                self.select_next_visible_history();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_previous_visible_history();
            }
            KeyCode::Enter => self.load_from_history(),
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
                self.select_latest_visible_history();
            }
            KeyCode::Char(c) => {
                self.history_search.insert_char(c);
                self.select_latest_visible_history();
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

        if !history_matches(entry, self.history_search.content().trim()) {
            return;
        }

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == entry.method) {
            self.method_index = idx;
        }

        self.url.set_content(&entry.url);

        let headers_text = entry
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.headers.set_content(&headers_text);

        self.body.set_content(entry.body.as_deref().unwrap_or(""));

        self.focus = Focus::Url;
        self.status_message = format!("Loaded: {} {}", entry.method, entry.url);
    }

    /// Builds and executes the current request, writing the result to history.
    pub fn send_request(&mut self) {
        let url = self.url.content().trim().to_string();
        if url.is_empty() {
            self.status_message = "URL is empty — enter a URL and try again.".to_string();
            return;
        }

        let method = self.current_method().clone();
        let headers = parse_headers(&self.headers.content());
        let body_text = self.body.content();
        let body = if body_text.trim().is_empty() {
            None
        } else {
            Some(body_text)
        };

        let request = HttpRequest {
            method,
            url,
            headers,
            body,
        };
        self.status_message = format!("Sending {} {}…", request.method, request.url);

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
                self.select_latest_visible_history();
                self.last_request = Some(request);
                self.response = Some(response);
                self.response_tab = ResponseTab::Body;
                self.response_scroll = 0;
                self.focus = Focus::Response;
            }
            Err(e) => {
                self.status_message = format!("Error: {e}");
            }
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

/// Returns whether a history entry matches the search query.
fn history_matches(entry: &HistoryEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }

    entry.method.as_str().to_lowercase().contains(&query)
        || entry.url.to_lowercase().contains(&query)
        || entry.headers.iter().any(|(key, value)| {
            key.to_lowercase().contains(&query) || value.to_lowercase().contains(&query)
        })
        || entry
            .body
            .as_deref()
            .is_some_and(|body| body.to_lowercase().contains(&query))
}

#[cfg(test)]
mod tests {
    //! Tests for request history filtering and selection behavior.

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    /// Creates an app with deterministic in-memory history.
    fn app_with_history(history: Vec<HistoryEntry>) -> App {
        let mut app = App::new(Config {
            history_file: std::env::temp_dir().join(format!(
                "spark-tui-test-history-{}.jsonl",
                std::process::id()
            )),
        });
        app.history = history;
        app.history_index = app.history.len().saturating_sub(1);
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
}
