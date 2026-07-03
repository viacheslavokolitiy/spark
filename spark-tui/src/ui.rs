//! Rendering functions for the Spark terminal interface.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use spark_core::history::{HistoryEntry, relative_time_label};
use spark_core::http::{HttpMethod, HttpRequest, HttpResponse};
use spark_core::saved::SavedRequest;
use std::borrow::Cow;
use tui_piechart::{
    LegendAlignment, LegendLayout, LegendPosition, PieChart, PieSlice, Resolution, symbols,
};

use crate::app::{
    App, CollectionIoDialogField, CollectionIoMode, Focus, ResponseTab, SaveDialogField,
    SidebarMode,
};

/// Millisecond threshold at which durations switch to seconds.
const MS_IN_SECONDS: u128 = 1_000;
/// Header name used by servers to set response cookies.
const SET_COOKIE_HEADER: &str = "set-cookie";
/// Header name used by servers to describe cache policy.
const CACHE_CONTROL_HEADER: &str = "cache-control";
/// Legacy header name used by servers to disable cache.
const PRAGMA_HEADER: &str = "pragma";
/// Always-visible footer navigation key help.
const VIM_NAV_KEY_HELP: &str = "j/k move  h/l switch  H/L tabs  Tab focus  Enter open/send  q quit";
/// Always-visible footer action key help.
const VIM_ACTION_KEY_HELP: &str = "n new  x close  r rename  p save  I import  X export  e env";
/// Always-visible legacy control shortcut help.
const CONTROL_KEY_HELP: &str = "^S send ^P save ^L import ^X export ^T new ^W close ^R ren ^O side";

/// Operating-system family used to label visible key help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyHelpPlatform {
    /// Apple macOS terminals.
    MacOs,
    /// Windows terminals.
    Windows,
    /// Linux and other Unix-like terminals.
    Linux,
}

impl KeyHelpPlatform {
    /// Returns the platform label used for vim-style navigation bindings.
    const fn nav_label(self) -> &'static str {
        match self {
            Self::MacOs => "mac nav",
            Self::Windows => "win nav",
            Self::Linux => "linux nav",
        }
    }

    /// Returns the platform label used for vim-style action bindings.
    const fn vim_label(self) -> &'static str {
        match self {
            Self::MacOs => "mac vim",
            Self::Windows => "win vim",
            Self::Linux => "linux vim",
        }
    }
}

/// Returns the key-help platform for the current build target.
fn current_key_help_platform() -> KeyHelpPlatform {
    if cfg!(target_os = "macos") {
        KeyHelpPlatform::MacOs
    } else if cfg!(target_os = "windows") {
        KeyHelpPlatform::Windows
    } else {
        KeyHelpPlatform::Linux
    }
}

// ── Color helpers ────────────────────────────────────────────────────────────

/// Returns the display colour for an HTTP method per the design spec.
fn method_color(method: HttpMethod) -> Color {
    match method {
        HttpMethod::Get | HttpMethod::Head => Color::Green,
        HttpMethod::Post => Color::Yellow,
        HttpMethod::Put => Color::Cyan,
        HttpMethod::Patch => Color::Magenta,
        HttpMethod::Delete => Color::Rgb(255, 140, 0),
        HttpMethod::Options => Color::Rgb(255, 105, 180),
    }
}

/// Returns a colour appropriate for an HTTP status code.
fn status_color(code: u16) -> Color {
    match code {
        100..=199 => Color::Cyan,
        200..=299 => Color::Green,
        300..=399 => Color::Yellow,
        400..=499 => Color::Red,
        500..=599 => Color::Magenta,
        _ => Color::White,
    }
}

/// Formats a round-trip duration for display in the response title.
///
/// Values under one second are shown in milliseconds (`123ms`); values one
/// second or above are shown with one decimal place in seconds (`1.2s`).
fn format_duration(ms: u128) -> String {
    if ms < MS_IN_SECONDS {
        format!("{ms}ms")
    } else {
        let millis = u64::try_from(ms).unwrap_or(u64::MAX);
        let seconds = std::time::Duration::from_millis(millis).as_secs_f64();
        format!("{seconds:.1}s")
    }
}

/// Converts a cursor index to a terminal coordinate offset.
fn cursor_offset(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// Border style for a focused vs unfocused block.
fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Renders the full application UI into `frame`.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Root split: content area + status row + always-visible key help rows.
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);

    let main_area = root[0];
    let status_area = root[1];
    let key_help_area = root[2];

    // Horizontal split: sidebar (25%) | central pane (75%)
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(main_area);

    let sidebar_area = columns[0];
    let central_area = columns[1];

    // Central split: request tabs | composer (50%) | response (50%)
    let central_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Percentage(65),
            Constraint::Min(6),
        ])
        .split(central_area);

    let request_tabs_area = central_rows[0];
    let composer_area = central_rows[1];
    let response_area = central_rows[2];

    render_sidebar(frame, app, sidebar_area);
    render_request_tabs(frame, app, request_tabs_area);
    render_composer(frame, app, composer_area);
    render_response(frame, app, response_area);
    render_status(frame, app, status_area);
    render_key_help(frame, key_help_area);
    render_save_dialog(frame, app, area);
    render_rename_tab_dialog(frame, app, area);
    render_collection_io_dialog(frame, app, area);
}

/// Renders the open request tab selector.
fn render_request_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = app
        .request_tabs
        .iter()
        .enumerate()
        .map(|(idx, tab)| {
            let marker = if app.is_request_tab_sending(idx) {
                "* "
            } else {
                ""
            };
            format!(" {}{} ", marker, tab.title(idx))
        })
        .collect::<Vec<_>>();

    let tabs = Tabs::new(titles)
        .select(app.active_request_tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(tabs, area);
}

// ── Sidebar ──────────────────────────────────────────────────────────────────

/// Renders the history search field and filtered request history list.
fn render_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    render_sidebar_tabs(frame, app, rows[0]);
    render_history_search(frame, app, rows[1]);
    match app.sidebar_mode {
        SidebarMode::History => render_history(frame, app, rows[2]),
        SidebarMode::Saved => render_saved_requests(frame, app, rows[2]),
    }
}

/// Renders the sidebar mode selector.
fn render_sidebar_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let selected_tab = match app.sidebar_mode {
        SidebarMode::History => 0,
        SidebarMode::Saved => 1,
    };

    let tabs = Tabs::new(["History", "Saved"])
        .select(selected_tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(tabs, area);
}

/// Renders the request history search input.
fn render_history_search(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Search;
    let block = Block::default()
        .title(" Search ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let search_text = app.history_search.text();
    let para = Paragraph::new(search_text.as_ref()).block(block);
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(app.history_search.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = area.y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

/// Renders the filtered request history list.
fn render_history(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::History;

    // Build the visual item list, inserting a separator row each time the
    // relative-time bucket changes.  `visual_map[i]` is `Some(history_idx)`
    // for real entries and `None` for separator rows.
    let mut items: Vec<ListItem> = Vec::new();
    let mut visual_map: Vec<Option<usize>> = Vec::new();
    let mut current_label: Option<String> = None;
    let filtered_indices = app.filtered_history_indices();

    for idx in &filtered_indices {
        let entry = &app.history[*idx];
        let label = relative_time_label(&entry.timestamp);

        if current_label.as_deref() != Some(label.as_str()) {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("  {label}"),
                Style::default()
                    .fg(Color::Rgb(120, 120, 120))
                    .add_modifier(Modifier::ITALIC),
            ))));
            visual_map.push(None);
            current_label = Some(label);
        }

        let color = method_color(entry.method);
        let method_span = Span::styled(
            format!("{:<7}", entry.method.as_str()),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
        let url_span = Span::raw(entry.url.as_str());
        items.push(ListItem::new(Line::from(vec![method_span, url_span])));
        visual_map.push(Some(*idx));
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No matching requests",
            Style::default().fg(Color::DarkGray),
        ))));
        visual_map.push(None);
    }

    // Map the logical history_index back to its visual position.
    let visual_selected = if filtered_indices.is_empty() {
        None
    } else {
        visual_map
            .iter()
            .position(|v| *v == Some(app.history_index))
    };

    let block = Block::default()
        .title(" History  (Ctrl+O: saved) ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let mut list_state = ListState::default();
    list_state.select(visual_selected);

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Renders the filtered saved request list.
fn render_saved_requests(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::History;
    let filtered_indices = app.filtered_saved_indices();
    let mut items: Vec<ListItem> = Vec::new();
    let mut visual_map: Vec<Option<usize>> = Vec::new();
    let mut current_collection: Option<String> = None;
    let mut current_folder: Option<Option<String>> = None;

    for idx in &filtered_indices {
        let request = &app.saved_requests[*idx];
        if current_collection.as_deref() != Some(request.collection.as_str()) {
            items.push(saved_collection_list_item(&request.collection));
            visual_map.push(None);
            current_collection = Some(request.collection.clone());
            current_folder = None;
        }

        if current_folder.as_ref() != Some(&request.folder) {
            if let Some(folder) = &request.folder {
                items.push(saved_folder_list_item(folder));
                visual_map.push(None);
            }
            current_folder = Some(request.folder.clone());
        }

        items.push(saved_request_list_item(request));
        visual_map.push(Some(*idx));
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No saved requests",
            Style::default().fg(Color::DarkGray),
        ))));
        visual_map.push(None);
    }

    let visual_selected = if filtered_indices.is_empty() {
        None
    } else {
        visual_map
            .iter()
            .position(|idx| *idx == Some(app.saved_index))
    };

    let block = Block::default()
        .title(
            " Saved  (Ctrl+O: history | Enter: load | Del: remove | grouped by collection/folder) ",
        )
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let mut list_state = ListState::default();
    list_state.select(visual_selected);

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Builds a collection header row for saved requests.
fn saved_collection_list_item(collection: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("▾ {collection}"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )))
}

/// Builds a folder header row for saved requests.
fn saved_folder_list_item(folder: &str) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        format!("  ▾ {folder}"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )))
}

/// Builds a list item for a saved request.
fn saved_request_list_item(request: &SavedRequest) -> ListItem<'_> {
    let method_span = Span::styled(
        format!("{:<7}", request.method.as_str()),
        Style::default()
            .fg(method_color(request.method))
            .add_modifier(Modifier::BOLD),
    );
    let name_span = Span::styled(
        request.name.as_str(),
        Style::default().add_modifier(Modifier::BOLD),
    );
    let url_span = Span::styled(request.url.as_str(), Style::default().fg(Color::DarkGray));

    ListItem::new(Line::from(vec![
        Span::raw("    "),
        method_span,
        Span::raw(" "),
        name_span,
        Span::raw("  "),
        url_span,
    ]))
}

// ── Composer ─────────────────────────────────────────────────────────────────

/// Renders the request composer pane.
fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    // Split composer: [environment] | [method + URL row] | [params] | [headers] | [body]
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(area);

    render_environment_bar(frame, app, rows[0]);
    let method_area = render_method_url(frame, app, rows[1]);
    render_params(frame, app, rows[2]);
    render_headers(frame, app, rows[3]);
    render_body(frame, app, rows[4]);
    render_method_dropdown(frame, app, method_area);
}

/// Renders the active environment selector summary.
fn render_environment_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (name, detail, style) = if let Some(environment) = app.active_environment() {
        (
            environment.name.as_str(),
            format!("{} variables", environment.variables.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "No Environment",
            format!(
                "Ctrl+E cycles environments from {}",
                app.config.environments_file.display()
            ),
            Style::default().fg(Color::DarkGray),
        )
    };

    let line = Line::from(vec![
        Span::styled(" Env ", Style::default().fg(Color::DarkGray)),
        Span::styled(name.to_string(), style),
        Span::raw("  "),
        Span::styled(detail, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Renders the method selector and URL input row.
fn render_method_url(frame: &mut Frame, app: &App, area: Rect) -> Rect {
    // Split: [method selector] | [URL input]
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(14), Constraint::Min(0)])
        .split(area);

    let method_area = cols[0];
    let url_area = cols[1];

    // Method selector
    let method = app.current_method();
    let method_focused = app.focus == Focus::Method;
    let method_block = Block::default()
        .title(" Method ")
        .borders(Borders::ALL)
        .border_style(border_style(method_focused));

    let marker = if app.method_dropdown_open { "^" } else { "v" };
    let method_para = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("{:<10}", method.as_str()),
            Style::default()
                .fg(method_color(*method))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(marker, Style::default().fg(Color::DarkGray)),
    ]))
    .block(method_block);

    frame.render_widget(method_para, method_area);

    // URL input
    let url_focused = app.focus == Focus::Url;
    let url_block = Block::default()
        .title(" URL  (Enter / Ctrl+S: send | Ctrl+P: save | {{var}} supported) ")
        .borders(Borders::ALL)
        .border_style(border_style(url_focused));

    let active_tab = app.active_tab();
    let url_text = active_tab.url.text();
    let url_para = Paragraph::new(url_text.as_ref()).block(url_block);
    frame.render_widget(url_para, url_area);

    if url_focused {
        // x+1 / y+1 to step inside the border
        let cx = (url_area.x + 1 + cursor_offset(active_tab.url.cursor_col))
            .min(url_area.x + url_area.width.saturating_sub(2));
        let cy = url_area.y + 1;
        frame.set_cursor_position((cx, cy));
    }

    method_area
}

/// Renders the query params editor.
fn render_params(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Params;
    let block = Block::default()
        .title(" Params  (key=value per line | # disables) ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let active_tab = app.active_tab();
    let params_text = active_tab.params.text();
    let para = Paragraph::new(params_text.as_ref())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(active_tab.params.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + cursor_offset(active_tab.params.cursor_row))
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

/// Renders the expanded HTTP method dropdown.
fn render_method_dropdown(frame: &mut Frame, app: &App, method_area: Rect) {
    if !app.method_dropdown_open {
        return;
    }

    let methods = HttpMethod::all();
    let dropdown_area = Rect {
        x: method_area.x,
        y: method_area.y.saturating_add(method_area.height),
        width: method_area.width,
        height: u16::try_from(methods.len() + 2).unwrap_or(u16::MAX),
    };
    let items = methods.iter().map(|method| {
        ListItem::new(Line::from(Span::styled(
            method.as_str(),
            Style::default()
                .fg(method_color(*method))
                .add_modifier(Modifier::BOLD),
        )))
    });

    let block = Block::default()
        .title(" Select ")
        .borders(Borders::ALL)
        .border_style(border_style(app.focus == Focus::Method));
    let mut state = ListState::default();
    state.select(Some(app.active_tab().method_index));
    let list = List::new(items.collect::<Vec<_>>())
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(Clear, dropdown_area);
    frame.render_stateful_widget(list, dropdown_area, &mut state);
}

/// Renders the headers editor.
fn render_headers(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Headers;
    let block = Block::default()
        .title(" Headers  (Key: Value per line) ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let active_tab = app.active_tab();
    let headers_text = active_tab.headers.text();
    let para = Paragraph::new(headers_text.as_ref())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(active_tab.headers.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + cursor_offset(active_tab.headers.cursor_row))
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

/// Renders the request body editor.
fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Body;
    let block = Block::default()
        .title(" Body ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let active_tab = app.active_tab();
    let body_text = active_tab.body.text();
    let para = Paragraph::new(body_text.as_ref())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(active_tab.body.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + cursor_offset(active_tab.body.cursor_row))
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

// ── Response viewer ──────────────────────────────────────────────────────────

/// Renders the response viewer pane.
fn render_response(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Response;
    let active_tab = app.active_tab();

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(focused));
    if let Some(title) = response_title(app) {
        block = block.title(title).title_alignment(Alignment::Right);
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    render_response_tabs(frame, active_tab.response_tab, rows[0], rows[1]);

    let content_area = rows[2];
    if content_area.is_empty() {
        return;
    }

    if active_tab.response_tab == ResponseTab::History {
        render_response_history_chart(frame, &app.history, content_area);
        return;
    }

    let content: Text = match (&active_tab.response, &active_tab.response_tab) {
        (None, _) if app.is_sending() => Text::raw("Sending request..."),
        (None, _) | (Some(_), ResponseTab::History) => Text::raw(String::new()),
        (Some(resp), ResponseTab::Body) => render_response_body_text(resp),
        (Some(resp), ResponseTab::Cookies) => render_response_cookies_text(resp),
        (Some(resp), ResponseTab::Headers) => render_response_headers_text(resp),
        (Some(resp), ResponseTab::Scripts) => render_response_scripts_text(resp),
        (Some(resp), ResponseTab::Trace) => {
            render_response_trace_text(active_tab.last_request.as_ref(), resp)
        }
        (Some(resp), ResponseTab::Sizes) => {
            render_response_size_text(active_tab.last_request.as_ref(), resp)
        }
    };

    let para = Paragraph::new(content)
        .wrap(Wrap { trim: false })
        .scroll((active_tab.response_scroll, 0));

    frame.render_widget(para, content_area);
}

/// Returns the response pane title, if current state should display one.
fn response_title(app: &App) -> Option<Line<'static>> {
    if app.is_sending() {
        return Some(Line::raw(" Response  Sending... "));
    }

    app.active_tab().response.as_ref().map(|response| {
        Line::from(vec![
            Span::raw(" Response "),
            Span::styled(
                format!(" {} ", response.status_code),
                status_code_badge_style(response.status_code),
            ),
            Span::raw(" "),
        ])
    })
}

/// Returns the filled badge style for a response status code.
fn status_code_badge_style(code: u16) -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(status_color(code))
        .add_modifier(Modifier::BOLD)
}

/// Renders response tabs as labels with a selected underline row.
fn render_response_tabs(
    frame: &mut Frame,
    selected: ResponseTab,
    labels_area: Rect,
    line_area: Rect,
) {
    let selected_tab = ResponseTab::all()
        .iter()
        .position(|tab| *tab == selected)
        .unwrap_or_default();

    let mut label_spans = Vec::new();
    let mut underline_spans = Vec::new();
    for (idx, tab) in ResponseTab::all().iter().enumerate() {
        if idx > 0 {
            label_spans.push(Span::raw("  "));
            underline_spans.push(Span::raw("  "));
        }

        let label = tab.label();
        let label_style = if idx == selected_tab {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let underline_style = if idx == selected_tab {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        label_spans.push(Span::styled(label, label_style));
        underline_spans.push(Span::styled(
            if idx == selected_tab {
                "-".repeat(label.len())
            } else {
                " ".repeat(label.len())
            },
            underline_style,
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(label_spans)), labels_area);
    frame.render_widget(Paragraph::new(Line::from(underline_spans)), line_area);
}

/// Counts response-code buckets represented in request history.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ResponseCodeBuckets {
    /// Number of 2xx responses.
    success_2xx: usize,
    /// Number of 3xx responses.
    success_3xx: usize,
    /// Number of 4xx responses.
    failure_4xx: usize,
    /// Number of 5xx responses.
    failure_5xx: usize,
}

impl ResponseCodeBuckets {
    /// Returns total bucketed response count.
    fn total(self) -> usize {
        self.success_2xx + self.success_3xx + self.failure_4xx + self.failure_5xx
    }
}

/// Renders the response-code history chart tab.
fn render_response_history_chart(frame: &mut Frame, history: &[HistoryEntry], area: Rect) {
    let buckets = response_code_buckets(history);
    if buckets.total() == 0 {
        frame.render_widget(Paragraph::new("No response codes in history yet."), area);
        return;
    }

    let chart = PieChart::new(response_code_slices(buckets))
        .show_legend(true)
        .show_percentages(true)
        .legend_position(LegendPosition::Right)
        .legend_layout(LegendLayout::Vertical)
        .legend_alignment(LegendAlignment::Left)
        .resolution(Resolution::Braille)
        .pie_char(symbols::PIE_CHAR_BLOCK)
        .legend_marker(symbols::LEGEND_MARKER_CIRCLE);

    frame.render_widget(chart, area);
}

/// Counts supported response code buckets in history.
fn response_code_buckets(history: &[HistoryEntry]) -> ResponseCodeBuckets {
    let mut buckets = ResponseCodeBuckets::default();

    for code in history.iter().filter_map(|entry| entry.response_code) {
        match code {
            200..=299 => buckets.success_2xx += 1,
            300..=399 => buckets.success_3xx += 1,
            400..=499 => buckets.failure_4xx += 1,
            500..=599 => buckets.failure_5xx += 1,
            _ => {}
        }
    }

    buckets
}

/// Converts bounded chart dimensions and counts into `f64`.
fn usize_to_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Builds non-empty pie slices for the status-code distribution.
fn response_code_slices(buckets: ResponseCodeBuckets) -> Vec<PieSlice<'static>> {
    [
        ("2xx success", buckets.success_2xx, 0),
        ("3xx redirect", buckets.success_3xx, 1),
        ("4xx client", buckets.failure_4xx, 2),
        ("5xx server", buckets.failure_5xx, 3),
    ]
    .into_iter()
    .filter(|(_, count, _)| *count > 0)
    .map(|(label, count, bucket_idx)| {
        PieSlice::new(
            label,
            usize_to_f64(count),
            response_bucket_color(bucket_idx),
        )
    })
    .collect()
}

/// Returns the configured color for a response-code bucket index.
fn response_bucket_color(bucket_idx: usize) -> Color {
    match bucket_idx {
        0 => Color::Green,
        1 => Color::Yellow,
        2 => Color::Red,
        _ => Color::Rgb(255, 0, 0),
    }
}

/// Builds prettified response body tab text.
fn render_response_body_text(resp: &HttpResponse) -> Text<'_> {
    let mut lines: Vec<Line> = Vec::new();

    match format_response_body(&resp.body) {
        Cow::Borrowed("") => lines.push(Line::from(Span::styled(
            "Response body is empty.",
            Style::default().fg(Color::DarkGray),
        ))),
        Cow::Borrowed(body) => {
            push_body_lines(&mut lines, body);
        }
        Cow::Owned(body) => {
            push_body_lines(&mut lines, &body);
        }
    }

    Text::from(lines)
}

/// Adds body lines with stable line numbers.
fn push_body_lines(lines: &mut Vec<Line<'_>>, body: &str) {
    for (idx, line) in body.lines().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:>4}  ", idx + 1),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(line.to_string()),
        ]));
    }
}

/// Builds response cookies tab text.
fn render_response_cookies_text(resp: &HttpResponse) -> Text<'static> {
    let cookies = response_cookies(resp);
    if cookies.is_empty() {
        return Text::raw("No Set-Cookie headers in this response.");
    }

    let mut lines = Vec::new();
    for (idx, cookie) in cookies.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            cookie.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(metadata_line("Value", &cookie.value));
        for attr in &cookie.attributes {
            lines.push(metadata_line("Attribute", attr));
        }
    }

    Text::from(lines)
}

/// Builds response headers tab text.
fn render_response_headers_text(resp: &HttpResponse) -> Text<'_> {
    let mut lines = Vec::new();
    let sc = status_color(resp.status_code);

    lines.push(Line::from(Span::styled(
        format!("{} {}", resp.status_code, resp.status_text),
        Style::default().fg(sc).add_modifier(Modifier::BOLD),
    )));

    if resp.headers.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "No response headers.",
            Style::default().fg(Color::DarkGray),
        )));
        return Text::from(lines);
    }

    lines.push(Line::raw(""));
    for (key, value) in &resp.headers {
        let key_style = if is_cache_header(key) {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{key}: "), key_style),
            Span::raw(value.as_str()),
        ]));
    }

    Text::from(lines)
}

/// Builds response scripts tab text.
fn render_response_scripts_text(resp: &HttpResponse) -> Text<'static> {
    let scripts = response_scripts(&resp.body);
    if scripts.is_empty() {
        return Text::raw("No script tags found in the response body.");
    }

    let mut lines = Vec::new();
    for (idx, script) in scripts.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Script {}", idx + 1),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        match script {
            ResponseScript::External { src } => lines.push(metadata_line("Source", src)),
            ResponseScript::Inline { preview, bytes } => {
                lines.push(metadata_line("Type", "inline"));
                lines.push(metadata_line("Size", &format!("{bytes} bytes")));
                lines.push(metadata_line("Preview", preview));
            }
        }
    }

    Text::from(lines)
}

/// Builds response trace tab text.
fn render_response_trace_text(req: Option<&HttpRequest>, resp: &HttpResponse) -> Text<'static> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Request",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    if let Some(req) = req {
        lines.push(metadata_line("Method", req.method.as_str()));
        lines.push(metadata_line("URL", &req.url));
        lines.push(metadata_line("Headers", &req.headers.len().to_string()));
        lines.push(metadata_line(
            "Body bytes",
            &body_bytes(req.body.as_deref()).to_string(),
        ));
    } else {
        lines.push(Line::raw("No request captured."));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Response",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(metadata_line(
        "Status",
        &format!("{} {}", resp.status_code, resp.status_text),
    ));
    lines.push(metadata_line(
        "Duration",
        &format_duration(resp.duration_ms),
    ));
    lines.push(metadata_line("Headers", &resp.headers.len().to_string()));
    lines.push(metadata_line(
        "Body bytes",
        &body_bytes(Some(&resp.body)).to_string(),
    ));

    if let Some(cache) = header_value(&resp.headers, CACHE_CONTROL_HEADER) {
        lines.push(metadata_line("Cache-Control", cache));
    }
    if let Some(pragma) = header_value(&resp.headers, PRAGMA_HEADER) {
        lines.push(metadata_line("Pragma", pragma));
    }

    Text::from(lines)
}

/// Builds response size tab text.
fn render_response_size_text(req: Option<&HttpRequest>, resp: &HttpResponse) -> Text<'static> {
    let response_headers = header_bytes(&resp.headers);
    let response_body = body_bytes(Some(resp.body.as_str()));
    let mut lines = Vec::new();

    lines.push(Line::from(Span::styled(
        "Request",
        Style::default().add_modifier(Modifier::BOLD),
    )));

    if let Some(req) = req {
        let request_headers = header_bytes(&req.headers);
        let request_body = body_bytes(req.body.as_deref());
        lines.push(size_line("Headers", request_headers));
        lines.push(size_line("Body", request_body));
        lines.push(size_line("Total", request_headers + request_body));
    } else {
        lines.push(Line::raw("No request captured."));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Response",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(size_line("Headers", response_headers));
    lines.push(size_line("Body", response_body));
    lines.push(size_line("Total", response_headers + response_body));

    Text::from(lines)
}

/// Builds one byte-size display line.
fn size_line(label: &str, bytes: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<8}"), Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{bytes} bytes")),
    ])
}

/// Returns the byte count for header lines serialized as `Name: Value\r\n`.
fn header_bytes(headers: &[(String, String)]) -> usize {
    headers
        .iter()
        .map(|(key, value)| key.len() + ": ".len() + value.len() + "\r\n".len())
        .sum()
}

/// Returns the byte count for an optional body.
fn body_bytes(body: Option<&str>) -> usize {
    body.map_or(0, str::len)
}

/// Formats response body text for display when a structured format is detected.
fn format_response_body(body: &str) -> Cow<'_, str> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Cow::Borrowed("");
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return serde_json::to_string_pretty(&value).map_or(Cow::Borrowed(body), Cow::Owned);
    }

    if looks_like_markup(trimmed) {
        let pretty = format_markup_body(trimmed);
        if pretty != trimmed {
            return Cow::Owned(pretty);
        }
    }

    Cow::Borrowed(body)
}

/// Returns whether a body looks like HTML or XML markup.
fn looks_like_markup(body: &str) -> bool {
    body.starts_with('<') && body.contains('>')
}

/// Adds readable newlines around markup tags without trying to parse HTML.
fn format_markup_body(body: &str) -> String {
    let mut output = String::with_capacity(body.len() + body.len() / 8);
    let mut indent = 0usize;
    let mut token = String::new();
    let mut inside_tag = false;

    for ch in body.chars() {
        if ch == '<' && !inside_tag && !token.trim().is_empty() {
            push_markup_token(&mut output, token.trim(), &mut indent);
            token.clear();
        }
        token.push(ch);
        if ch == '<' {
            inside_tag = true;
        }
        if inside_tag && ch == '>' {
            push_markup_token(&mut output, token.trim(), &mut indent);
            token.clear();
            inside_tag = false;
        }
    }

    let remaining = token.trim();
    if !remaining.is_empty() {
        push_markup_token(&mut output, remaining, &mut indent);
    }

    output.trim_end().to_string()
}

/// Appends one formatted markup token.
fn push_markup_token(output: &mut String, token: &str, indent: &mut usize) {
    if token.is_empty() {
        return;
    }

    if is_closing_tag(token) {
        *indent = indent.saturating_sub(1);
    }

    if !output.is_empty() {
        output.push('\n');
    }
    output.push_str(&"  ".repeat(*indent));
    output.push_str(token);

    if is_opening_tag(token) {
        *indent += 1;
    }
}

/// Returns whether a markup token is a closing tag.
fn is_closing_tag(token: &str) -> bool {
    token.starts_with("</")
}

/// Returns whether a markup token should increase indentation.
fn is_opening_tag(token: &str) -> bool {
    token.starts_with('<')
        && !token.starts_with("</")
        && !token.starts_with("<!")
        && !token.starts_with("<?")
        && !token.ends_with("/>")
}

/// Parsed response cookie for display.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResponseCookie {
    /// Cookie name.
    name: String,
    /// Cookie value.
    value: String,
    /// Cookie attributes after the name-value pair.
    attributes: Vec<String>,
}

/// Returns response cookies parsed from all `Set-Cookie` headers.
fn response_cookies(resp: &HttpResponse) -> Vec<ResponseCookie> {
    resp.headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(SET_COOKIE_HEADER))
        .filter_map(|(_, value)| parse_set_cookie(value))
        .collect()
}

/// Parses a single `Set-Cookie` header value.
fn parse_set_cookie(value: &str) -> Option<ResponseCookie> {
    let mut parts = value.split(';').map(str::trim);
    let name_value = parts.next()?;
    let equals = name_value.find('=')?;
    let name = name_value[..equals].trim();
    if name.is_empty() {
        return None;
    }

    Some(ResponseCookie {
        name: name.to_string(),
        value: name_value[equals + 1..].trim().to_string(),
        attributes: parts
            .filter(|attr| !attr.is_empty())
            .map(ToString::to_string)
            .collect(),
    })
}

/// Script reference derived from an HTML response body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResponseScript {
    /// External script with a `src` attribute.
    External {
        /// Script source URL or path.
        src: String,
    },
    /// Inline script content preview.
    Inline {
        /// First meaningful inline script content.
        preview: String,
        /// Full inline script byte length.
        bytes: usize,
    },
}

/// Returns scripts found in a response body.
fn response_scripts(body: &str) -> Vec<ResponseScript> {
    let mut scripts = Vec::new();
    let mut remaining = body;

    while let Some(start) = find_case_insensitive(remaining, "<script") {
        remaining = &remaining[start..];
        let Some(open_end) = remaining.find('>') else {
            break;
        };
        let open_tag = &remaining[..=open_end];
        if let Some(src) = attribute_value(open_tag, "src") {
            scripts.push(ResponseScript::External { src });
        } else {
            let content_start = open_end + 1;
            let after_open = &remaining[content_start..];
            if let Some(close_start) = find_case_insensitive(after_open, "</script>") {
                let content = after_open[..close_start].trim();
                let preview = inline_script_preview(content);
                scripts.push(ResponseScript::Inline {
                    preview,
                    bytes: content.len(),
                });
                remaining = &after_open[close_start + "</script>".len()..];
                continue;
            }
        }
        remaining = &remaining[open_end + 1..];
    }

    scripts
}

/// Returns a compact inline script preview.
fn inline_script_preview(content: &str) -> String {
    let preview = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if preview.chars().count() > 100 {
        preview.chars().take(97).chain("...".chars()).collect()
    } else if preview.is_empty() {
        "(empty inline script)".to_string()
    } else {
        preview
    }
}

/// Finds `needle` in `haystack` without ASCII case sensitivity.
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Extracts a quoted or unquoted attribute value from a markup tag.
fn attribute_value(tag: &str, name: &str) -> Option<String> {
    let attr_start = find_attribute_name(tag, name)?;
    let after_name = &tag[attr_start + name.len()..];
    let after_equals = after_name.trim_start().strip_prefix('=')?.trim_start();
    let mut chars = after_equals.chars();
    let first = chars.next()?;

    if first == '"' || first == '\'' {
        let value = chars.as_str();
        let end = value.find(first)?;
        Some(value[..end].to_string())
    } else {
        let end = after_equals
            .find(|ch: char| ch.is_whitespace() || ch == '>')
            .unwrap_or(after_equals.len());
        Some(after_equals[..end].to_string())
    }
}

/// Finds an attribute name with simple markup token boundaries.
fn find_attribute_name(tag: &str, name: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(pos) = find_case_insensitive(&tag[offset..], name) {
        let absolute = offset + pos;
        let before = tag[..absolute].chars().next_back();
        let after = tag[absolute + name.len()..].chars().next();
        let valid_before = before.is_some_and(|ch| ch.is_whitespace() || ch == '<');
        let valid_after = after.is_some_and(|ch| ch.is_whitespace() || ch == '=');
        if valid_before && valid_after {
            return Some(absolute);
        }
        offset = absolute + name.len();
    }
    None
}

/// Builds a metadata key/value display line.
fn metadata_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<14}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

/// Returns the first header value matching `name`, ignoring ASCII case.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Returns whether a header is cache-related.
fn is_cache_header(name: &str) -> bool {
    name.eq_ignore_ascii_case(CACHE_CONTROL_HEADER) || name.eq_ignore_ascii_case(PRAGMA_HEADER)
}

// ── Status bar ───────────────────────────────────────────────────────────────

/// Renders the bottom status bar.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let para = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(para, area);
}

/// Renders the persistent keybinding help row.
fn render_key_help(frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let platform = current_key_help_platform();
    render_key_help_line(frame, platform.nav_label(), VIM_NAV_KEY_HELP, rows[0]);
    render_key_help_line(frame, platform.vim_label(), VIM_ACTION_KEY_HELP, rows[1]);
    render_key_help_line(frame, "ctrl", CONTROL_KEY_HELP, rows[2]);
}

/// Renders one persistent keybinding help line.
fn render_key_help_line(frame: &mut Frame, label: &'static str, help: &'static str, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            format!(" {label} "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(" "),
        Span::styled(help, Style::default().fg(Color::Gray).bg(Color::Black)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(Color::Black)),
        area,
    );
}

// ── Save dialog ──────────────────────────────────────────────────────────────

/// Renders the save target dialog when it is active.
fn render_save_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let Some(dialog) = &app.save_dialog else {
        return;
    };

    let dialog_area = centered_rect(area, 62, 11);
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Save Request ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Choose or type a collection and optional folder."),
        rows[0],
    );
    render_save_dialog_input(
        frame,
        " Collection ",
        dialog.collection.text().as_ref(),
        dialog.collection.cursor_col,
        dialog.field == SaveDialogField::Collection,
        rows[1],
    );
    render_save_dialog_input(
        frame,
        " Folder ",
        dialog.folder.text().as_ref(),
        dialog.folder.cursor_col,
        dialog.field == SaveDialogField::Folder,
        rows[2],
    );
    frame.render_widget(
        Paragraph::new("Tab: switch | Enter: save | Esc: cancel")
            .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

/// Renders one save dialog text input and places the cursor when focused.
fn render_save_dialog_input(
    frame: &mut Frame,
    title: &'static str,
    text: &str,
    cursor_col: usize,
    focused: bool,
    area: Rect,
) {
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style(focused));
    let input = Paragraph::new(text).block(block);
    frame.render_widget(input, area);

    if focused {
        let cx =
            (area.x + 1 + cursor_offset(cursor_col)).min(area.x + area.width.saturating_sub(2));
        let cy = area.y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

// ── Rename tab dialog ────────────────────────────────────────────────────────

/// Renders the active request tab rename dialog when it is open.
fn render_rename_tab_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let Some(dialog) = &app.rename_tab_dialog else {
        return;
    };

    let dialog_area = centered_rect(area, 54, 8);
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .title(" Rename Tab ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Set a custom name for the active request tab."),
        rows[0],
    );
    render_save_dialog_input(
        frame,
        " Tab name ",
        dialog.title.text().as_ref(),
        dialog.title.cursor_col,
        true,
        rows[1],
    );
    frame.render_widget(
        Paragraph::new("Enter: rename | Esc: cancel | blank: reset")
            .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

/// Renders the collection import/export dialog when it is active.
fn render_collection_io_dialog(frame: &mut Frame, app: &App, area: Rect) {
    let Some(dialog) = &app.collection_io_dialog else {
        return;
    };

    let dialog_area = centered_rect(area, 68, 10);
    frame.render_widget(Clear, dialog_area);

    let title = match dialog.mode {
        CollectionIoMode::Import => " Import Collections ",
        CollectionIoMode::Export => " Export Collections ",
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(inner);

    let help = match dialog.mode {
        CollectionIoMode::Import => "Import Postman or OpenAPI collections into saved requests.",
        CollectionIoMode::Export => "Export all saved requests as Postman or OpenAPI JSON.",
    };
    frame.render_widget(Paragraph::new(help), rows[0]);

    let format_style = if dialog.field == CollectionIoDialogField::Format {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Format ", Style::default().fg(Color::DarkGray)),
            Span::styled(dialog.format.label(), format_style),
            Span::styled(
                "  Left/Right changes format",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        rows[1],
    );

    render_save_dialog_input(
        frame,
        " File path ",
        dialog.path.text().as_ref(),
        dialog.path.cursor_col,
        dialog.field == CollectionIoDialogField::Path,
        rows[2],
    );
    frame.render_widget(
        Paragraph::new("Tab: switch | Left/Right: format | Enter: run | Esc: cancel")
            .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

/// Returns a centered rectangle with bounded dimensions.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for response body rendering helpers.

    use std::path::PathBuf;

    use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color, style::Modifier};
    use spark_core::{
        config::Config,
        history::HistoryEntry,
        http::{HttpMethod, HttpRequest, HttpResponse},
    };

    use crate::app::{App, ResponseTab};

    use super::{
        KeyHelpPlatform, ResponseCookie, ResponseScript, body_bytes, current_key_help_platform,
        format_response_body, header_bytes, render, render_response_tabs, response_code_buckets,
        response_cookies, response_scripts, response_title, status_color,
    };

    /// Valid compact JSON is expanded for display.
    #[test]
    fn response_body_formats_json() {
        let formatted = format_response_body(r#"{"name":"spark","items":[1,2]}"#);

        assert_eq!(
            formatted,
            "{\n  \"items\": [\n    1,\n    2\n  ],\n  \"name\": \"spark\"\n}"
        );
    }

    /// Non-JSON response bodies are preserved as-is.
    #[test]
    fn response_body_preserves_plain_text() {
        let body = "not json\nsecond line";

        assert_eq!(format_response_body(body), body);
    }

    /// Compact markup is split into readable lines.
    #[test]
    fn response_body_formats_markup() {
        let formatted = format_response_body("<html><body><h1>Hello</h1></body></html>");

        assert_eq!(
            formatted,
            "<html>\n  <body>\n    <h1>\n      Hello\n    </h1>\n  </body>\n</html>"
        );
    }

    /// Set-Cookie headers are parsed into displayable cookie records.
    #[test]
    fn response_cookies_parse_set_cookie_headers() {
        let resp = response_with_headers(vec![(
            "Set-Cookie".to_string(),
            "session=abc; Path=/; HttpOnly".to_string(),
        )]);

        assert_eq!(
            response_cookies(&resp),
            vec![ResponseCookie {
                name: "session".to_string(),
                value: "abc".to_string(),
                attributes: vec!["Path=/".to_string(), "HttpOnly".to_string()],
            }]
        );
    }

    /// Script tab data includes external and inline scripts.
    #[test]
    fn response_scripts_find_external_and_inline_scripts() {
        let body = r#"<script src="/app.js"></script><script>console.log("ok");</script>"#;

        assert_eq!(
            response_scripts(body),
            vec![
                ResponseScript::External {
                    src: "/app.js".to_string()
                },
                ResponseScript::Inline {
                    preview: "console.log(\"ok\");".to_string(),
                    bytes: 18,
                },
            ]
        );
    }

    /// Similar attribute names do not count as script sources.
    #[test]
    fn response_scripts_ignore_similar_attribute_names() {
        let body = r#"<script data-src="/app.js">window.app = true;</script>"#;

        assert_eq!(
            response_scripts(body),
            vec![ResponseScript::Inline {
                preview: "window.app = true;".to_string(),
                bytes: 18,
            }]
        );
    }

    /// Header byte size uses HTTP-style serialized header lines.
    #[test]
    fn header_size_counts_serialized_header_bytes() {
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Test".to_string(), "ok".to_string()),
        ];

        assert_eq!(header_bytes(&headers), 44);
    }

    /// Body byte size uses UTF-8 bytes rather than character count.
    #[test]
    fn body_size_counts_utf8_bytes() {
        assert_eq!(body_bytes(Some("é")), 2);
        assert_eq!(body_bytes(None), 0);
    }

    /// Response code history is counted into the four displayed buckets.
    #[test]
    fn response_code_buckets_count_supported_status_ranges() {
        let history = vec![
            history_entry(Some(200)),
            history_entry(Some(204)),
            history_entry(Some(301)),
            history_entry(Some(404)),
            history_entry(Some(500)),
            history_entry(Some(503)),
            history_entry(Some(102)),
            history_entry(None),
        ];

        let buckets = response_code_buckets(&history);

        assert_eq!(buckets.success_2xx, 2);
        assert_eq!(buckets.success_3xx, 1);
        assert_eq!(buckets.failure_4xx, 1);
        assert_eq!(buckets.failure_5xx, 2);
        assert_eq!(buckets.total(), 6);
    }

    /// Empty startup state has no response-pane title.
    #[test]
    fn response_title_is_absent_before_first_response() {
        let app = app_without_persisted_state();

        assert_eq!(response_title(&app), None);
    }

    /// Completed responses show only the response label and styled code.
    #[test]
    fn response_title_shows_styled_completed_status_code() {
        let mut app = app_without_persisted_state();
        app.request_tabs[app.active_request_tab].response = Some(HttpResponse {
            status_code: 411,
            status_text: "Length Required".to_string(),
            headers: Vec::new(),
            body: String::new(),
            duration_ms: 1_250,
        });

        let title = response_title(&app).expect("response title");

        assert_eq!(title.to_string(), " Response  411  ");
        assert_eq!(title.spans[1].style.fg, Some(Color::Black));
        assert_eq!(title.spans[1].style.bg, Some(Color::Red));
        assert!(title.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    /// Informational response codes get their own badge color.
    #[test]
    fn status_color_styles_informational_response_codes() {
        assert_eq!(status_color(102), Color::Cyan);
    }

    /// Response tabs render without vertical delimiters and underline selection.
    #[test]
    fn response_tabs_render_selected_bottom_line_without_delimiters() {
        let backend = TestBackend::new(64, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                render_response_tabs(
                    frame,
                    ResponseTab::Headers,
                    Rect::new(0, 0, 64, 1),
                    Rect::new(0, 1, 64, 1),
                );
            })
            .expect("draw response tabs");

        let labels = buffer_line(terminal.backend().buffer(), 0, 64);
        let underline = buffer_line(terminal.backend().buffer(), 1, 64);

        assert!(!labels.contains('|'));
        assert!(labels.contains("Body  Cookies  Headers  Scripts"));
        assert!(underline.contains("              -------"));
    }

    /// Status messages and key help render on separate footer rows.
    #[test]
    fn footer_keeps_key_help_visible_with_long_status() {
        let backend = TestBackend::new(96, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = app_without_persisted_state();
        app.status_message =
            "Loaded: GET https://api.example.com/users/with/a/very/long/path".to_string();

        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw full ui");

        let status = buffer_line(terminal.backend().buffer(), 20, 96);
        let nav_keys = buffer_line(terminal.backend().buffer(), 21, 96);
        let cmd_keys = buffer_line(terminal.backend().buffer(), 22, 96);
        let ctrl_keys = buffer_line(terminal.backend().buffer(), 23, 96);

        assert!(status.contains("Loaded: GET"));
        assert!(!status.contains("j/k move"));
        assert!(nav_keys.contains(current_key_help_platform().nav_label()));
        assert!(nav_keys.contains("j/k move"));
        assert!(nav_keys.contains("q quit"));
        assert!(cmd_keys.contains(current_key_help_platform().vim_label()));
        assert!(cmd_keys.contains("I import"));
        assert!(cmd_keys.contains("X export"));
        assert!(ctrl_keys.contains("ctrl"));
        assert!(ctrl_keys.contains("^L import"));
        assert!(ctrl_keys.contains("^X export"));
    }

    /// Platform-specific key help labels are distinct.
    #[test]
    fn key_help_platform_labels_are_os_specific() {
        assert_eq!(KeyHelpPlatform::MacOs.nav_label(), "mac nav");
        assert_eq!(KeyHelpPlatform::MacOs.vim_label(), "mac vim");
        assert_eq!(KeyHelpPlatform::Windows.nav_label(), "win nav");
        assert_eq!(KeyHelpPlatform::Windows.vim_label(), "win vim");
        assert_eq!(KeyHelpPlatform::Linux.nav_label(), "linux nav");
        assert_eq!(KeyHelpPlatform::Linux.vim_label(), "linux vim");
    }

    /// Creates an app pointed at missing state files for deterministic tests.
    fn app_without_persisted_state() -> App {
        App::new(Config {
            history_file: PathBuf::from("/tmp/spark-ui-test-missing-history.jsonl"),
            saved_requests_file: PathBuf::from("/tmp/spark-ui-test-missing-saved.json"),
            environments_file: PathBuf::from("/tmp/spark-ui-test-missing-env.json"),
        })
    }

    /// Reads one rendered buffer line as a plain string.
    fn buffer_line(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    }

    /// Creates a history entry with the provided response code.
    fn history_entry(response_code: Option<u16>) -> HistoryEntry {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com".to_string(),
            query_params: Vec::new(),
            headers: Vec::new(),
            body: None,
        };

        response_code.map_or_else(
            || HistoryEntry::from_request(&request),
            |code| HistoryEntry::from_response(&request, code),
        )
    }

    /// Creates a response with provided headers.
    fn response_with_headers(headers: Vec<(String, String)>) -> HttpResponse {
        HttpResponse {
            status_code: 200,
            status_text: "OK".to_string(),
            headers,
            body: String::new(),
            duration_ms: 10,
        }
    }
}
