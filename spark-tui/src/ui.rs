use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use spark_core::http::HttpMethod;

use crate::app::{App, Focus};

// ── Color helpers ────────────────────────────────────────────────────────────

/// Returns the display colour for an HTTP method per the design spec.
fn method_color(method: &HttpMethod) -> Color {
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

    render_history(frame, app, sidebar_area);
    render_composer(frame, app, composer_area);
    render_response(frame, app, response_area);
    render_status(frame, app, status_area);
}

// ── Sidebar ──────────────────────────────────────────────────────────────────

fn render_history(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::History;

    let items: Vec<ListItem> = app
        .history
        .iter()
        .map(|entry| {
            let color = method_color(&entry.method);
            let method_span = Span::styled(
                format!("{:<7}", entry.method.as_str()),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            );
            let url_span = Span::raw(entry.url.clone());
            ListItem::new(Line::from(vec![method_span, url_span]))
        })
        .collect();

    let block = Block::default()
        .title(" History ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let mut list_state = ListState::default();
    if !app.history.is_empty() {
        list_state.select(Some(app.history_index));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    frame.render_stateful_widget(list, area, &mut list_state);
}

// ── Composer ─────────────────────────────────────────────────────────────────

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
            .fg(method_color(method))
            .add_modifier(Modifier::BOLD),
    ))
    .block(method_block);

    frame.render_widget(method_para, method_area);

    // URL input
    let url_focused = app.focus == Focus::Url;
    let url_block = Block::default()
        .title(" URL  (←/→ method • Enter / Ctrl+S: send) ")
        .borders(Borders::ALL)
        .border_style(border_style(url_focused));

    let url_para = Paragraph::new(app.url.content()).block(url_block);
    frame.render_widget(url_para, url_area);

    if url_focused {
        // x+1 / y+1 to step inside the border
        let cx = (url_area.x + 1 + app.url.cursor_col as u16)
            .min(url_area.x + url_area.width.saturating_sub(2));
        let cy = url_area.y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

fn render_headers(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Headers;
    let block = Block::default()
        .title(" Headers  (Key: Value per line) ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let para = Paragraph::new(app.headers.content())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + app.headers.cursor_col as u16)
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + app.headers.cursor_row as u16)
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

fn render_body(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Body;
    let block = Block::default()
        .title(" Body ")
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let para = Paragraph::new(app.body.content())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);

    if focused {
        let cx = (area.x + 1 + app.body.cursor_col as u16)
            .min(area.x + area.width.saturating_sub(2));
        let cy = (area.y + 1 + app.body.cursor_row as u16)
            .min(area.y + area.height.saturating_sub(2));
        frame.set_cursor_position((cx, cy));
    }
}

// ── Response viewer ──────────────────────────────────────────────────────────

fn render_response(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Response;

    let title = match &app.response {
        Some(r) => format!(" Response  {} {} ", r.status_code, r.status_text),
        None => " Response ".to_string(),
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style(focused));

    let content: Text = match &app.response {
        None => Text::raw("No response yet. Compose a request and press Ctrl+S or Enter."),
        Some(resp) => {
            let mut lines: Vec<Line> = Vec::new();
            let sc = status_color(resp.status_code);

            // Status line
            lines.push(Line::from(Span::styled(
                format!("{} {}", resp.status_code, resp.status_text),
                Style::default().fg(sc).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));

            // Response headers
            for (k, v) in &resp.headers {
                lines.push(Line::from(vec![
                    Span::styled(format!("{k}: "), Style::default().fg(Color::DarkGray)),
                    Span::raw(v.clone()),
                ]));
            }
            lines.push(Line::raw(""));

            // Body
            for line in resp.body.lines() {
                lines.push(Line::raw(line.to_string()));
            }

            Text::from(lines)
        }
    };

    let para = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((app.response_scroll, 0));

    frame.render_widget(para, area);
}

// ── Status bar ───────────────────────────────────────────────────────────────

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let para = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::White).bg(Color::DarkGray));
    frame.render_widget(para, area);
}
