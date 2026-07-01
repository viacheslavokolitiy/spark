//! Application state, focus management, input handling, and request actions.

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::Backend};
use spark_core::{
    config::Config,
    environment::{Environment, load_environments, resolve_template},
    history::{HistoryEntry, append_history, load_history},
    http::{HttpMethod, HttpRequest, HttpResponse},
    saved::{
        DEFAULT_COLLECTION, SavedRequest, load_saved_requests, remove_saved_request,
        upsert_saved_request,
    },
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
    /// Shows prettified response body content.
    Body,
    /// Shows response cookies derived from `Set-Cookie` headers.
    Cookies,
    /// Shows response headers.
    Headers,
    /// Shows scripts referenced or embedded in HTML response bodies.
    Scripts,
    /// Shows request/response timing and metadata.
    Trace,
    /// Shows request and response byte sizes.
    Sizes,
    /// Shows response code distribution across history.
    History,
}

impl ResponseTab {
    /// Returns every response tab in display order.
    pub const fn all() -> &'static [Self] {
        &[
            Self::Body,
            Self::Cookies,
            Self::Headers,
            Self::Scripts,
            Self::Trace,
            Self::Sizes,
            Self::History,
        ]
    }

    /// Returns the label displayed in the response tab bar.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Body => "Body",
            Self::Cookies => "Cookies",
            Self::Headers => "Headers",
            Self::Scripts => "Scripts",
            Self::Trace => "Trace",
            Self::Sizes => "Sizes",
            Self::History => "History",
        }
    }

    /// Returns the next tab in display order.
    fn next(self) -> Self {
        let tabs = Self::all();
        let current = tabs.iter().position(|tab| *tab == self).unwrap_or_default();
        tabs[(current + 1) % tabs.len()]
    }

    /// Returns the previous tab in display order.
    fn previous(self) -> Self {
        let tabs = Self::all();
        let current = tabs.iter().position(|tab| *tab == self).unwrap_or_default();
        tabs[(current + tabs.len() - 1) % tabs.len()]
    }
}

/// Active collection shown in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarMode {
    /// Show request history.
    History,
    /// Show saved reusable requests.
    Saved,
}

/// Field currently focused inside the save target dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveDialogField {
    /// Collection name input.
    Collection,
    /// Folder name input.
    Folder,
}

/// Save target dialog state.
#[derive(Debug)]
pub struct SaveDialog {
    /// Collection name input.
    pub collection: TextInput,
    /// Optional folder name input.
    pub folder: TextInput,
    /// Field currently receiving input.
    pub field: SaveDialogField,
}

impl SaveDialog {
    /// Creates a save target dialog initialized with a collection and optional folder.
    fn new(collection: &str, folder: Option<&str>) -> Self {
        let mut collection_input = TextInput::single_line();
        let mut folder_input = TextInput::single_line();
        if collection != DEFAULT_COLLECTION || folder.is_some() {
            collection_input.set_content(collection);
        }
        folder_input.set_content(folder.unwrap_or(""));

        Self {
            collection: collection_input,
            folder: folder_input,
            field: SaveDialogField::Collection,
        }
    }

    /// Returns the trimmed collection name, falling back to the default collection.
    fn collection_name(&self) -> String {
        let collection_text = self.collection.text();
        let collection = collection_text.trim();
        if collection.is_empty() {
            DEFAULT_COLLECTION.to_string()
        } else {
            collection.to_string()
        }
    }

    /// Returns the trimmed folder name when one is present.
    fn folder_name(&self) -> Option<String> {
        let folder_text = self.folder.text();
        let folder = folder_text.trim();
        (!folder.is_empty()).then(|| folder.to_string())
    }

    /// Moves focus to the next save dialog field.
    fn next_field(&mut self) {
        self.field = match self.field {
            SaveDialogField::Collection => SaveDialogField::Folder,
            SaveDialogField::Folder => SaveDialogField::Collection,
        };
    }

    /// Moves focus to the previous save dialog field.
    fn previous_field(&mut self) {
        self.next_field();
    }
}

/// Rename dialog state for the active request tab.
#[derive(Debug)]
pub struct RenameTabDialog {
    /// New tab title input.
    pub title: TextInput,
}

impl RenameTabDialog {
    /// Creates a rename dialog initialized with the current tab title.
    fn new(title: &str) -> Self {
        let mut input = TextInput::single_line();
        input.set_content(title);
        Self { title: input }
    }

    /// Returns the trimmed custom tab title, when present.
    fn title(&self) -> Option<String> {
        let title_text = self.title.text();
        let title = title_text.trim();
        (!title.is_empty()).then(|| title.to_string())
    }
}

/// Which text area a generic key handler should target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextAreaTarget {
    /// Headers editor.
    Headers,
    /// Body editor.
    Body,
}

/// Editable request workspace shown as one top-level tab.
#[derive(Debug)]
pub struct RequestTab {
    /// Optional custom display name for this tab.
    pub custom_title: Option<String>,
    /// HTTP method selection index for this tab.
    pub method_index: usize,
    /// URL input for this tab.
    pub url: TextInput,
    /// Header editor for this tab.
    pub headers: TextInput,
    /// Body editor for this tab.
    pub body: TextInput,
    /// Most recent response for this tab.
    pub response: Option<HttpResponse>,
    /// Request that produced the most recent response for this tab.
    pub last_request: Option<HttpRequest>,
    /// Active response sub-tab for this request tab.
    pub response_tab: ResponseTab,
    /// Vertical scroll offset for the response viewer in this tab.
    pub response_scroll: u16,
}

impl RequestTab {
    /// Creates a blank request tab.
    fn blank() -> Self {
        Self {
            custom_title: None,
            method_index: 0,
            url: TextInput::single_line(),
            headers: TextInput::multi_line(),
            body: TextInput::multi_line(),
            response: None,
            last_request: None,
            response_tab: ResponseTab::Body,
            response_scroll: 0,
        }
    }

    /// Returns a compact display label for the tab bar.
    pub fn title(&self, index: usize) -> String {
        if let Some(title) = &self.custom_title {
            return title.clone();
        }

        let url_text = self.url.text();
        let trimmed = url_text.trim();
        if trimmed.is_empty() {
            format!("Untitled {}", index + 1)
        } else {
            trimmed.to_string()
        }
    }
}

/// Request queued for execution from a specific tab.
#[derive(Debug)]
struct PendingRequest {
    /// Index of the tab that started the request.
    tab_index: usize,
    /// Fully resolved request ready for execution.
    request: HttpRequest,
}

/// Complete application state.
pub struct App {
    /// Application configuration.
    pub config: Config,
    /// Currently focused element.
    pub focus: Focus,
    /// Whether the request method dropdown is expanded.
    pub method_dropdown_open: bool,
    /// Open request workspaces.
    pub request_tabs: Vec<RequestTab>,
    /// Currently selected request workspace.
    pub active_request_tab: usize,
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
    /// Loaded request environments.
    pub environments: Vec<Environment>,
    /// Currently active environment, when any environments are loaded.
    pub environment_index: Option<usize>,
    /// Active save destination dialog.
    pub save_dialog: Option<SaveDialog>,
    /// Active request tab rename dialog.
    pub rename_tab_dialog: Option<RenameTabDialog>,
    /// Request waiting for a painted "sending" frame before execution.
    pending_request: Option<PendingRequest>,
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
        let environments = load_environments(&config.environments_file);
        let environment_index = (!environments.is_empty()).then_some(0);
        Self {
            config,
            focus: Focus::History,
            method_dropdown_open: false,
            request_tabs: vec![RequestTab::blank()],
            active_request_tab: 0,
            history_search: TextInput::single_line(),
            history,
            history_index,
            saved_requests,
            saved_index,
            sidebar_mode: SidebarMode::History,
            environments,
            environment_index,
            save_dialog: None,
            rename_tab_dialog: None,
            pending_request: None,
            should_quit: false,
            status_message: String::from(
                "Tab: cycle focus | Ctrl+T: new tab | Ctrl+R: rename tab | Ctrl+S: send | Ctrl+P: save | Ctrl+O: saved/history | Ctrl+E: env",
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
        if self.rename_tab_dialog.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
            self.handle_rename_tab_dialog_key(key);
            return;
        }

        if self.save_dialog.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
            self.handle_save_dialog_key(key);
            return;
        }

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
                    self.open_save_dialog();
                    return;
                }
                KeyCode::Char('t') => {
                    self.open_new_request_tab();
                    return;
                }
                KeyCode::Char('w') => {
                    self.close_active_request_tab();
                    return;
                }
                KeyCode::Char('r') => {
                    self.open_rename_tab_dialog();
                    return;
                }
                KeyCode::Left => {
                    self.select_previous_request_tab();
                    return;
                }
                KeyCode::Right => {
                    self.select_next_request_tab();
                    return;
                }
                KeyCode::Char('o') => {
                    self.toggle_sidebar_mode();
                    return;
                }
                KeyCode::Char('e') => {
                    self.select_next_environment();
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
        &HttpMethod::all()[self.active_tab().method_index]
    }

    /// Returns whether a request is queued or currently being started.
    #[must_use]
    pub fn is_sending(&self) -> bool {
        self.pending_request
            .as_ref()
            .is_some_and(|pending| pending.tab_index == self.active_request_tab)
    }

    /// Returns whether `tab_index` has a request queued or executing.
    #[must_use]
    pub fn is_request_tab_sending(&self, tab_index: usize) -> bool {
        self.pending_request
            .as_ref()
            .is_some_and(|pending| pending.tab_index == tab_index)
    }

    /// Returns the currently active request tab.
    #[must_use]
    pub fn active_tab(&self) -> &RequestTab {
        &self.request_tabs[self.active_request_tab]
    }

    /// Returns the currently active request tab mutably.
    fn active_tab_mut(&mut self) -> &mut RequestTab {
        &mut self.request_tabs[self.active_request_tab]
    }

    /// Returns the active environment, if one is selected.
    #[must_use]
    pub fn active_environment(&self) -> Option<&Environment> {
        self.environment_index
            .and_then(|idx| self.environments.get(idx))
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

        let mut indices = self
            .saved_requests
            .iter()
            .enumerate()
            .filter_map(|(idx, request)| saved_request_matches(request, query).then_some(idx))
            .collect::<Vec<_>>();

        indices.sort_by(|left, right| {
            let left_request = &self.saved_requests[*left];
            let right_request = &self.saved_requests[*right];
            saved_request_group_key(left_request, *left)
                .cmp(&saved_request_group_key(right_request, *right))
        });

        indices
    }

    // ── Focus cycling ────────────────────────────────────────────────────────
    /// Moves focus to the next pane in tab order.
    fn next_focus(&mut self) {
        self.method_dropdown_open = false;
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
        self.method_dropdown_open = false;
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
        match key.code {
            KeyCode::Tab => {
                self.method_dropdown_open = false;
                self.next_focus();
            }
            KeyCode::BackTab => {
                self.method_dropdown_open = false;
                self.prev_focus();
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.method_dropdown_open = !self.method_dropdown_open;
            }
            KeyCode::Esc => {
                self.method_dropdown_open = false;
            }
            KeyCode::Up | KeyCode::Char('k') if self.method_dropdown_open => {
                self.select_previous_method();
            }
            KeyCode::Down | KeyCode::Char('j') if self.method_dropdown_open => {
                self.select_next_method();
            }
            KeyCode::Left | KeyCode::Char('h') if !self.method_dropdown_open => {
                self.select_previous_method();
            }
            KeyCode::Right | KeyCode::Char('l') if !self.method_dropdown_open => {
                self.select_next_method();
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
            KeyCode::Left => self.active_tab_mut().url.move_left(),
            KeyCode::Right => self.active_tab_mut().url.move_right(),
            KeyCode::Home => self.active_tab_mut().url.move_to_line_start(),
            KeyCode::End => self.active_tab_mut().url.move_to_line_end(),
            KeyCode::Backspace => self.active_tab_mut().url.backspace(),
            KeyCode::Char(c) => self.active_tab_mut().url.insert_char(c),
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
            TextAreaTarget::Headers => &mut self.active_tab_mut().headers,
            TextAreaTarget::Body => &mut self.active_tab_mut().body,
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
                let tab = self.active_tab_mut();
                tab.response_scroll = tab.response_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let tab = self.active_tab_mut();
                tab.response_scroll = tab.response_scroll.saturating_sub(1);
            }
            KeyCode::Char('g') => self.active_tab_mut().response_scroll = 0,
            KeyCode::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    // ── Actions ──────────────────────────────────────────────────────────────

    /// Opens a blank request tab and selects it.
    fn open_new_request_tab(&mut self) {
        self.method_dropdown_open = false;
        self.request_tabs.push(RequestTab::blank());
        self.active_request_tab = self.request_tabs.len() - 1;
        self.focus = Focus::Url;
        self.status_message = format!("Opened request tab {}", self.active_request_tab + 1);
    }

    /// Closes the current request tab, preserving one blank tab at minimum.
    fn close_active_request_tab(&mut self) {
        self.method_dropdown_open = false;
        if self.request_tabs.len() == 1 {
            self.request_tabs[0] = RequestTab::blank();
            self.pending_request = None;
            self.status_message = "Cleared the only request tab.".to_string();
            return;
        }

        let closed = self.active_request_tab;
        self.request_tabs.remove(closed);
        if let Some(mut pending) = self.pending_request.take()
            && pending.tab_index != closed
        {
            if pending.tab_index > closed {
                pending.tab_index -= 1;
            }
            self.pending_request = Some(pending);
        }
        self.active_request_tab = closed.min(self.request_tabs.len().saturating_sub(1));
        self.status_message = format!("Closed request tab {}", closed + 1);
    }

    /// Selects the next open request tab.
    fn select_next_request_tab(&mut self) {
        self.method_dropdown_open = false;
        self.active_request_tab = (self.active_request_tab + 1) % self.request_tabs.len();
        self.status_message = format!("Request tab {}", self.active_request_tab + 1);
    }

    /// Selects the previous open request tab.
    fn select_previous_request_tab(&mut self) {
        self.method_dropdown_open = false;
        self.active_request_tab = if self.active_request_tab == 0 {
            self.request_tabs.len() - 1
        } else {
            self.active_request_tab - 1
        };
        self.status_message = format!("Request tab {}", self.active_request_tab + 1);
    }

    /// Opens the active request tab rename dialog.
    fn open_rename_tab_dialog(&mut self) {
        self.method_dropdown_open = false;
        let current_title = self.active_tab().custom_title.clone().unwrap_or_default();
        self.rename_tab_dialog = Some(RenameTabDialog::new(&current_title));
        self.status_message = "Rename tab: Enter applies, Esc cancels, blank resets.".to_string();
    }

    /// Applies a custom title to the active request tab.
    fn rename_active_request_tab(&mut self, title: Option<String>) {
        self.active_tab_mut().custom_title = title;
        let display_title = self.active_tab().title(self.active_request_tab);
        self.status_message = format!("Renamed request tab to: {display_title}");
    }

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

        let method = entry.method;
        let url = entry.url.clone();
        let headers_text = format_headers(&entry.headers);
        let body_text = entry.body.clone().unwrap_or_default();

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == method) {
            self.active_tab_mut().method_index = idx;
        }

        self.active_tab_mut().url.set_content(&url);
        self.active_tab_mut().headers.set_content(&headers_text);
        self.active_tab_mut().body.set_content(&body_text);

        self.method_dropdown_open = false;
        self.focus = Focus::Url;
        self.status_message = format!("Loaded: {method} {url}");
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

        let method = request.method;
        let url = request.url.clone();
        let name = request.name.clone();
        let headers_text = format_headers(&request.headers);
        let body_text = request.body.clone().unwrap_or_default();

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == method) {
            self.active_tab_mut().method_index = idx;
        }

        self.active_tab_mut().url.set_content(&url);
        self.active_tab_mut().headers.set_content(&headers_text);
        self.active_tab_mut().body.set_content(&body_text);

        self.method_dropdown_open = false;
        self.focus = Focus::Url;
        self.status_message = format!("Loaded saved: {name}");
    }

    /// Opens the save target dialog for the current request template.
    fn open_save_dialog(&mut self) {
        if self.current_template_request().is_none() {
            self.status_message = "URL is empty - enter a URL before saving.".to_string();
            return;
        }

        let (collection, folder) = self.selected_saved_context();
        self.save_dialog = Some(SaveDialog::new(&collection, folder.as_deref()));
        self.status_message =
            "Save target: edit collection/folder, Enter saves, Esc cancels.".to_string();
    }

    /// Saves the current composer contents into `collection` and `folder`.
    fn save_current_request_to(&mut self, collection: String, folder: Option<String>) {
        let Some(request) = self.current_template_request() else {
            self.status_message = "URL is empty - enter a URL before saving.".to_string();
            return;
        };

        let mut saved = SavedRequest::from_request(&request);
        saved.collection = collection;
        saved.folder = folder;
        match upsert_saved_request(
            &self.config.saved_requests_file,
            &mut self.saved_requests,
            saved,
        ) {
            Ok(idx) => {
                self.saved_index = idx;
                self.sidebar_mode = SidebarMode::Saved;
                let name = &self.saved_requests[idx].name;
                let location = saved_location_label(&self.saved_requests[idx]);
                self.status_message = format!("Saved request: {location}/{name}");
            }
            Err(e) => {
                self.status_message = format!("Error saving request: {e}");
            }
        }
    }

    /// Handles key input while the save target dialog is active.
    fn handle_save_dialog_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.save_dialog = None;
                self.status_message = "Save cancelled.".to_string();
            }
            KeyCode::Enter => {
                let Some(dialog) = self.save_dialog.take() else {
                    return;
                };
                self.save_current_request_to(dialog.collection_name(), dialog.folder_name());
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(dialog) = &mut self.save_dialog {
                    dialog.next_field();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(dialog) = &mut self.save_dialog {
                    dialog.previous_field();
                }
            }
            _ => {
                if let Some(dialog) = &mut self.save_dialog {
                    let input = match dialog.field {
                        SaveDialogField::Collection => &mut dialog.collection,
                        SaveDialogField::Folder => &mut dialog.folder,
                    };
                    handle_single_line_text_input(input, key);
                }
            }
        }
    }

    /// Handles key input while the rename tab dialog is active.
    fn handle_rename_tab_dialog_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.rename_tab_dialog = None;
                self.status_message = "Rename cancelled.".to_string();
            }
            KeyCode::Enter => {
                let Some(dialog) = self.rename_tab_dialog.take() else {
                    return;
                };
                self.rename_active_request_tab(dialog.title());
            }
            _ => {
                if let Some(dialog) = &mut self.rename_tab_dialog {
                    handle_single_line_text_input(&mut dialog.title, key);
                }
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
        let Some(template) = self.current_template_request() else {
            self.status_message = "URL is empty — enter a URL and try again.".to_string();
            return;
        };
        let request = match self.resolve_request_template(&template) {
            Ok(request) => request,
            Err(e) => {
                self.status_message = format!("Environment error: {e}");
                return;
            }
        };
        self.method_dropdown_open = false;
        self.focus = Focus::Response;
        let tab_index = self.active_request_tab;
        let tab = self.active_tab_mut();
        tab.response_tab = ResponseTab::Body;
        tab.response_scroll = 0;
        self.status_message = format!("Sending {} {}…", request.method, request.url);
        self.pending_request = Some(PendingRequest { tab_index, request });
    }

    /// Executes a queued request, writing the result to history.
    fn execute_pending_request(&mut self) {
        let Some(pending) = self.pending_request.take() else {
            return;
        };
        let request = pending.request;
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
                if let Some(tab) = self.request_tabs.get_mut(pending.tab_index) {
                    tab.last_request = Some(request);
                    tab.response = Some(response);
                }
            }
            Err(e) => {
                self.status_message = format!("Error: {e}");
            }
        }
    }

    /// Builds an unresolved request template from the current composer fields.
    fn current_template_request(&self) -> Option<HttpRequest> {
        let tab = self.active_tab();
        let url_text = tab.url.text();
        let url = url_text.trim();
        if url.is_empty() {
            return None;
        }

        let method = *self.current_method();
        let headers_text = tab.headers.text();
        let headers = parse_headers(headers_text.as_ref());
        let body_text = tab.body.text();
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

    /// Returns the collection/folder context for a newly saved request.
    fn selected_saved_context(&self) -> (String, Option<String>) {
        if self.sidebar_mode == SidebarMode::Saved
            && self.filtered_saved_indices().contains(&self.saved_index)
            && let Some(request) = self.saved_requests.get(self.saved_index)
        {
            return (request.collection.clone(), request.folder.clone());
        }

        (DEFAULT_COLLECTION.to_string(), None)
    }

    /// Resolves environment variables in a request template.
    fn resolve_request_template(&self, request: &HttpRequest) -> Result<HttpRequest, String> {
        let environment = self.active_environment();
        let url = resolve_template(&request.url, environment).map_err(|e| e.to_string())?;
        let headers = request
            .headers
            .iter()
            .map(|(key, value)| {
                let key = resolve_template(key, environment).map_err(|e| e.to_string())?;
                let value = resolve_template(value, environment).map_err(|e| e.to_string())?;
                Ok((key, value))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let body = request
            .body
            .as_deref()
            .map(|body| resolve_template(body, environment).map_err(|e| e.to_string()))
            .transpose()?;

        Ok(HttpRequest {
            method: request.method,
            url,
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

    /// Selects the previous HTTP method.
    fn select_previous_method(&mut self) {
        let count = HttpMethod::all().len();
        let tab = self.active_tab_mut();
        tab.method_index = if tab.method_index == 0 {
            count - 1
        } else {
            tab.method_index - 1
        };
    }

    /// Selects the next HTTP method.
    fn select_next_method(&mut self) {
        let count = HttpMethod::all().len();
        let tab = self.active_tab_mut();
        tab.method_index = (tab.method_index + 1) % count;
    }

    /// Selects the next response pane tab.
    fn next_response_tab(&mut self) {
        let tab = self.active_tab_mut();
        tab.response_tab = tab.response_tab.next();
        tab.response_scroll = 0;
    }

    /// Selects the previous response pane tab.
    fn previous_response_tab(&mut self) {
        let tab = self.active_tab_mut();
        tab.response_tab = tab.response_tab.previous();
        tab.response_scroll = 0;
    }

    /// Selects the next loaded environment.
    fn select_next_environment(&mut self) {
        if self.environments.is_empty() {
            self.environment_index = None;
            self.status_message = format!(
                "No environments loaded from {}",
                self.config.environments_file.display()
            );
            return;
        }

        let next = self
            .environment_index
            .map_or(0, |idx| (idx + 1) % self.environments.len());
        self.environment_index = Some(next);
        self.status_message = format!("Environment: {}", self.environments[next].name);
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

/// Applies simple single-line text editing keys to `input`.
fn handle_single_line_text_input(input: &mut TextInput, key: KeyEvent) {
    match key.code {
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => input.move_to_line_start(),
        KeyCode::End => input.move_to_line_end(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Char(c) => input.insert_char(c),
        _ => {}
    }
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

    contains_case_insensitive(&request.collection, query)
        || request
            .folder
            .as_deref()
            .is_some_and(|folder| contains_case_insensitive(folder, query))
        || contains_case_insensitive(&request.name, query)
        || contains_case_insensitive(request.method.as_str(), query)
        || contains_case_insensitive(&request.url, query)
        || check_headers(&request.headers, query)
        || request
            .body
            .as_deref()
            .is_some_and(|body| contains_case_insensitive(body, query))
}

/// Returns a slash-separated collection/folder label for a saved request.
fn saved_location_label(request: &SavedRequest) -> String {
    request.folder.as_ref().map_or_else(
        || request.collection.clone(),
        |folder| format!("{}/{folder}", request.collection),
    )
}

/// Returns a stable grouping key for saved request display.
fn saved_request_group_key(request: &SavedRequest, index: usize) -> (String, String, usize) {
    (
        request.collection.to_lowercase(),
        request.folder.as_deref().unwrap_or("").to_lowercase(),
        index,
    )
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
            environments_file: std::env::temp_dir()
                .join(format!("spark-tui-test-env-{test_id}.json")),
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

    /// Creates a saved request inside a collection and optional folder.
    fn saved_request_in(
        name: &str,
        method: HttpMethod,
        url: &str,
        collection: &str,
        folder: Option<&str>,
    ) -> SavedRequest {
        let mut saved = saved_request(name, method, url, None);
        saved.collection = collection.to_string();
        saved.folder = folder.map(ToString::to_string);
        saved
    }

    /// Creates a named environment with one base URL.
    fn environment(name: &str, base_url: &str) -> Environment {
        Environment {
            name: name.to_string(),
            variables: vec![
                ("base_url".to_string(), base_url.to_string()),
                ("token".to_string(), "abc123".to_string()),
            ],
        }
    }

    /// Builds a plain key event for input handler tests.
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Builds a control-modified key event for input handler tests.
    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// Types text through the app key handler.
    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
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

    /// App startup focuses the history list and selects the latest request.
    #[test]
    fn startup_selects_latest_history_request() {
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

        assert_eq!(app.focus, Focus::History);
        assert_eq!(app.history_index, 1);
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

    /// Enter opens and closes the method dropdown while keeping focus on method.
    #[test]
    fn method_dropdown_toggles_with_enter() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::Method;

        app.handle_key(key(KeyCode::Enter));
        assert!(app.method_dropdown_open);
        assert_eq!(app.focus, Focus::Method);

        app.handle_key(key(KeyCode::Enter));
        assert!(!app.method_dropdown_open);
        assert_eq!(app.focus, Focus::Method);
    }

    /// Open method dropdown uses vertical navigation to select a method.
    #[test]
    fn method_dropdown_selects_methods_with_arrow_keys() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::Method;
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.current_method(), &HttpMethod::Post);

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.current_method(), &HttpMethod::Get);
    }

    /// Leaving the method component closes its dropdown.
    #[test]
    fn method_dropdown_closes_when_focus_moves() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::Method;
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Tab));

        assert!(!app.method_dropdown_open);
        assert_eq!(app.focus, Focus::Url);
    }

    /// Ctrl+T opens a new blank request tab and focuses the URL.
    #[test]
    fn ctrl_t_opens_blank_request_tab() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('t')));

        assert_eq!(app.request_tabs.len(), 2);
        assert_eq!(app.active_request_tab, 1);
        assert_eq!(app.focus, Focus::Url);
        assert_eq!(app.active_tab().url.content(), "");
    }

    /// Ctrl+Left and Ctrl+Right switch request tabs without losing draft state.
    #[test]
    fn ctrl_arrows_switch_request_tabs_and_preserve_drafts() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");
        app.handle_key(ctrl_key(KeyCode::Char('t')));
        app.active_tab_mut()
            .url
            .set_content("https://example.com/orders");

        app.handle_key(ctrl_key(KeyCode::Left));

        assert_eq!(app.active_request_tab, 0);
        assert_eq!(app.active_tab().url.content(), "https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Right));

        assert_eq!(app.active_request_tab, 1);
        assert_eq!(app.active_tab().url.content(), "https://example.com/orders");
    }

    /// Ctrl+W closes the active request tab and selects a remaining tab.
    #[test]
    fn ctrl_w_closes_active_request_tab() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");
        app.handle_key(ctrl_key(KeyCode::Char('t')));
        app.active_tab_mut()
            .url
            .set_content("https://example.com/orders");

        app.handle_key(ctrl_key(KeyCode::Char('w')));

        assert_eq!(app.request_tabs.len(), 1);
        assert_eq!(app.active_request_tab, 0);
        assert_eq!(app.active_tab().url.content(), "https://example.com/users");
    }

    /// Closing the final request tab clears it rather than leaving no workspace.
    #[test]
    fn ctrl_w_clears_final_request_tab() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('w')));

        assert_eq!(app.request_tabs.len(), 1);
        assert_eq!(app.active_tab().url.content(), "");
        assert_eq!(app.status_message, "Cleared the only request tab.");
    }

    /// Ctrl+R opens the active request tab rename dialog.
    #[test]
    fn ctrl_r_opens_rename_tab_dialog() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('r')));

        let dialog = app
            .rename_tab_dialog
            .as_ref()
            .expect("rename dialog should open");
        assert_eq!(dialog.title.content(), "");
        assert_eq!(
            app.status_message,
            "Rename tab: Enter applies, Esc cancels, blank resets."
        );
    }

    /// Enter in the rename dialog applies a custom active tab title.
    #[test]
    fn rename_tab_dialog_applies_custom_title() {
        let mut app = app_with_history(Vec::new());

        app.handle_key(ctrl_key(KeyCode::Char('r')));
        app.rename_tab_dialog
            .as_mut()
            .expect("rename dialog should open")
            .title
            .set_content("Users");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.rename_tab_dialog.is_none());
        assert_eq!(app.active_tab().custom_title.as_deref(), Some("Users"));
        assert_eq!(app.active_tab().title(app.active_request_tab), "Users");
        assert_eq!(app.status_message, "Renamed request tab to: Users");
    }

    /// Escape closes the rename dialog without changing the tab title.
    #[test]
    fn rename_tab_dialog_escape_cancels() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut().custom_title = Some("Original".to_string());

        app.handle_key(ctrl_key(KeyCode::Char('r')));
        app.rename_tab_dialog
            .as_mut()
            .expect("rename dialog should open")
            .title
            .set_content("Changed");
        app.handle_key(key(KeyCode::Esc));

        assert!(app.rename_tab_dialog.is_none());
        assert_eq!(app.active_tab().custom_title.as_deref(), Some("Original"));
        assert_eq!(app.status_message, "Rename cancelled.");
    }

    /// Blank rename input clears the custom title and returns to derived titles.
    #[test]
    fn blank_rename_tab_dialog_resets_custom_title() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut().custom_title = Some("Users".to_string());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('r')));
        app.rename_tab_dialog
            .as_mut()
            .expect("rename dialog should open")
            .title
            .set_content("   ");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.active_tab().custom_title.is_none());
        assert_eq!(
            app.active_tab().title(app.active_request_tab),
            "https://example.com/users"
        );
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

    /// Saved request search matches collection and folder metadata.
    #[test]
    fn saved_search_matches_collection_and_folder() {
        let mut app = app_with_saved_requests(vec![
            saved_request_in(
                "List users",
                HttpMethod::Get,
                "https://example.com/users",
                "Identity",
                Some("Users"),
            ),
            saved_request_in(
                "Create order",
                HttpMethod::Post,
                "https://example.com/orders",
                "Commerce",
                Some("Orders"),
            ),
        ]);

        app.history_search.set_content("identity");
        assert_eq!(app.filtered_saved_indices(), vec![0]);

        app.history_search.set_content("orders");
        assert_eq!(app.filtered_saved_indices(), vec![1]);
    }

    /// Saving the current composer pins a reusable request and selects saved mode.
    #[test]
    fn save_current_request_adds_saved_request() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.save_current_request_to(DEFAULT_COLLECTION.to_string(), None);

        assert_eq!(app.sidebar_mode, SidebarMode::Saved);
        assert_eq!(app.saved_requests.len(), 1);
        assert_eq!(app.saved_requests[0].name, "GET https://example.com/users");
        let _ = std::fs::remove_file(&app.config.saved_requests_file);
    }

    /// Ctrl+P opens a save dialog initialized for a new destination.
    #[test]
    fn ctrl_p_opens_save_dialog() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('p')));

        let dialog = app.save_dialog.as_ref().expect("save dialog should open");
        assert_eq!(dialog.collection.content(), "");
        assert_eq!(dialog.folder.content(), "");
        assert_eq!(dialog.field, SaveDialogField::Collection);
    }

    /// The save dialog can create a new collection and folder for a request.
    #[test]
    fn save_dialog_saves_into_typed_collection_and_folder() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('p')));
        type_text(&mut app, "Identity");
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "Users");
        app.handle_key(key(KeyCode::Enter));

        assert!(app.save_dialog.is_none());
        assert_eq!(app.saved_requests.len(), 1);
        assert_eq!(app.saved_requests[0].collection, "Identity");
        assert_eq!(app.saved_requests[0].folder.as_deref(), Some("Users"));
        assert_eq!(
            app.status_message,
            "Saved request: Identity/Users/GET https://example.com/users"
        );
        let _ = std::fs::remove_file(&app.config.saved_requests_file);
    }

    /// Escape cancels the save dialog without writing a saved request.
    #[test]
    fn save_dialog_escape_cancels_save() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");

        app.handle_key(ctrl_key(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::Esc));

        assert!(app.save_dialog.is_none());
        assert!(app.saved_requests.is_empty());
        assert_eq!(app.status_message, "Save cancelled.");
    }

    /// Saving while a saved request is selected preserves its collection and folder.
    #[test]
    fn save_current_request_uses_selected_saved_folder_context() {
        let mut app = app_with_saved_requests(vec![saved_request_in(
            "List users",
            HttpMethod::Get,
            "https://example.com/users",
            "Identity",
            Some("Users"),
        )]);
        app.saved_index = 0;
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users/42");

        let (collection, folder) = app.selected_saved_context();
        app.save_current_request_to(collection, folder);

        let saved = app
            .saved_requests
            .iter()
            .find(|request| request.url == "https://example.com/users/42")
            .expect("new request should be saved");
        assert_eq!(saved.collection, "Identity");
        assert_eq!(saved.folder.as_deref(), Some("Users"));
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
        assert_eq!(app.active_tab().url.content(), "https://example.com/orders");
        assert_eq!(app.active_tab().body.content(), "{\"status\":\"pending\"}");
    }

    /// Environment cycling moves through loaded environments.
    #[test]
    fn ctrl_e_cycles_active_environment() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![
            environment("Local", "http://localhost:8080"),
            environment("Prod", "https://api.example.com"),
        ];
        app.environment_index = Some(0);

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));

        assert_eq!(
            app.active_environment().map(|env| env.name.as_str()),
            Some("Prod")
        );
        assert_eq!(app.status_message, "Environment: Prod");
    }

    /// Sending resolves variables without mutating the composer template.
    #[test]
    fn send_request_resolves_environment_variables() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.active_tab_mut().url.set_content("{{base_url}}/users");
        app.active_tab_mut()
            .headers
            .set_content("Authorization: Bearer {{token}}");
        app.active_tab_mut()
            .body
            .set_content("{\"source\":\"{{ base_url }}\"}");

        app.send_request();

        let request = app
            .pending_request
            .as_ref()
            .expect("resolved request should be queued");
        assert_eq!(request.request.url, "http://localhost:8080/users");
        assert_eq!(
            request.request.headers,
            vec![("Authorization".to_string(), "Bearer abc123".to_string())]
        );
        assert_eq!(
            request.request.body.as_deref(),
            Some("{\"source\":\"http://localhost:8080\"}")
        );
        assert_eq!(app.active_tab().url.content(), "{{base_url}}/users");
    }

    /// Missing variables prevent a request from being queued.
    #[test]
    fn send_request_reports_missing_environment_variables() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.active_tab_mut()
            .url
            .set_content("{{base_url}}/users/{{user_id}}");

        app.send_request();

        assert!(app.pending_request.is_none());
        assert_eq!(
            app.status_message,
            "Environment error: missing environment variable: user_id"
        );
    }
}
