//! Application state, focus management, input handling, and request actions.

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::Backend};
use spark_core::{
    collection_io::{CollectionFormat, export_collection, import_into_saved_requests},
    config::Config,
    environment::{Environment, load_environments, resolve_template, write_environments},
    history::{HistoryEntry, append_history, load_history},
    http::{
        ApiKeyLocation, HttpMethod, HttpRequest, HttpResponse, QueryParam, RequestAuth,
        RequestScripts,
    },
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
    /// Query parameter editor.
    Params,
    /// Authentication helper editor.
    Auth,
    /// Headers text area.
    Headers,
    /// Body text area.
    Body,
    /// Pre-request script editor.
    PreRequestScript,
    /// Response test script editor.
    TestScript,
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
    /// Shows the most recent collection runner results.
    Runner,
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
            Self::Runner,
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
            Self::Runner => "Runner",
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

/// Saved request group selected for a collection runner execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRunTarget {
    /// Collection name to run.
    pub collection: String,
    /// Optional folder inside the selected collection.
    pub folder: Option<String>,
}

impl CollectionRunTarget {
    /// Returns a display label for this runner target.
    #[must_use]
    pub fn label(&self) -> String {
        self.folder.as_ref().map_or_else(
            || self.collection.clone(),
            |folder| format!("{}/{folder}", self.collection),
        )
    }
}

/// One completed collection runner request result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRunResult {
    /// Saved request name.
    pub name: String,
    /// HTTP method used by the resolved request.
    pub method: HttpMethod,
    /// Resolved request URL.
    pub url: String,
    /// Response status code, when a response was received.
    pub status_code: Option<u16>,
    /// Round-trip duration in milliseconds, when a response was received.
    pub duration_ms: Option<u128>,
    /// Number of response tests that passed.
    pub tests_passed: usize,
    /// Number of response tests that ran.
    pub tests_total: usize,
    /// Error message for failed request setup or execution.
    pub error: Option<String>,
}

impl CollectionRunResult {
    /// Returns whether this result counts as a passed runner item.
    #[must_use]
    pub fn passed(&self) -> bool {
        if self.error.is_some() {
            return false;
        }
        if self.tests_total > 0 {
            return self.tests_passed == self.tests_total;
        }
        self.status_code.is_some_and(|code| code < 400)
    }
}

/// State for the active or most recent collection runner execution.
#[derive(Debug, Clone)]
pub struct CollectionRun {
    /// Selected collection or folder target.
    pub target: CollectionRunTarget,
    /// Saved request indexes still waiting to run.
    queue: Vec<usize>,
    /// Total number of requests selected for the run.
    pub total: usize,
    /// Name of the request currently executing.
    pub current_request: Option<String>,
    /// Completed request results.
    pub results: Vec<CollectionRunResult>,
}

impl CollectionRun {
    /// Creates a collection run from saved request indexes.
    fn new(target: CollectionRunTarget, queue: Vec<usize>) -> Self {
        let total = queue.len();
        Self {
            target,
            queue,
            total,
            current_request: None,
            results: Vec::new(),
        }
    }

    /// Returns number of completed requests.
    #[must_use]
    pub fn completed(&self) -> usize {
        self.results.len()
    }

    /// Returns number of passed requests.
    #[must_use]
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|result| result.passed()).count()
    }

    /// Returns number of failed requests.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.completed().saturating_sub(self.passed())
    }

    /// Returns whether there are queued or currently executing requests.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.current_request.is_some() || self.completed() < self.total
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

/// Import/export dialog mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionIoMode {
    /// Import external requests into saved collections.
    Import,
    /// Export saved collections to an external file.
    Export,
}

/// Field currently focused inside the collection import/export dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionIoDialogField {
    /// Format selector.
    Format,
    /// File path input.
    Path,
}

/// Field currently focused inside the environment manager dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentDialogField {
    /// Environment list.
    List,
    /// Environment name input.
    Name,
    /// Environment variable editor.
    Variables,
}

/// Environment manager dialog state.
#[derive(Debug)]
pub struct EnvironmentDialog {
    /// Selected environment index, if an existing environment is selected.
    pub selected_index: Option<usize>,
    /// Environment name input.
    pub name: TextInput,
    /// Variable editor input, one `key=value` pair per line.
    pub variables: TextInput,
    /// Field currently receiving input.
    pub field: EnvironmentDialogField,
}

impl EnvironmentDialog {
    /// Creates an environment manager dialog from the active environment.
    fn new(environments: &[Environment], active_index: Option<usize>) -> Self {
        let selected_index = active_index.filter(|idx| *idx < environments.len());
        let mut dialog = Self {
            selected_index,
            name: TextInput::single_line(),
            variables: TextInput::multi_line(),
            field: EnvironmentDialogField::List,
        };
        dialog.load_selected(environments);
        dialog
    }

    /// Loads the selected environment into editable fields.
    fn load_selected(&mut self, environments: &[Environment]) {
        if let Some(environment) = self.selected_index.and_then(|idx| environments.get(idx)) {
            self.name.set_content(&environment.name);
            self.variables
                .set_content(&format_environment_variables(&environment.variables));
        } else {
            self.name.set_content("");
            self.variables.set_content("");
        }
    }

    /// Clears fields for a new environment draft.
    fn new_draft(&mut self) {
        self.selected_index = None;
        self.name.set_content("");
        self.variables.set_content("");
        self.field = EnvironmentDialogField::Name;
    }

    /// Moves focus to the next dialog field.
    fn next_field(&mut self) {
        self.field = match self.field {
            EnvironmentDialogField::List => EnvironmentDialogField::Name,
            EnvironmentDialogField::Name => EnvironmentDialogField::Variables,
            EnvironmentDialogField::Variables => EnvironmentDialogField::List,
        };
    }

    /// Moves focus to the previous dialog field.
    fn previous_field(&mut self) {
        self.field = match self.field {
            EnvironmentDialogField::List => EnvironmentDialogField::Variables,
            EnvironmentDialogField::Name => EnvironmentDialogField::List,
            EnvironmentDialogField::Variables => EnvironmentDialogField::Name,
        };
    }

    /// Builds an environment from the editor fields.
    fn environment(&self) -> Option<Environment> {
        let name_text = self.name.text();
        let name = name_text.trim();
        if name.is_empty() {
            return None;
        }
        let variables_text = self.variables.text();
        Some(Environment {
            name: name.to_string(),
            variables: parse_environment_variables(variables_text.as_ref()),
        })
    }
}

/// Format selector state for collection import/export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionIoFormat {
    /// Detect Postman or `OpenAPI` from file content.
    Auto,
    /// Postman Collection v2.1 JSON.
    Postman,
    /// `OpenAPI` 3.x JSON or YAML.
    OpenApi,
}

impl CollectionIoFormat {
    /// Returns the import dialog formats.
    const fn import_options() -> &'static [Self] {
        &[Self::Auto, Self::Postman, Self::OpenApi]
    }

    /// Returns the export dialog formats.
    const fn export_options() -> &'static [Self] {
        &[Self::Postman, Self::OpenApi]
    }

    /// Returns the display label for the selected format.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Postman => "Postman",
            Self::OpenApi => "OpenAPI",
        }
    }

    /// Returns the core collection format, if one is explicitly selected.
    const fn explicit_format(self) -> Option<CollectionFormat> {
        match self {
            Self::Auto => None,
            Self::Postman => Some(CollectionFormat::Postman),
            Self::OpenApi => Some(CollectionFormat::OpenApi),
        }
    }
}

/// Collection import/export dialog state.
#[derive(Debug)]
pub struct CollectionIoDialog {
    /// Whether this dialog imports or exports collections.
    pub mode: CollectionIoMode,
    /// Selected external collection format.
    pub format: CollectionIoFormat,
    /// Path to import from or export to.
    pub path: TextInput,
    /// Field currently receiving input.
    pub field: CollectionIoDialogField,
}

impl CollectionIoDialog {
    /// Creates a collection import/export dialog.
    fn new(mode: CollectionIoMode) -> Self {
        let mut path = TextInput::single_line();
        path.set_content(match mode {
            CollectionIoMode::Import => "collection.json",
            CollectionIoMode::Export => "spark-collections.postman.json",
        });
        let format = match mode {
            CollectionIoMode::Import => CollectionIoFormat::Auto,
            CollectionIoMode::Export => CollectionIoFormat::Postman,
        };
        Self {
            mode,
            format,
            path,
            field: CollectionIoDialogField::Path,
        }
    }

    /// Returns the selected format options for this dialog mode.
    fn options(&self) -> &'static [CollectionIoFormat] {
        match self.mode {
            CollectionIoMode::Import => CollectionIoFormat::import_options(),
            CollectionIoMode::Export => CollectionIoFormat::export_options(),
        }
    }

    /// Returns the selected core format for import or export.
    fn selected_format(&self) -> Option<CollectionFormat> {
        self.format.explicit_format()
    }

    /// Returns the trimmed file path when present.
    fn path(&self) -> Option<String> {
        let path_text = self.path.text();
        let path = path_text.trim();
        (!path.is_empty()).then(|| path.to_string())
    }

    /// Moves focus to the next dialog field.
    fn next_field(&mut self) {
        self.field = match self.field {
            CollectionIoDialogField::Format => CollectionIoDialogField::Path,
            CollectionIoDialogField::Path => CollectionIoDialogField::Format,
        };
    }

    /// Moves focus to the previous dialog field.
    fn previous_field(&mut self) {
        self.next_field();
    }

    /// Selects the next available format.
    fn next_format(&mut self) {
        let options = self.options();
        let current = options
            .iter()
            .position(|format| *format == self.format)
            .unwrap_or_default();
        self.format = options[(current + 1) % options.len()];
    }

    /// Selects the previous available format.
    fn previous_format(&mut self) {
        let options = self.options();
        let current = options
            .iter()
            .position(|format| *format == self.format)
            .unwrap_or_default();
        self.format = options[(current + options.len() - 1) % options.len()];
    }
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
    /// Query params editor.
    Params,
    /// Authentication helper editor.
    Auth,
    /// Headers editor.
    Headers,
    /// Body editor.
    Body,
    /// Pre-request script editor.
    PreRequestScript,
    /// Response test script editor.
    TestScript,
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
    /// Query parameter editor for this tab.
    pub params: TextInput,
    /// Authentication helper editor for this tab.
    pub auth: TextInput,
    /// Header editor for this tab.
    pub headers: TextInput,
    /// Body editor for this tab.
    pub body: TextInput,
    /// Pre-request script editor for this tab.
    pub pre_request_script: TextInput,
    /// Response test script editor for this tab.
    pub test_script: TextInput,
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
            params: TextInput::multi_line(),
            auth: TextInput::multi_line(),
            headers: TextInput::multi_line(),
            body: TextInput::multi_line(),
            pre_request_script: TextInput::multi_line(),
            test_script: TextInput::multi_line(),
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
    /// Saved request name when this pending request belongs to the collection runner.
    runner_request_name: Option<String>,
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
    /// Active collection import/export dialog.
    pub collection_io_dialog: Option<CollectionIoDialog>,
    /// Active environment manager dialog.
    pub environment_dialog: Option<EnvironmentDialog>,
    /// Active or most recent collection runner state.
    pub collection_run: Option<CollectionRun>,
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
            collection_io_dialog: None,
            environment_dialog: None,
            collection_run: None,
            pending_request: None,
            should_quit: false,
            status_message: "Ready.".to_string(),
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
        if self.environment_dialog.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
            self.handle_environment_dialog_key(key);
            return;
        }

        if self.collection_io_dialog.is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
            self.handle_collection_io_dialog_key(key);
            return;
        }

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

        if self.handle_control_shortcut(key) {
            return;
        }

        if self.handle_vim_action_key(key) {
            return;
        }

        match self.focus {
            Focus::History => self.handle_history_key(key),
            Focus::Search => self.handle_search_key(key),
            Focus::Method => self.handle_method_key(key),
            Focus::Url => self.handle_url_key(key),
            Focus::Params => self.handle_text_area_key(key, TextAreaTarget::Params),
            Focus::Auth => self.handle_text_area_key(key, TextAreaTarget::Auth),
            Focus::Headers => self.handle_text_area_key(key, TextAreaTarget::Headers),
            Focus::Body => self.handle_text_area_key(key, TextAreaTarget::Body),
            Focus::PreRequestScript => {
                self.handle_text_area_key(key, TextAreaTarget::PreRequestScript);
            }
            Focus::TestScript => self.handle_text_area_key(key, TextAreaTarget::TestScript),
            Focus::Response => self.handle_response_key(key),
        }
    }

    /// Handles global control-modified shortcuts.
    fn handle_control_shortcut(&mut self, key: KeyEvent) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        match key.code {
            KeyCode::Char('c') => self.should_quit = true,
            KeyCode::Char('s') => self.send_request(),
            KeyCode::Char('p') => self.open_save_dialog(),
            KeyCode::Char('l') => self.open_collection_io_dialog(CollectionIoMode::Import),
            KeyCode::Char('x') => self.open_collection_io_dialog(CollectionIoMode::Export),
            KeyCode::Char('g') => self.start_collection_run_from_selection(),
            KeyCode::Char('t') => self.open_new_request_tab(),
            KeyCode::Char('w') => self.close_active_request_tab(),
            KeyCode::Char('r') => self.open_rename_tab_dialog(),
            KeyCode::Left => self.select_previous_request_tab(),
            KeyCode::Right => self.select_next_request_tab(),
            KeyCode::Char('o') => self.toggle_sidebar_mode(),
            KeyCode::Char('e') => self.select_next_environment(),
            _ => return false,
        }
        true
    }

    /// Handles vim-style command keys when focus is not inside an editor.
    fn handle_vim_action_key(&mut self, key: KeyEvent) -> bool {
        if !vim_action_modifiers_are_allowed(key.modifiers) || self.focus_accepts_text() {
            return false;
        }

        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                true
            }
            KeyCode::Char('n') => {
                self.open_new_request_tab();
                true
            }
            KeyCode::Char('x') => {
                self.close_active_request_tab();
                true
            }
            KeyCode::Char('H') => {
                self.select_previous_request_tab();
                true
            }
            KeyCode::Char('L') => {
                self.select_next_request_tab();
                true
            }
            KeyCode::Char('r') => {
                self.open_rename_tab_dialog();
                true
            }
            KeyCode::Char('p') => {
                self.open_save_dialog();
                true
            }
            KeyCode::Char('I') => {
                self.open_collection_io_dialog(CollectionIoMode::Import);
                true
            }
            KeyCode::Char('X') => {
                self.open_collection_io_dialog(CollectionIoMode::Export);
                true
            }
            KeyCode::Char('E') => {
                self.open_environment_dialog();
                true
            }
            KeyCode::Char('R') => {
                self.start_collection_run_from_selection();
                true
            }
            KeyCode::Char('e') => {
                self.select_next_environment();
                true
            }
            _ => false,
        }
    }

    /// Returns whether the current focus should receive ordinary character input.
    fn focus_accepts_text(&self) -> bool {
        matches!(
            self.focus,
            Focus::Search
                | Focus::Url
                | Focus::Params
                | Focus::Auth
                | Focus::Headers
                | Focus::Body
                | Focus::PreRequestScript
                | Focus::TestScript
        )
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

    /// Returns whether the collection runner has a request queued or executing.
    #[must_use]
    pub fn is_collection_run_active(&self) -> bool {
        self.pending_request
            .as_ref()
            .is_some_and(|pending| pending.runner_request_name.is_some())
            || self
                .collection_run
                .as_ref()
                .is_some_and(CollectionRun::is_running)
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
            Focus::Url => Focus::Params,
            Focus::Params => Focus::Auth,
            Focus::Auth => Focus::Headers,
            Focus::Headers => Focus::Body,
            Focus::Body => Focus::PreRequestScript,
            Focus::PreRequestScript => Focus::TestScript,
            Focus::TestScript => Focus::Response,
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
            Focus::Params => Focus::Url,
            Focus::Auth => Focus::Params,
            Focus::Headers => Focus::Auth,
            Focus::Body => Focus::Headers,
            Focus::PreRequestScript => Focus::Body,
            Focus::TestScript => Focus::PreRequestScript,
            Focus::Response => Focus::TestScript,
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
            TextAreaTarget::Params => &mut self.active_tab_mut().params,
            TextAreaTarget::Auth => &mut self.active_tab_mut().auth,
            TextAreaTarget::Headers => &mut self.active_tab_mut().headers,
            TextAreaTarget::Body => &mut self.active_tab_mut().body,
            TextAreaTarget::PreRequestScript => &mut self.active_tab_mut().pre_request_script,
            TextAreaTarget::TestScript => &mut self.active_tab_mut().test_script,
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

    /// Opens the environment manager dialog.
    fn open_environment_dialog(&mut self) {
        self.method_dropdown_open = false;
        self.environment_dialog = Some(EnvironmentDialog::new(
            &self.environments,
            self.environment_index,
        ));
        self.status_message =
            "Environment manager: n new, d delete, Ctrl+S save, Esc close.".to_string();
    }

    /// Opens the collection import/export dialog.
    fn open_collection_io_dialog(&mut self, mode: CollectionIoMode) {
        self.method_dropdown_open = false;
        self.collection_io_dialog = Some(CollectionIoDialog::new(mode));
        self.status_message = match mode {
            CollectionIoMode::Import => {
                "Import collections: enter a file path, choose format, Enter imports.".to_string()
            }
            CollectionIoMode::Export => {
                "Export collections: enter a file path, choose format, Enter exports.".to_string()
            }
        };
    }

    /// Imports saved requests from an external collection file.
    fn import_collections_from(&mut self, path: &str, format: Option<CollectionFormat>) {
        match import_into_saved_requests(
            std::path::Path::new(path),
            &self.config.saved_requests_file,
            &mut self.saved_requests,
            format,
        ) {
            Ok(count) => {
                self.saved_index = self.saved_requests.len().saturating_sub(1);
                self.sidebar_mode = SidebarMode::Saved;
                self.select_latest_visible_saved_request();
                self.status_message = format!("Imported {count} saved requests from {path}");
            }
            Err(e) => {
                self.status_message = format!("Error importing collections: {e}");
            }
        }
    }

    /// Exports saved requests to an external collection file.
    fn export_collections_to(&mut self, path: &str, format: CollectionFormat) {
        match export_collection(std::path::Path::new(path), &self.saved_requests, format) {
            Ok(()) => {
                let count = self.saved_requests.len();
                self.status_message = format!("Exported {count} saved requests to {path}");
            }
            Err(e) => {
                self.status_message = format!("Error exporting collections: {e}");
            }
        }
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
        let params_text = format_query_params(&entry.query_params);
        let auth_text = format_auth(&entry.auth);
        let headers_text = format_headers(&entry.headers);
        let body_text = entry.body.clone().unwrap_or_default();
        let pre_request_script = entry.scripts.pre_request.clone();
        let test_script = entry.scripts.tests.clone();

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == method) {
            self.active_tab_mut().method_index = idx;
        }

        self.active_tab_mut().url.set_content(&url);
        self.active_tab_mut().params.set_content(&params_text);
        self.active_tab_mut().auth.set_content(&auth_text);
        self.active_tab_mut().headers.set_content(&headers_text);
        self.active_tab_mut().body.set_content(&body_text);
        self.active_tab_mut()
            .pre_request_script
            .set_content(&pre_request_script);
        self.active_tab_mut().test_script.set_content(&test_script);

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
        let params_text = format_query_params(&request.query_params);
        let auth_text = format_auth(&request.auth);
        let headers_text = format_headers(&request.headers);
        let body_text = request.body.clone().unwrap_or_default();
        let pre_request_script = request.scripts.pre_request.clone();
        let test_script = request.scripts.tests.clone();

        if let Some(idx) = HttpMethod::all().iter().position(|m| *m == method) {
            self.active_tab_mut().method_index = idx;
        }

        self.active_tab_mut().url.set_content(&url);
        self.active_tab_mut().params.set_content(&params_text);
        self.active_tab_mut().auth.set_content(&auth_text);
        self.active_tab_mut().headers.set_content(&headers_text);
        self.active_tab_mut().body.set_content(&body_text);
        self.active_tab_mut()
            .pre_request_script
            .set_content(&pre_request_script);
        self.active_tab_mut().test_script.set_content(&test_script);

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

    /// Handles key input while the environment manager dialog is active.
    fn handle_environment_dialog_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save_environment_dialog();
            return;
        }

        match key.code {
            KeyCode::Esc => {
                self.environment_dialog = None;
                self.status_message = "Environment manager closed.".to_string();
            }
            KeyCode::Tab => {
                if let Some(dialog) = &mut self.environment_dialog {
                    dialog.next_field();
                }
            }
            KeyCode::BackTab => {
                if let Some(dialog) = &mut self.environment_dialog {
                    dialog.previous_field();
                }
            }
            KeyCode::Char('n')
                if environment_dialog_accepts_command(self.environment_dialog.as_ref()) =>
            {
                if let Some(dialog) = &mut self.environment_dialog {
                    dialog.new_draft();
                }
                self.status_message = "New environment draft.".to_string();
            }
            KeyCode::Char('d')
                if environment_dialog_accepts_command(self.environment_dialog.as_ref()) =>
            {
                self.delete_selected_environment();
            }
            _ => self.handle_environment_dialog_field_key(key),
        }
    }

    /// Handles key input for the focused environment manager field.
    fn handle_environment_dialog_field_key(&mut self, key: KeyEvent) {
        let Some(field) = self.environment_dialog.as_ref().map(|dialog| dialog.field) else {
            return;
        };
        match field {
            EnvironmentDialogField::List => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.select_previous_environment_in_dialog(),
                KeyCode::Down | KeyCode::Char('j') => self.select_next_environment_in_dialog(),
                KeyCode::Enter => {
                    self.environment_index = self
                        .environment_dialog
                        .as_ref()
                        .and_then(|dialog| dialog.selected_index);
                    if let Some(idx) = self.environment_index
                        && let Some(environment) = self.environments.get(idx)
                    {
                        self.status_message = format!("Environment: {}", environment.name);
                    }
                }
                _ => {}
            },
            EnvironmentDialogField::Name => {
                if let Some(dialog) = &mut self.environment_dialog {
                    handle_single_line_text_input(&mut dialog.name, key);
                }
            }
            EnvironmentDialogField::Variables => {
                if let Some(dialog) = &mut self.environment_dialog {
                    match key.code {
                        KeyCode::Enter => dialog.variables.insert_newline(),
                        _ => handle_multi_line_text_input(&mut dialog.variables, key),
                    }
                }
            }
        }
    }

    /// Selects the previous environment in the manager dialog.
    fn select_previous_environment_in_dialog(&mut self) {
        let Some(dialog) = &mut self.environment_dialog else {
            return;
        };
        if self.environments.is_empty() {
            dialog.selected_index = None;
            dialog.load_selected(&self.environments);
            return;
        }
        let current = dialog.selected_index.unwrap_or(0);
        dialog.selected_index = Some(current.saturating_sub(1));
        dialog.load_selected(&self.environments);
    }

    /// Selects the next environment in the manager dialog.
    fn select_next_environment_in_dialog(&mut self) {
        let Some(dialog) = &mut self.environment_dialog else {
            return;
        };
        if self.environments.is_empty() {
            dialog.selected_index = None;
            dialog.load_selected(&self.environments);
            return;
        }
        let current = dialog.selected_index.unwrap_or(0);
        dialog.selected_index = Some((current + 1).min(self.environments.len() - 1));
        dialog.load_selected(&self.environments);
    }

    /// Saves the current environment manager fields to disk.
    fn save_environment_dialog(&mut self) {
        let Some(dialog) = &self.environment_dialog else {
            return;
        };
        let Some(environment) = dialog.environment() else {
            self.status_message = "Environment name is empty.".to_string();
            return;
        };

        let index = if let Some(idx) = dialog
            .selected_index
            .filter(|idx| *idx < self.environments.len())
        {
            self.environments[idx] = environment;
            idx
        } else {
            self.environments.push(environment);
            self.environments.len() - 1
        };

        match write_environments(&self.config.environments_file, &self.environments) {
            Ok(()) => {
                self.environment_index = Some(index);
                if let Some(dialog) = &mut self.environment_dialog {
                    dialog.selected_index = Some(index);
                    dialog.load_selected(&self.environments);
                }
                self.status_message =
                    format!("Saved environment: {}", self.environments[index].name);
            }
            Err(e) => {
                self.status_message = format!("Error saving environments: {e}");
            }
        }
    }

    /// Deletes the selected environment from the manager and disk.
    fn delete_selected_environment(&mut self) {
        let Some(dialog) = &self.environment_dialog else {
            return;
        };
        let Some(idx) = dialog
            .selected_index
            .filter(|idx| *idx < self.environments.len())
        else {
            self.status_message = "No environment selected.".to_string();
            return;
        };
        let removed = self.environments.remove(idx);
        self.environment_index = self.environment_index.and_then(|active| {
            if self.environments.is_empty() {
                None
            } else if active == idx {
                Some(idx.min(self.environments.len() - 1))
            } else if active > idx {
                Some(active - 1)
            } else {
                Some(active)
            }
        });
        match write_environments(&self.config.environments_file, &self.environments) {
            Ok(()) => {
                if let Some(dialog) = &mut self.environment_dialog {
                    dialog.selected_index = self.environment_index;
                    if dialog.selected_index.is_none() && !self.environments.is_empty() {
                        dialog.selected_index = Some(0);
                    }
                    dialog.load_selected(&self.environments);
                }
                self.status_message = format!("Deleted environment: {}", removed.name);
            }
            Err(e) => {
                self.status_message = format!("Error deleting environment: {e}");
            }
        }
    }

    /// Handles key input while the collection import/export dialog is active.
    fn handle_collection_io_dialog_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.collection_io_dialog = None;
                self.status_message = "Collection import/export cancelled.".to_string();
            }
            KeyCode::Enter => {
                let Some(dialog) = self.collection_io_dialog.take() else {
                    return;
                };
                let Some(path) = dialog.path() else {
                    self.status_message = "Collection file path is empty.".to_string();
                    return;
                };
                match dialog.mode {
                    CollectionIoMode::Import => {
                        self.import_collections_from(&path, dialog.selected_format());
                    }
                    CollectionIoMode::Export => {
                        let Some(format) = dialog.selected_format() else {
                            self.status_message =
                                "Choose Postman or OpenAPI for export.".to_string();
                            return;
                        };
                        self.export_collections_to(&path, format);
                    }
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(dialog) = &mut self.collection_io_dialog {
                    dialog.next_field();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(dialog) = &mut self.collection_io_dialog {
                    dialog.previous_field();
                }
            }
            KeyCode::Left => {
                if let Some(dialog) = &mut self.collection_io_dialog {
                    match dialog.field {
                        CollectionIoDialogField::Format => dialog.previous_format(),
                        CollectionIoDialogField::Path => dialog.path.move_left(),
                    }
                }
            }
            KeyCode::Right => {
                if let Some(dialog) = &mut self.collection_io_dialog {
                    match dialog.field {
                        CollectionIoDialogField::Format => dialog.next_format(),
                        CollectionIoDialogField::Path => dialog.path.move_right(),
                    }
                }
            }
            _ => {
                if let Some(dialog) = &mut self.collection_io_dialog
                    && dialog.field == CollectionIoDialogField::Path
                {
                    handle_single_line_text_input(&mut dialog.path, key);
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
        let Some(mut template) = self.current_template_request() else {
            self.status_message = "URL is empty — enter a URL and try again.".to_string();
            return;
        };
        let pre_request = match run_pre_request_script(
            &template.scripts.pre_request,
            self.active_environment(),
        ) {
            Ok(result) => result,
            Err(e) => {
                self.status_message = format!("Pre-request error: {e}");
                return;
            }
        };
        template.query_params.extend(pre_request.query_params);
        template.headers.extend(pre_request.headers);
        let environment = merged_environment(self.active_environment(), &pre_request.variables);
        let request = match Self::resolve_request_template(&template, environment.as_ref()) {
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
        self.pending_request = Some(PendingRequest {
            tab_index,
            request,
            runner_request_name: None,
        });
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
                let test_results = run_test_script(&request.scripts.tests, &response);
                if let Some(name) = &pending.runner_request_name {
                    self.complete_collection_run_request(CollectionRunResult {
                        name: name.clone(),
                        method: request.method,
                        url: request.url.clone(),
                        status_code: Some(response.status_code),
                        duration_ms: Some(response.duration_ms),
                        tests_passed: test_results.iter().filter(|test| test.passed).count(),
                        tests_total: test_results.len(),
                        error: None,
                    });
                } else {
                    self.status_message =
                        request_status_message(&request, response.status_code, &test_results);
                }
                self.history.push(entry);
                self.history_index = self.history.len() - 1;
                self.select_latest_visible_sidebar_item();
                if let Some(tab) = self.request_tabs.get_mut(pending.tab_index) {
                    tab.last_request = Some(request);
                    tab.response = Some(response);
                }
            }
            Err(e) => {
                if let Some(name) = pending.runner_request_name {
                    self.complete_collection_run_request(CollectionRunResult {
                        name,
                        method: request.method,
                        url: request.url,
                        status_code: None,
                        duration_ms: None,
                        tests_passed: 0,
                        tests_total: 0,
                        error: Some(e.to_string()),
                    });
                } else {
                    self.status_message = format!("Error: {e}");
                }
            }
        }

        if self
            .collection_run
            .as_ref()
            .is_some_and(CollectionRun::is_running)
            && self.pending_request.is_none()
        {
            self.queue_next_collection_run_request();
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
        let params_text = tab.params.text();
        let query_params = parse_query_params(params_text.as_ref());
        let auth_text = tab.auth.text();
        let auth = parse_auth(auth_text.as_ref());
        let headers_text = tab.headers.text();
        let headers = parse_headers(headers_text.as_ref());
        let body_text = tab.body.text();
        let body = if body_text.trim().is_empty() {
            None
        } else {
            Some(body_text.into_owned())
        };
        let pre_request_script = tab.pre_request_script.text().into_owned();
        let test_script = tab.test_script.text().into_owned();

        Some(HttpRequest {
            method,
            url: url.to_string(),
            query_params,
            auth,
            headers,
            body,
            scripts: RequestScripts {
                pre_request: pre_request_script,
                tests: test_script,
            },
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

    /// Starts a collection runner execution from the current saved sidebar selection.
    fn start_collection_run_from_selection(&mut self) {
        let Some(target) = self.selected_collection_run_target() else {
            self.status_message =
                "Select a saved request to run its folder or collection.".to_string();
            return;
        };
        let queue = self.saved_request_indices_for_target(&target);
        if queue.is_empty() {
            self.status_message = format!("No saved requests found for {}", target.label());
            return;
        }

        let total = queue.len();
        self.method_dropdown_open = false;
        self.focus = Focus::Response;
        self.active_tab_mut().response_tab = ResponseTab::Runner;
        self.active_tab_mut().response_scroll = 0;
        self.pending_request = None;
        self.collection_run = Some(CollectionRun::new(target.clone(), queue));
        self.status_message = format!("Running {} saved requests in {}", total, target.label());
        self.queue_next_collection_run_request();
    }

    /// Returns the runner target implied by the selected saved request.
    fn selected_collection_run_target(&self) -> Option<CollectionRunTarget> {
        if self.sidebar_mode != SidebarMode::Saved {
            return None;
        }
        if !self.filtered_saved_indices().contains(&self.saved_index) {
            return None;
        }
        let request = self.saved_requests.get(self.saved_index)?;
        Some(CollectionRunTarget {
            collection: request.collection.clone(),
            folder: request.folder.clone(),
        })
    }

    /// Returns saved request indexes included in a runner target.
    fn saved_request_indices_for_target(&self, target: &CollectionRunTarget) -> Vec<usize> {
        self.saved_requests
            .iter()
            .enumerate()
            .filter_map(|(idx, request)| saved_request_is_in_target(request, target).then_some(idx))
            .collect()
    }

    /// Queues the next collection runner request, or finalizes the run.
    fn queue_next_collection_run_request(&mut self) {
        let Some(run) = &mut self.collection_run else {
            return;
        };
        let Some(saved_index) = run.queue.first().copied() else {
            run.current_request = None;
            self.status_message = collection_run_status_message(run);
            return;
        };
        run.queue.remove(0);

        let Some(saved) = self.saved_requests.get(saved_index).cloned() else {
            return;
        };
        let request_name = saved.name.clone();
        if let Some(run) = &mut self.collection_run {
            run.current_request = Some(request_name.clone());
        }

        match self.resolved_saved_request(&saved) {
            Ok(request) => {
                self.status_message = format!("Running {}: {}", saved.name, request.url);
                self.pending_request = Some(PendingRequest {
                    tab_index: self.active_request_tab,
                    request,
                    runner_request_name: Some(request_name),
                });
            }
            Err(error) => {
                self.complete_collection_run_request(CollectionRunResult {
                    name: request_name,
                    method: saved.method,
                    url: saved.url,
                    status_code: None,
                    duration_ms: None,
                    tests_passed: 0,
                    tests_total: 0,
                    error: Some(error),
                });
                self.queue_next_collection_run_request();
            }
        }
    }

    /// Records one collection runner result.
    fn complete_collection_run_request(&mut self, result: CollectionRunResult) {
        if let Some(run) = &mut self.collection_run {
            run.current_request = None;
            run.results.push(result);
            self.status_message = collection_run_status_message(run);
        }
    }

    /// Builds a resolved request from a saved request template.
    fn resolved_saved_request(&self, saved: &SavedRequest) -> Result<HttpRequest, String> {
        let mut template = HttpRequest {
            method: saved.method,
            url: saved.url.clone(),
            query_params: saved.query_params.clone(),
            auth: saved.auth.clone(),
            headers: saved.headers.clone(),
            body: saved.body.clone(),
            scripts: saved.scripts.clone(),
        };
        let pre_request =
            run_pre_request_script(&template.scripts.pre_request, self.active_environment())?;
        template.query_params.extend(pre_request.query_params);
        template.headers.extend(pre_request.headers);
        let environment = merged_environment(self.active_environment(), &pre_request.variables);
        Self::resolve_request_template(&template, environment.as_ref())
    }

    /// Resolves environment variables in a request template.
    fn resolve_request_template(
        request: &HttpRequest,
        environment: Option<&Environment>,
    ) -> Result<HttpRequest, String> {
        let url = resolve_template(&request.url, environment).map_err(|e| e.to_string())?;
        let query_params = request
            .query_params
            .iter()
            .map(|param| {
                let key = resolve_template(&param.key, environment).map_err(|e| e.to_string())?;
                let value =
                    resolve_template(&param.value, environment).map_err(|e| e.to_string())?;
                Ok(QueryParam {
                    enabled: param.enabled,
                    key,
                    value,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let auth = resolve_auth_template(&request.auth, environment)?;
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
            query_params,
            auth,
            headers,
            body,
            scripts: request.scripts.clone(),
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

/// Effects produced by a pre-request script.
#[derive(Debug, Default, PartialEq, Eq)]
struct PreRequestEffects {
    /// Variables available to later pre-request lines and request templating.
    variables: Vec<(String, String)>,
    /// Headers appended to the outgoing request template.
    headers: Vec<(String, String)>,
    /// Query params appended to the outgoing request template.
    query_params: Vec<QueryParam>,
}

/// One response test outcome.
#[derive(Debug, PartialEq, Eq)]
struct ScriptTestResult {
    /// Human-readable assertion name.
    name: String,
    /// Whether the assertion passed.
    passed: bool,
    /// Failure detail for display and tests.
    message: String,
}

/// Runs a pre-request script against an optional base environment.
fn run_pre_request_script(
    script: &str,
    environment: Option<&Environment>,
) -> Result<PreRequestEffects, String> {
    let mut effects = PreRequestEffects::default();
    for (line_index, line) in script.lines().enumerate() {
        let line = line.trim();
        if script_line_is_inactive(line) {
            continue;
        }
        let (command, rest) = script_command(line);
        match command {
            "set" | "var" => {
                let (key, value) = split_script_assignment(rest)
                    .ok_or_else(|| script_line_error(line_index, "expected set name=value"))?;
                let value = resolve_script_value(value, environment, &effects.variables)?;
                effects.variables.push((key.to_string(), value));
            }
            "header" => {
                let (key, value) = split_script_header(rest)
                    .ok_or_else(|| script_line_error(line_index, "expected header Name: Value"))?;
                let value = resolve_script_value(value, environment, &effects.variables)?;
                effects.headers.push((key.to_string(), value));
            }
            "param" | "query" => {
                let (key, value) = split_script_assignment(rest)
                    .ok_or_else(|| script_line_error(line_index, "expected param name=value"))?;
                let value = resolve_script_value(value, environment, &effects.variables)?;
                effects
                    .query_params
                    .push(QueryParam::enabled(key.to_string(), value));
            }
            _ => return Err(script_line_error(line_index, "unknown pre-request command")),
        }
    }
    Ok(effects)
}

/// Runs response tests and returns one result per active assertion line.
fn run_test_script(script: &str, response: &HttpResponse) -> Vec<ScriptTestResult> {
    script
        .lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let line = line.trim();
            (!script_line_is_inactive(line)).then(|| run_test_line(line_index, line, response))
        })
        .collect()
}

/// Runs one response test line.
fn run_test_line(line_index: usize, line: &str, response: &HttpResponse) -> ScriptTestResult {
    let (command, rest) = script_command(line);
    match command {
        "status" => test_status(line_index, rest, response.status_code),
        "header" => test_header(line_index, rest, response),
        "body" => test_body(line_index, rest, &response.body),
        _ => ScriptTestResult {
            name: format!("line {}", line_index + 1),
            passed: false,
            message: "unknown test command".to_string(),
        },
    }
}

/// Tests a response status assertion.
fn test_status(line_index: usize, expected: &str, actual: u16) -> ScriptTestResult {
    let expected = expected.trim();
    let passed = status_matches(expected, actual);
    ScriptTestResult {
        name: format!("status {expected}"),
        passed,
        message: if passed {
            "passed".to_string()
        } else {
            format!(
                "line {} expected status {expected}, got {actual}",
                line_index + 1
            )
        },
    }
}

/// Tests a response header assertion.
fn test_header(line_index: usize, rest: &str, response: &HttpResponse) -> ScriptTestResult {
    let mut parts = rest.splitn(3, char::is_whitespace);
    let name = parts.next().unwrap_or("").trim();
    let operator = parts.next().unwrap_or("exists").trim();
    let expected = parts.next().unwrap_or("").trim();
    let actual = response
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str());
    let passed = match operator {
        "exists" => actual.is_some(),
        "contains" => actual.is_some_and(|value| value.contains(expected)),
        "equals" | "is" => actual == Some(expected),
        _ => false,
    };
    ScriptTestResult {
        name: format!("header {name} {operator}"),
        passed,
        message: if passed {
            "passed".to_string()
        } else {
            format!("line {} header assertion failed for {name}", line_index + 1)
        },
    }
}

/// Tests a response body assertion.
fn test_body(line_index: usize, rest: &str, body: &str) -> ScriptTestResult {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let operator = parts.next().unwrap_or("contains").trim();
    let expected = parts.next().unwrap_or("").trim();
    let passed = match operator {
        "contains" => body.contains(expected),
        "equals" | "is" => body == expected,
        _ => false,
    };
    ScriptTestResult {
        name: format!("body {operator}"),
        passed,
        message: if passed {
            "passed".to_string()
        } else {
            format!("line {} body assertion failed", line_index + 1)
        },
    }
}

/// Returns the status bar message for a completed request and its tests.
fn request_status_message(
    request: &HttpRequest,
    response_code: u16,
    tests: &[ScriptTestResult],
) -> String {
    let base = format!("✓ {} {}  —  {}", request.method, request.url, response_code);
    if tests.is_empty() {
        return base;
    }

    let passed = tests.iter().filter(|test| test.passed).count();
    let total = tests.len();
    if passed == total {
        format!("{base}  —  tests {passed}/{total} passed")
    } else {
        let first_failure = tests
            .iter()
            .find(|test| !test.passed)
            .map_or("test failed", |test| test.message.as_str());
        format!("{base}  —  tests {passed}/{total} passed: {first_failure}")
    }
}

/// Returns whether a script line should be skipped.
fn script_line_is_inactive(line: &str) -> bool {
    line.is_empty() || line.starts_with('#') || line.starts_with("//")
}

/// Splits the first command token from the rest of a script line.
fn script_command(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map_or((line, ""), |(command, rest)| (command.trim(), rest.trim()))
}

/// Splits `name=value` script syntax and rejects empty names.
fn split_script_assignment(text: &str) -> Option<(&str, &str)> {
    let (key, value) = text.split_once('=')?;
    let key = key.trim();
    (!key.is_empty()).then_some((key, value.trim()))
}

/// Splits `Name: Value` or `Name=Value` header syntax.
fn split_script_header(text: &str) -> Option<(&str, &str)> {
    text.split_once(':')
        .or_else(|| text.split_once('='))
        .and_then(|(key, value)| {
            let key = key.trim();
            (!key.is_empty()).then_some((key, value.trim()))
        })
}

/// Resolves a pre-request script value against script and base environment variables.
fn resolve_script_value(
    value: &str,
    environment: Option<&Environment>,
    variables: &[(String, String)],
) -> Result<String, String> {
    let merged = merged_environment(environment, variables);
    resolve_template(value, merged.as_ref()).map_err(|e| e.to_string())
}

/// Returns an environment with script variables taking precedence over base variables.
fn merged_environment(
    environment: Option<&Environment>,
    variables: &[(String, String)],
) -> Option<Environment> {
    if variables.is_empty() {
        return environment.cloned();
    }

    let mut merged = variables.to_vec();
    if let Some(environment) = environment {
        merged.extend(environment.variables.clone());
    }
    Some(Environment {
        name: "Script".to_string(),
        variables: merged,
    })
}

/// Returns whether a status assertion matches the actual status code.
fn status_matches(expected: &str, actual: u16) -> bool {
    let expected = expected.trim();
    if let Some(prefix) = expected.strip_suffix("xx") {
        return prefix
            .parse::<u16>()
            .is_ok_and(|hundreds| actual / 100 == hundreds);
    }
    if let Some((start, end)) = expected.split_once("..") {
        let Ok(start) = start.trim().parse::<u16>() else {
            return false;
        };
        let Ok(end) = end.trim().parse::<u16>() else {
            return false;
        };
        return (start..=end).contains(&actual);
    }
    expected.parse::<u16>() == Ok(actual)
}

/// Formats a one-based script line error.
fn script_line_error(line_index: usize, message: &str) -> String {
    format!("line {} {message}", line_index + 1)
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

/// Parses raw query param text into enabled/disabled key-value pairs.
fn parse_query_params(text: &str) -> Vec<QueryParam> {
    text.lines().filter_map(parse_query_param_line).collect()
}

/// Parses one query param editor line.
fn parse_query_param_line(line: &str) -> Option<QueryParam> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (enabled, content) = if let Some(disabled) = trimmed.strip_prefix('#') {
        (false, disabled.trim())
    } else {
        (true, trimmed)
    };
    if content.is_empty() {
        return None;
    }

    let (key, value) = content
        .split_once('=')
        .map_or((content, ""), |(key, value)| (key, value));
    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    Some(QueryParam {
        enabled,
        key: key.to_string(),
        value: value.trim().to_string(),
    })
}

/// Formats query params for the params editor.
fn format_query_params(params: &[QueryParam]) -> String {
    params
        .iter()
        .map(|param| {
            let prefix = if param.enabled { "" } else { "# " };
            format!("{prefix}{}={}", param.key, param.value)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses auth helper text into a request auth value.
fn parse_auth(text: &str) -> RequestAuth {
    let Some(mode) = auth_mode(text) else {
        return RequestAuth::None;
    };
    let fields = auth_fields(text);

    match mode.as_str() {
        "bearer" => fields
            .get("token")
            .filter(|token| !token.trim().is_empty())
            .map_or(RequestAuth::None, |token| RequestAuth::Bearer {
                token: token.trim().to_string(),
            }),
        "basic" => {
            let username = fields.get("username").map_or("", String::as_str).trim();
            let password = fields.get("password").map_or("", String::as_str).trim();
            if username.is_empty() {
                RequestAuth::None
            } else {
                RequestAuth::Basic {
                    username: username.to_string(),
                    password: password.to_string(),
                }
            }
        }
        "api-key-header" | "apikey-header" => parse_api_key_auth(&fields, ApiKeyLocation::Header),
        "api-key-query" | "apikey-query" => parse_api_key_auth(&fields, ApiKeyLocation::Query),
        _ => RequestAuth::None,
    }
}

/// Parses API key auth from keyed auth editor fields.
fn parse_api_key_auth(
    fields: &std::collections::BTreeMap<String, String>,
    location: ApiKeyLocation,
) -> RequestAuth {
    let key = fields.get("key").map_or("", String::as_str).trim();
    let value = fields.get("value").map_or("", String::as_str).trim();
    if key.is_empty() {
        RequestAuth::None
    } else {
        RequestAuth::ApiKey {
            key: key.to_string(),
            value: value.to_string(),
            location,
        }
    }
}

/// Returns the lowercased auth mode from editor text.
fn auth_mode(text: &str) -> Option<String> {
    text.lines()
        .flat_map(str::split_whitespace)
        .find(|token| !token.contains('='))
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_lowercase)
}

/// Returns key-value fields from auth editor text.
fn auth_fields(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut fields = std::collections::BTreeMap::new();
    for token in text.lines().flat_map(str::split_whitespace) {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        fields.insert(key, value.trim().to_string());
    }
    fields
}

/// Formats an auth helper for the auth editor.
fn format_auth(auth: &RequestAuth) -> String {
    match auth {
        RequestAuth::None => String::new(),
        RequestAuth::Bearer { token } => format!("bearer token={token}"),
        RequestAuth::Basic { username, password } => {
            format!("basic username={username} password={password}")
        }
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => {
            let mode = match location {
                ApiKeyLocation::Header => "api-key-header",
                ApiKeyLocation::Query => "api-key-query",
            };
            format!("{mode} key={key} value={value}")
        }
    }
}

/// Resolves auth helper templates using the active environment.
fn resolve_auth_template(
    auth: &RequestAuth,
    environment: Option<&Environment>,
) -> Result<RequestAuth, String> {
    match auth {
        RequestAuth::None => Ok(RequestAuth::None),
        RequestAuth::Bearer { token } => Ok(RequestAuth::Bearer {
            token: resolve_template(token, environment).map_err(|e| e.to_string())?,
        }),
        RequestAuth::Basic { username, password } => Ok(RequestAuth::Basic {
            username: resolve_template(username, environment).map_err(|e| e.to_string())?,
            password: resolve_template(password, environment).map_err(|e| e.to_string())?,
        }),
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => Ok(RequestAuth::ApiKey {
            key: resolve_template(key, environment).map_err(|e| e.to_string())?,
            value: resolve_template(value, environment).map_err(|e| e.to_string())?,
            location: *location,
        }),
    }
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

/// Applies common multi-line text editing keys to `input`.
fn handle_multi_line_text_input(input: &mut TextInput, key: KeyEvent) {
    match key.code {
        KeyCode::Up => input.move_up(),
        KeyCode::Down => input.move_down(),
        _ => handle_single_line_text_input(input, key),
    }
}

/// Returns whether the environment dialog should treat plain letters as commands.
fn environment_dialog_accepts_command(dialog: Option<&EnvironmentDialog>) -> bool {
    dialog.is_some_and(|dialog| dialog.field == EnvironmentDialogField::List)
}

/// Returns whether a key modifier set can trigger a vim-style action.
fn vim_action_modifiers_are_allowed(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
}

/// Parses environment variable editor text into key-value pairs.
fn parse_environment_variables(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(parse_environment_variable_line)
        .collect()
}

/// Parses one environment variable line.
fn parse_environment_variable_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

/// Formats environment variables for the editor.
fn format_environment_variables(variables: &[(String, String)]) -> String {
    variables
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns whether a history entry matches the search query.
fn history_matches(entry: &HistoryEntry, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    contains_case_insensitive(entry.method.as_str(), query)
        || contains_case_insensitive(&entry.url, query)
        || check_query_params(&entry.query_params, query)
        || check_auth(&entry.auth, query)
        || check_headers(&entry.headers, query)
        || check_scripts(&entry.scripts, query)
        || entry
            .body
            .as_deref()
            .is_some_and(|body| contains_case_insensitive(body, query))
}

/// Checks query parameters for a case-insensitive search match.
fn check_query_params(params: &[QueryParam], query: &str) -> bool {
    params.iter().any(|param| {
        contains_case_insensitive(&param.key, query)
            || contains_case_insensitive(&param.value, query)
    })
}

/// Checks auth helper fields for a case-insensitive search match.
fn check_auth(auth: &RequestAuth, query: &str) -> bool {
    match auth {
        RequestAuth::None => false,
        RequestAuth::Bearer { token } => {
            contains_case_insensitive("bearer", query) || contains_case_insensitive(token, query)
        }
        RequestAuth::Basic { username, password } => {
            contains_case_insensitive("basic", query)
                || contains_case_insensitive(username, query)
                || contains_case_insensitive(password, query)
        }
        RequestAuth::ApiKey {
            key,
            value,
            location,
        } => {
            let location = match location {
                ApiKeyLocation::Header => "header",
                ApiKeyLocation::Query => "query",
            };
            contains_case_insensitive("api key", query)
                || contains_case_insensitive(location, query)
                || contains_case_insensitive(key, query)
                || contains_case_insensitive(value, query)
        }
    }
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
        || check_query_params(&request.query_params, query)
        || check_auth(&request.auth, query)
        || check_headers(&request.headers, query)
        || check_scripts(&request.scripts, query)
        || request
            .body
            .as_deref()
            .is_some_and(|body| contains_case_insensitive(body, query))
}

/// Checks request scripts for a case-insensitive search match.
fn check_scripts(scripts: &RequestScripts, query: &str) -> bool {
    contains_case_insensitive(&scripts.pre_request, query)
        || contains_case_insensitive(&scripts.tests, query)
}

/// Returns a slash-separated collection/folder label for a saved request.
fn saved_location_label(request: &SavedRequest) -> String {
    request.folder.as_ref().map_or_else(
        || request.collection.clone(),
        |folder| format!("{}/{folder}", request.collection),
    )
}

/// Returns whether a saved request belongs to a collection runner target.
fn saved_request_is_in_target(request: &SavedRequest, target: &CollectionRunTarget) -> bool {
    request.collection == target.collection
        && target
            .folder
            .as_ref()
            .is_none_or(|folder| request.folder.as_ref() == Some(folder))
}

/// Returns a concise collection runner status message.
fn collection_run_status_message(run: &CollectionRun) -> String {
    let state = if run.is_running() {
        "running"
    } else {
        "finished"
    };
    format!(
        "Runner {state}: {} {}/{} complete, {} passed, {} failed",
        run.target.label(),
        run.completed(),
        run.total,
        run.passed(),
        run.failed()
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
            query_params: Vec::new(),
            auth: RequestAuth::None,
            headers,
            body: body.map(ToString::to_string),
            scripts: RequestScripts::default(),
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
            query_params: Vec::new(),
            auth: RequestAuth::None,
            headers: Vec::new(),
            body: body.map(ToString::to_string),
            scripts: RequestScripts::default(),
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

    /// Builds a shift-modified key event for input handler tests.
    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    /// Types text through the app key handler.
    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// Query param text parses enabled and disabled rows.
    #[test]
    fn query_param_text_parses_enabled_and_disabled_rows() {
        let params = parse_query_params("search=ada\n# archived=true\nempty=\n=skipped");

        assert_eq!(
            params,
            vec![
                QueryParam {
                    enabled: true,
                    key: "search".to_string(),
                    value: "ada".to_string(),
                },
                QueryParam {
                    enabled: false,
                    key: "archived".to_string(),
                    value: "true".to_string(),
                },
                QueryParam {
                    enabled: true,
                    key: "empty".to_string(),
                    value: String::new(),
                },
            ]
        );
    }

    /// Query params round-trip through the editor format.
    #[test]
    fn query_params_format_for_editor() {
        let params = vec![
            QueryParam::enabled("search".to_string(), "ada".to_string()),
            QueryParam {
                enabled: false,
                key: "archived".to_string(),
                value: "true".to_string(),
            },
        ];

        assert_eq!(format_query_params(&params), "search=ada\n# archived=true");
    }

    /// Auth helper text parses common auth modes.
    #[test]
    fn auth_text_parses_supported_modes() {
        assert_eq!(
            parse_auth("bearer token={{token}}"),
            RequestAuth::Bearer {
                token: "{{token}}".to_string(),
            }
        );
        assert_eq!(
            parse_auth("basic username=ada password=secret"),
            RequestAuth::Basic {
                username: "ada".to_string(),
                password: "secret".to_string(),
            }
        );
        assert_eq!(
            parse_auth("api-key-query key=api_key value=abc123"),
            RequestAuth::ApiKey {
                key: "api_key".to_string(),
                value: "abc123".to_string(),
                location: ApiKeyLocation::Query,
            }
        );
    }

    /// Auth helpers round-trip through the editor format.
    #[test]
    fn auth_formats_for_editor() {
        assert_eq!(
            format_auth(&RequestAuth::ApiKey {
                key: "X-API-Key".to_string(),
                value: "{{api_key}}".to_string(),
                location: ApiKeyLocation::Header,
            }),
            "api-key-header key=X-API-Key value={{api_key}}"
        );
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

    /// Search matches method, URL, query params, headers, and request body.
    #[test]
    fn history_search_matches_request_parts_case_insensitively() {
        let mut matching = history_entry(
            HttpMethod::Post,
            "https://example.com/orders",
            vec![("Authorization".to_string(), "Bearer token".to_string())],
            Some("{\"status\":\"pending\"}"),
        );
        matching.query_params = vec![QueryParam::enabled(
            "state".to_string(),
            "pending-review".to_string(),
        )];
        let mut app = app_with_history(vec![
            matching,
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

        app.history_search.set_content("state");
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

    /// Vim-style action keys work when focus is not editing text.
    #[test]
    fn vim_action_keys_work_outside_text_inputs() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::History;

        app.handle_key(key(KeyCode::Char('n')));

        assert_eq!(app.request_tabs.len(), 2);
        assert_eq!(app.active_request_tab, 1);

        app.focus = Focus::History;
        app.handle_key(key(KeyCode::Char('H')));

        assert_eq!(app.active_request_tab, 0);

        app.focus = Focus::History;
        app.handle_key(key(KeyCode::Char('L')));

        assert_eq!(app.active_request_tab, 1);

        app.focus = Focus::History;
        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.request_tabs.len(), 1);
    }

    /// Vim-style action keys do not steal ordinary text input from editors.
    #[test]
    fn vim_action_keys_do_not_run_inside_text_inputs() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::Url;

        app.handle_key(key(KeyCode::Char('n')));
        app.handle_key(key(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.request_tabs.len(), 1);
        assert_eq!(app.active_tab().url.content(), "npx");
        assert!(app.save_dialog.is_none());
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

    /// Ctrl+L opens the collection import dialog.
    #[test]
    fn ctrl_l_opens_collection_import_dialog() {
        let mut app = app_with_history(Vec::new());

        app.handle_key(ctrl_key(KeyCode::Char('l')));

        let dialog = app
            .collection_io_dialog
            .as_ref()
            .expect("collection import dialog should open");
        assert_eq!(dialog.mode, CollectionIoMode::Import);
        assert_eq!(dialog.format, CollectionIoFormat::Auto);
        assert_eq!(dialog.field, CollectionIoDialogField::Path);
    }

    /// Ctrl+X opens the collection export dialog.
    #[test]
    fn ctrl_x_opens_collection_export_dialog() {
        let mut app = app_with_history(Vec::new());

        app.handle_key(ctrl_key(KeyCode::Char('x')));

        let dialog = app
            .collection_io_dialog
            .as_ref()
            .expect("collection export dialog should open");
        assert_eq!(dialog.mode, CollectionIoMode::Export);
        assert_eq!(dialog.format, CollectionIoFormat::Postman);
        assert_eq!(dialog.field, CollectionIoDialogField::Path);
    }

    /// Import dialog reads external requests into saved collections.
    #[test]
    fn collection_import_dialog_imports_saved_requests() {
        let mut app = app_with_history(Vec::new());
        let import_path = std::env::temp_dir().join(format!("spark-tui-import-{}.json", test_id()));
        std::fs::write(
            &import_path,
            r#"{
              "info": {"name": "Identity"},
              "item": [{
                "name": "List users",
                "request": {
                  "method": "GET",
                  "url": {"raw": "https://api.example.com/users"}
                }
              }]
            }"#,
        )
        .expect("import fixture should write");

        app.handle_key(ctrl_key(KeyCode::Char('l')));
        app.collection_io_dialog
            .as_mut()
            .expect("collection import dialog should open")
            .path
            .set_content(import_path.to_string_lossy().as_ref());
        app.handle_key(key(KeyCode::Enter));

        assert!(app.collection_io_dialog.is_none());
        assert_eq!(app.saved_requests.len(), 1);
        assert_eq!(app.saved_requests[0].collection, "Identity");
        assert_eq!(app.saved_requests[0].url, "https://api.example.com/users");
        assert_eq!(
            app.status_message,
            format!(
                "Imported 1 saved requests from {}",
                import_path.to_string_lossy()
            )
        );
        let _ = std::fs::remove_file(import_path);
        let _ = std::fs::remove_file(&app.config.saved_requests_file);
    }

    /// Export dialog writes saved requests in the selected external format.
    #[test]
    fn collection_export_dialog_exports_saved_requests() {
        let mut app = app_with_saved_requests(vec![saved_request_in(
            "List users",
            HttpMethod::Get,
            "https://api.example.com/users",
            "Identity",
            Some("Users"),
        )]);
        let export_path = std::env::temp_dir().join(format!("spark-tui-export-{}.json", test_id()));

        app.handle_key(ctrl_key(KeyCode::Char('x')));
        app.collection_io_dialog
            .as_mut()
            .expect("collection export dialog should open")
            .path
            .set_content(export_path.to_string_lossy().as_ref());
        app.handle_key(key(KeyCode::Enter));

        let exported = std::fs::read_to_string(&export_path).expect("export should write");
        assert!(app.collection_io_dialog.is_none());
        assert!(exported.contains("https://schema.getpostman.com"));
        assert_eq!(
            app.status_message,
            format!(
                "Exported 1 saved requests to {}",
                export_path.to_string_lossy()
            )
        );
        let _ = std::fs::remove_file(export_path);
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

    /// Runner target follows the selected saved request folder when present.
    #[test]
    fn collection_runner_targets_selected_folder() {
        let mut app = app_with_saved_requests(vec![
            saved_request_in(
                "List users",
                HttpMethod::Get,
                "https://example.com/users",
                "Identity",
                Some("Users"),
            ),
            saved_request_in(
                "Create user",
                HttpMethod::Post,
                "https://example.com/users",
                "Identity",
                Some("Users"),
            ),
            saved_request_in(
                "List tokens",
                HttpMethod::Get,
                "https://example.com/tokens",
                "Identity",
                Some("Tokens"),
            ),
        ]);
        app.saved_index = 0;

        app.handle_key(ctrl_key(KeyCode::Char('g')));

        let run = app.collection_run.as_ref().expect("runner should start");
        assert_eq!(run.target.collection, "Identity");
        assert_eq!(run.target.folder.as_deref(), Some("Users"));
        assert_eq!(run.total, 2);
        assert_eq!(run.current_request.as_deref(), Some("List users"));
        assert_eq!(run.queue, vec![1]);
        assert!(app.pending_request.is_some());
        assert_eq!(app.active_tab().response_tab, ResponseTab::Runner);
    }

    /// Runner target expands to the whole collection for root-level requests.
    #[test]
    fn collection_runner_targets_collection_when_selected_request_has_no_folder() {
        let mut app = app_with_saved_requests(vec![
            saved_request_in(
                "Health",
                HttpMethod::Get,
                "https://example.com/health",
                "Identity",
                None,
            ),
            saved_request_in(
                "List users",
                HttpMethod::Get,
                "https://example.com/users",
                "Identity",
                Some("Users"),
            ),
            saved_request_in(
                "List orders",
                HttpMethod::Get,
                "https://example.com/orders",
                "Commerce",
                None,
            ),
        ]);
        app.saved_index = 0;

        app.handle_key(ctrl_key(KeyCode::Char('g')));

        let run = app.collection_run.as_ref().expect("runner should start");
        assert_eq!(run.target.collection, "Identity");
        assert_eq!(run.target.folder, None);
        assert_eq!(run.total, 2);
        assert_eq!(run.current_request.as_deref(), Some("Health"));
        assert_eq!(run.queue, vec![1]);
    }

    /// Runner resolves saved request templates with the active environment.
    #[test]
    fn collection_runner_resolves_saved_request_environment_variables() {
        let mut saved = saved_request_in(
            "List users",
            HttpMethod::Get,
            "{{base_url}}/users",
            "Identity",
            Some("Users"),
        );
        saved.auth = RequestAuth::Bearer {
            token: "{{token}}".to_string(),
        };
        let mut app = app_with_saved_requests(vec![saved]);
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.saved_index = 0;

        app.handle_key(ctrl_key(KeyCode::Char('g')));

        let pending = app
            .pending_request
            .as_ref()
            .expect("runner request should be queued");
        assert_eq!(pending.request.url, "http://localhost:8080/users");
        assert_eq!(
            pending.request.auth,
            RequestAuth::Bearer {
                token: "abc123".to_string()
            }
        );
        assert_eq!(pending.runner_request_name.as_deref(), Some("List users"));
    }

    /// Runner records pass and failure counts from completed results.
    #[test]
    fn collection_runner_counts_completed_results() {
        let mut app = app_with_saved_requests(vec![saved_request_in(
            "List users",
            HttpMethod::Get,
            "https://example.com/users",
            "Identity",
            Some("Users"),
        )]);
        app.saved_index = 0;
        app.handle_key(ctrl_key(KeyCode::Char('g')));

        app.complete_collection_run_request(CollectionRunResult {
            name: "List users".to_string(),
            method: HttpMethod::Get,
            url: "https://example.com/users".to_string(),
            status_code: Some(200),
            duration_ms: Some(15),
            tests_passed: 1,
            tests_total: 1,
            error: None,
        });

        let run = app.collection_run.as_ref().expect("runner should exist");
        assert_eq!(run.completed(), 1);
        assert_eq!(run.passed(), 1);
        assert_eq!(run.failed(), 0);
        assert_eq!(
            app.status_message,
            "Runner finished: Identity/Users 1/1 complete, 1 passed, 0 failed"
        );
    }

    /// Saving the current composer pins a reusable request and selects saved mode.
    #[test]
    fn save_current_request_adds_saved_request() {
        let mut app = app_with_history(Vec::new());
        app.active_tab_mut()
            .url
            .set_content("https://example.com/users");
        app.active_tab_mut()
            .params
            .set_content("search=ada\n# archived=true");
        app.active_tab_mut()
            .auth
            .set_content("bearer token={{token}}");
        app.active_tab_mut()
            .pre_request_script
            .set_content("set user_id=42");
        app.active_tab_mut().test_script.set_content("status 2xx");

        app.save_current_request_to(DEFAULT_COLLECTION.to_string(), None);

        assert_eq!(app.sidebar_mode, SidebarMode::Saved);
        assert_eq!(app.saved_requests.len(), 1);
        assert_eq!(app.saved_requests[0].name, "GET https://example.com/users");
        assert_eq!(
            app.saved_requests[0].query_params,
            vec![
                QueryParam::enabled("search".to_string(), "ada".to_string()),
                QueryParam {
                    enabled: false,
                    key: "archived".to_string(),
                    value: "true".to_string(),
                },
            ]
        );
        assert_eq!(
            app.saved_requests[0].auth,
            RequestAuth::Bearer {
                token: "{{token}}".to_string(),
            }
        );
        assert_eq!(app.saved_requests[0].scripts.pre_request, "set user_id=42");
        assert_eq!(app.saved_requests[0].scripts.tests, "status 2xx");
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
        let mut saved = saved_request(
            "Create order",
            HttpMethod::Post,
            "https://example.com/orders",
            Some("{\"status\":\"pending\"}"),
        );
        saved.query_params = vec![QueryParam::enabled(
            "status".to_string(),
            "pending".to_string(),
        )];
        saved.auth = RequestAuth::Bearer {
            token: "{{token}}".to_string(),
        };
        saved.scripts = RequestScripts {
            pre_request: "set order_id=42".to_string(),
            tests: "status 200".to_string(),
        };
        let mut app = app_with_saved_requests(vec![saved]);

        app.load_from_saved_request();

        assert_eq!(app.current_method(), &HttpMethod::Post);
        assert_eq!(app.active_tab().url.content(), "https://example.com/orders");
        assert_eq!(app.active_tab().params.content(), "status=pending");
        assert_eq!(app.active_tab().auth.content(), "bearer token={{token}}");
        assert_eq!(app.active_tab().body.content(), "{\"status\":\"pending\"}");
        assert_eq!(
            app.active_tab().pre_request_script.content(),
            "set order_id=42"
        );
        assert_eq!(app.active_tab().test_script.content(), "status 200");
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

    /// Vim-style `E` opens the environment manager without changing active environment.
    #[test]
    fn vim_e_opens_environment_manager() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.focus = Focus::History;

        app.handle_key(key(KeyCode::Char('E')));

        let dialog = app
            .environment_dialog
            .as_ref()
            .expect("environment dialog should open");
        assert_eq!(dialog.selected_index, Some(0));
        assert_eq!(dialog.name.content(), "Local");
        assert_eq!(
            dialog.variables.content(),
            "base_url=http://localhost:8080\ntoken=abc123"
        );
    }

    /// Shift-modified uppercase `E` opens the environment manager in real terminals.
    #[test]
    fn shifted_vim_e_opens_environment_manager() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::History;

        app.handle_key(shift_key(KeyCode::Char('E')));

        assert!(app.environment_dialog.is_some());
    }

    /// Environment manager can create and persist a new environment.
    #[test]
    fn environment_manager_saves_new_environment() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::History;

        app.handle_key(key(KeyCode::Char('E')));
        app.handle_key(key(KeyCode::Char('n')));
        type_text(&mut app, "Staging");
        app.handle_key(key(KeyCode::Tab));
        type_text(&mut app, "base_url=https://staging.example.com");
        app.handle_key(key(KeyCode::Enter));
        type_text(&mut app, "token=staging-token");
        app.handle_key(ctrl_key(KeyCode::Char('s')));

        assert_eq!(app.environments.len(), 1);
        assert_eq!(app.environment_index, Some(0));
        assert_eq!(app.environments[0].name, "Staging");
        assert_eq!(
            app.environments[0].variables,
            vec![
                (
                    "base_url".to_string(),
                    "https://staging.example.com".to_string()
                ),
                ("token".to_string(), "staging-token".to_string()),
            ]
        );
        assert_eq!(
            load_environments(&app.config.environments_file),
            app.environments
        );
        assert_eq!(app.status_message, "Saved environment: Staging");
        let _ = std::fs::remove_file(&app.config.environments_file);
    }

    /// Environment manager edits the selected environment in place.
    #[test]
    fn environment_manager_edits_selected_environment() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.focus = Focus::History;

        app.handle_key(key(KeyCode::Char('E')));
        app.handle_key(key(KeyCode::Tab));
        app.environment_dialog
            .as_mut()
            .expect("environment dialog should open")
            .name
            .set_content("Dev");
        app.handle_key(ctrl_key(KeyCode::Char('s')));

        assert_eq!(app.environments.len(), 1);
        assert_eq!(app.environments[0].name, "Dev");
        assert_eq!(app.environment_index, Some(0));
        assert_eq!(
            load_environments(&app.config.environments_file),
            app.environments
        );
        let _ = std::fs::remove_file(&app.config.environments_file);
    }

    /// Environment manager deletes the selected environment and clamps active selection.
    #[test]
    fn environment_manager_deletes_selected_environment() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![
            environment("Local", "http://localhost:8080"),
            environment("Prod", "https://api.example.com"),
        ];
        app.environment_index = Some(1);
        app.focus = Focus::History;

        app.handle_key(key(KeyCode::Char('E')));
        app.handle_key(key(KeyCode::Char('d')));

        assert_eq!(app.environments.len(), 1);
        assert_eq!(app.environments[0].name, "Local");
        assert_eq!(app.environment_index, Some(0));
        assert_eq!(
            load_environments(&app.config.environments_file),
            app.environments
        );
        assert_eq!(app.status_message, "Deleted environment: Prod");
        let _ = std::fs::remove_file(&app.config.environments_file);
    }

    /// Enter in the URL field queues a request with headers, query params, and auth.
    #[test]
    fn enter_in_url_queues_request_with_headers_query_params_and_auth() {
        let mut app = app_with_history(Vec::new());
        app.focus = Focus::Url;
        app.active_tab_mut()
            .url
            .set_content("https://example.com/search");
        app.active_tab_mut()
            .params
            .set_content("q=ada lovelace\n# archived=true");
        app.active_tab_mut()
            .auth
            .set_content("api-key-query key=api_key value=abc123");
        app.active_tab_mut()
            .headers
            .set_content("Authorization: Bearer token");

        app.handle_key(key(KeyCode::Enter));

        let request = app
            .pending_request
            .as_ref()
            .expect("request should be queued");
        assert_eq!(request.request.url, "https://example.com/search");
        assert_eq!(
            request.request.query_params,
            vec![
                QueryParam::enabled("q".to_string(), "ada lovelace".to_string()),
                QueryParam {
                    enabled: false,
                    key: "archived".to_string(),
                    value: "true".to_string(),
                },
            ]
        );
        assert_eq!(
            request.request.headers,
            vec![("Authorization".to_string(), "Bearer token".to_string())]
        );
        assert_eq!(
            request.request.auth,
            RequestAuth::ApiKey {
                key: "api_key".to_string(),
                value: "abc123".to_string(),
                location: ApiKeyLocation::Query,
            }
        );
        assert_eq!(
            request.request.url_with_query_params(),
            "https://example.com/search?q=ada%20lovelace&api_key=abc123"
        );
    }

    /// Pre-request scripts can add variables, headers, and query params before send.
    #[test]
    fn send_request_applies_pre_request_script_before_resolution() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.active_tab_mut()
            .url
            .set_content("{{base_url}}/users/{{user_id}}");
        app.active_tab_mut()
            .pre_request_script
            .set_content("set user_id=42\nheader X-Trace: {{user_id}}\nparam debug=true");

        app.send_request();

        let request = app
            .pending_request
            .as_ref()
            .expect("resolved request should be queued");
        assert_eq!(request.request.url, "http://localhost:8080/users/42");
        assert_eq!(
            request.request.headers,
            vec![("X-Trace".to_string(), "42".to_string())]
        );
        assert_eq!(
            request.request.query_params,
            vec![QueryParam::enabled("debug".to_string(), "true".to_string())]
        );
    }

    /// Response test scripts report individual assertion results.
    #[test]
    fn response_test_script_checks_status_headers_and_body() {
        let response = HttpResponse {
            status_code: 201,
            status_text: "Created".to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: "{\"name\":\"Ada\"}".to_string(),
            duration_ms: 25,
        };

        let results = run_test_script(
            "status 2xx\nheader Content-Type contains json\nbody contains Ada",
            &response,
        );

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| result.passed));
    }

    /// Sending resolves variables without mutating the composer template.
    #[test]
    fn send_request_resolves_environment_variables() {
        let mut app = app_with_history(Vec::new());
        app.environments = vec![environment("Local", "http://localhost:8080")];
        app.environment_index = Some(0);
        app.active_tab_mut().url.set_content("{{base_url}}/users");
        app.active_tab_mut()
            .auth
            .set_content("bearer token={{token}}");
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
            request.request.headers_with_auth(),
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
