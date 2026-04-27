//! Rendering functions for the Spark terminal interface.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use spark_core::history::{HistoryEntry, relative_time_label};
use spark_core::http::{HttpMethod, HttpRequest, HttpResponse};
use spark_core::saved::SavedRequest;
use std::borrow::Cow;
use tui_piechart::{
    LegendAlignment, LegendLayout, LegendPosition, PieChart, PieSlice, Resolution, symbols,
};

use crate::app::{App, Focus, ResponseTab, SidebarMode};

/// Millisecond threshold at which durations switch to seconds.
const MS_IN_SECONDS: u128 = 1_000;

// ── Color helpers ────────────────────────────────────────────────────────────

/// Returns the display colour for an HTTP method per the design spec.
fn method_color(method: HttpMethod) -> Color {
    match method {
        HttpMethod::Get | HttpMethod::Head => Color::Green,
        HttpMethod::Post => Color::Yellow,
        HttpMethod::Put => Color::Blue,
        HttpMethod::Patch => Color::Magenta,
        HttpMethod::Delete => Color::Rgb(255, 140, 0),
        HttpMethod::Options => Color::Rgb(255, 105, 180),
    }
}

/// Returns a colour appropriate for an HTTP status code.
fn status_color(code: u16) -> Color {
    match code {
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

    // Root split: content area + 1-line status bar
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let main_area = root[0];
    let status_area = root[1];

    // Horizontal split: sidebar (25%) | central pane (75%)
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(main_area);

    let sidebar_area = columns[0];
    let central_area = columns[1];

    // Central split: composer (50%) | response (50%)
    let central_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(central_area);

    let composer_area = central_rows[0];
    let response_area = central_rows[1];

    render_sidebar(frame, app, sidebar_area);
    render_composer(frame, app, composer_area);
    render_response(frame, app, response_area);
    render_status(frame, app, status_area);
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

    for idx in &filtered_indices {
        let request = &app.saved_requests[*idx];
        items.push(saved_request_list_item(request));
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  No saved requests",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let visual_selected = filtered_indices
        .iter()
        .position(|idx| *idx == app.saved_index);

    let block = Block::default()
        .title(" Saved  (Ctrl+O: history | Enter: load | Del: remove) ")
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
    // Split composer: [method + URL row] | [headers] | [body]
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(40),
            Constraint::Min(3),
        ])
        .split(area);

    render_method_url(frame, app, rows[0]);
    render_headers(frame, app, rows[1]);
    render_body(frame, app, rows[2]);
}

/// Renders the method selector and URL input row.
fn render_method_url(frame: &mut Frame, app: &App, area: Rect) {
    // Split: [method selector (12 cols)] | [URL input]
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(12), Constraint::Min(0)])
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

    let method_para = Paragraph::new(Span::styled(
        method.as_str(),
        Style::default()
            .fg(method_color(*method))
            .add_modifier(Modifier::BOLD),
    ))
    .block(method_block);

    frame.render_widget(method_para, method_area);

    // URL input
    let url_focused = app.focus == Focus::Url;
    let url_block = Block::default()
        .title(" URL  (Enter / Ctrl+S: send | Ctrl+P: save) ")
        .borders(Borders::ALL)
        .border_style(border_style(url_focused));

    let url_text = app.url.text();
    let url_para = Paragraph::new(url_text.as_ref()).block(url_block);
    frame.render_widget(url_para, url_area);

    if url_focused {
        // x+1 / y+1 to step inside the border
        let cx = (url_area.x + 1 + cursor_offset(app.url.cursor_col))
            .min(url_area.x + url_area.width.saturating_sub(2));
        let cy = url_area.y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

/// Renders the headers editor.
fn render_headers(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Headers;
    let block = Block::default()
        .title(" Headers  (Key: Value per line) ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let headers_text = app.headers.text();
    let para = Paragraph::new(headers_text.as_ref())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(app.headers.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + cursor_offset(app.headers.cursor_row))
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

    let body_text = app.body.text();
    let para = Paragraph::new(body_text.as_ref())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + cursor_offset(app.body.cursor_col))
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + cursor_offset(app.body.cursor_row))
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

// ── Response viewer ──────────────────────────────────────────────────────────

/// Renders the response viewer pane.
fn render_response(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Response;

    let title = match &app.response {
        Some(r) => format!(
            " Response  {} {}  {} ",
            r.status_code,
            r.status_text,
            format_duration(r.duration_ms),
        ),
        None => " Response ".to_string(),
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let selected_tab = match app.response_tab {
        ResponseTab::Body => 0,
        ResponseTab::Sizes => 1,
        ResponseTab::History => 2,
    };

    let tabs = Tabs::new(["Body", "Sizes", "History"])
        .select(selected_tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, rows[0]);

    if app.response_tab == ResponseTab::History {
        render_response_history_chart(frame, &app.history, rows[1]);
        return;
    }

    let content: Text = match (&app.response, &app.response_tab) {
        (None, _) => Text::raw("No response yet. Compose a request and press Ctrl+S or Enter."),
        (Some(resp), ResponseTab::Body) => render_response_body_text(resp),
        (Some(resp), ResponseTab::Sizes) => {
            render_response_size_text(app.last_request.as_ref(), resp)
        }
        (Some(_), ResponseTab::History) => Text::raw(String::new()),
    };

    let para = Paragraph::new(content)
        .wrap(Wrap { trim: false })
        .scroll((app.response_scroll, 0));

    frame.render_widget(para, rows[1]);
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

/// Builds response body tab text.
fn render_response_body_text(resp: &HttpResponse) -> Text<'_> {
    let mut lines: Vec<Line> = Vec::new();
    let sc = status_color(resp.status_code);

    lines.push(Line::from(Span::styled(
        format!("{} {}", resp.status_code, resp.status_text),
        Style::default().fg(sc).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    for (k, v) in &resp.headers {
        lines.push(Line::from(vec![
            Span::styled(format!("{k}: "), Style::default().fg(Color::DarkGray)),
            Span::raw(v.as_str()),
        ]));
    }
    lines.push(Line::raw(""));

    match format_response_body(&resp.body) {
        Cow::Borrowed(body) => {
            for line in body.lines() {
                lines.push(Line::raw(line));
            }
        }
        Cow::Owned(body) => {
            for line in body.lines() {
                lines.push(Line::raw(line.to_string()));
            }
        }
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

    serde_json::from_str::<serde_json::Value>(trimmed)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .map_or_else(|_| Cow::Borrowed(body), Cow::Owned)
}

// ── Status bar ───────────────────────────────────────────────────────────────

/// Renders the bottom status bar.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let para = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    //! Tests for response body rendering helpers.

    use spark_core::{
        history::HistoryEntry,
        http::{HttpMethod, HttpRequest},
    };

    use super::{body_bytes, format_response_body, header_bytes, response_code_buckets};

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

    /// Creates a history entry with the provided response code.
    fn history_entry(response_code: Option<u16>) -> HistoryEntry {
        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com".to_string(),
            headers: Vec::new(),
            body: None,
        };

        response_code.map_or_else(
            || HistoryEntry::from_request(&request),
            |code| HistoryEntry::from_response(&request, code),
        )
    }
}
