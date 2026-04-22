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

/// Which text area a generic key handler should target.
enum TextAreaTarget {
    Headers,
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
    /// Loaded request history (oldest first).
    pub history: Vec<HistoryEntry>,
    /// Currently selected row in the history list.
    pub history_index: usize,
    /// Most recent HTTP response.
    pub response: Option<HttpResponse>,
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
            history,
            history_index,
            response: None,
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

        // Clone to avoid holding a shared borrow of `self` while calling &mut methods.
        match self.focus {
            Focus::History => self.handle_history_key(key),
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

    // ── Focus cycling ────────────────────────────────────────────────────────
    fn next_focus(&mut self) {
        self.focus = match self.focus {
            Focus::History => Focus::Method,
            Focus::Method => Focus::Url,
            Focus::Url => Focus::Headers,
            Focus::Headers => Focus::Body,
            Focus::Body => Focus::Response,
            Focus::Response => Focus::History,
        };
    }

    fn prev_focus(&mut self) {
        self.focus = match self.focus {
            Focus::History => Focus::Response,
            Focus::Method => Focus::History,
            Focus::Url => Focus::Method,
            Focus::Headers => Focus::Url,
            Focus::Body => Focus::Headers,
            Focus::Response => Focus::Body,
        };
    }

    // ── Per-pane key handlers ────────────────────────────────────────────────

    fn handle_history_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.history.is_empty() {
                    self.history_index =
                        (self.history_index + 1).min(self.history.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.history_index = self.history_index.saturating_sub(1);
            }
            KeyCode::Enter => self.load_from_history(),
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn handle_method_key(&mut self, key: KeyEvent) {
        let count = HttpMethod::all().len();
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
            KeyCode::Left | KeyCode::Char('h') => {
                self.method_index = if self.method_index == 0 { count - 1 } else { self.method_index - 1 };
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.method_index = (self.method_index + 1) % count;
            }
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn handle_url_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => { self.next_focus(); return; }
            KeyCode::BackTab => { self.prev_focus(); return; }
            KeyCode::Enter => { self.send_request(); return; }
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

    fn handle_text_area_key(&mut self, key: KeyEvent, target: TextAreaTarget) {
        // Handle focus-change keys before borrowing the target area.
        match key.code {
            KeyCode::Tab => { self.next_focus(); return; }
            KeyCode::BackTab => { self.prev_focus(); return; }
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

    fn handle_response_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => self.next_focus(),
            KeyCode::BackTab => self.prev_focus(),
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
        let Some(entry) = self.history.get(self.history_index) else {
            return;
        };

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
        let body = if body_text.trim().is_empty() { None } else { Some(body_text) };

        let request = HttpRequest { method, url, headers, body };
        self.status_message = format!("Sending {} {}…", request.method, request.url);

        match request.execute() {
            Ok(response) => {
                let entry = HistoryEntry::from_request(&request);
                let _ = append_history(&self.config.history_file, &entry);
                self.status_message =
                    format!("✓ {} {}  —  {}", request.method, request.url, response.status_code);
                self.history.push(entry);
                self.history_index = self.history.len() - 1;
                self.response = Some(response);
                self.response_scroll = 0;
                self.focus = Focus::Response;
            }
            Err(e) => {
                self.status_message = format!("Error: {e}");
            }
        }
    }
}

/// Parses raw header text (`Key: Value` per line) into key-value pairs.
fn parse_headers(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let colon = l.find(':')?;
            Some((l[..colon].trim().to_string(), l[colon + 1..].trim().to_string()))
        })
        .collect()
}
