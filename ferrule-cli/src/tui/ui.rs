//! Pure rendering for the TUI.
//!
//! [`render`] reads the immutable [`App`] state and draws four regions:
//! a schema-tree pane on the left, the query editor at the top-right,
//! the results pane at the bottom-right, and a one-line status bar along
//! the bottom. Drawing is side-effect free apart from the `frame`
//! mutation ratatui requires; per the brief, rendering itself is not
//! unit-tested — all logic that *is* tested lives in the sibling
//! modules.

use super::app::{App, Focus};
use super::schema_tree::VisibleRow;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

/// Border style for the focused pane vs. an unfocused one.
fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title)
}

/// Draw the whole UI for the current frame.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Header (connection label) · body · status bar.
    let [header, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, app, header);

    // Body: schema tree on the left, editor/results stacked on the right.
    let [tree_area, right] =
        Layout::horizontal([Constraint::Length(30), Constraint::Fill(1)]).areas(body);

    let [input_area, results_area] =
        Layout::vertical([Constraint::Length(7), Constraint::Fill(1)]).areas(right);

    render_tree(frame, app, tree_area);
    render_input(frame, app, input_area);
    render_results(frame, app, results_area);
    render_status(frame, app, status);
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let header = Line::from(vec![
        Span::styled(
            "ferrule tui ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("· "),
        Span::styled(app.conn_label(), Style::default().fg(Color::Green)),
    ]);
    frame.render_widget(Paragraph::new(header), area);
}

fn render_tree(frame: &mut Frame, app: &App, area: Rect) {
    let tree = app.tree();
    let selected = tree.selected();
    let items: Vec<ListItem> = tree
        .visible_rows()
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let content = match row {
                VisibleRow::Schema { name, expanded } => {
                    let marker = if expanded { "▾" } else { "▸" };
                    format!("{marker} {name}")
                }
                VisibleRow::Table { name, .. } => format!("    {name}"),
            };
            let style = if i == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(content, style)))
        })
        .collect();

    let focused = app.focus() == Focus::SchemaTree;
    let list = List::new(items).block(pane_block("Schema", focused));
    frame.render_widget(list, area);
}

fn render_input(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus() == Focus::Input;
    let input = app.input();
    let paragraph =
        Paragraph::new(input.text()).block(pane_block("Query (Ctrl-Enter to run)", focused));
    frame.render_widget(paragraph, area);

    // When the editor is focused, place the hardware cursor at the
    // character cursor so typing feels native. The `+1` offsets account
    // for the pane's border; the cursor wraps across the inner width.
    if focused {
        let inner_width = area.width.saturating_sub(2).max(1);
        let cursor = u16::try_from(input.cursor()).unwrap_or(u16::MAX);
        let col = area.x + 1 + (cursor % inner_width);
        let row = area.y + 1 + (cursor / inner_width);
        frame.set_cursor_position((col, row));
    }
}

fn render_results(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus() == Focus::Results;
    let results = app.results();
    let scroll = results.scroll();
    let visible: Vec<Line> = results
        .lines()
        .iter()
        .skip(scroll)
        .map(|l| Line::from(l.as_str()))
        .collect();
    let title = if results.line_count() == 0 {
        "Results".to_string()
    } else {
        format!("Results ({} rows)", results.row_count())
    };
    let paragraph = Paragraph::new(visible).block(pane_block(&title, focused));
    frame.render_widget(paragraph, area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let status = app.status();
    let style = if status.is_error() {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let line = Line::from(Span::styled(status.text(), style));
    frame.render_widget(Paragraph::new(line), area);
}
