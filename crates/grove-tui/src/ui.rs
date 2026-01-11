//! UI rendering

use crate::app::{ChatApp, Role, ServerStatus};
use ratatui::{prelude::*, widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap}};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Render the chat UI
pub fn render(frame: &mut Frame, app: &ChatApp) {
    // Use locked autocomplete height for stable UI
    let autocomplete_height = app.get_autocomplete_display_height() as u16;
    let input_height = 2 + autocomplete_height; // 1 for border + 1 for input + autocomplete

    let chunks = Layout::vertical([
        Constraint::Length(2),            // Header
        Constraint::Min(1),               // Messages
        Constraint::Length(input_height), // Input + autocomplete
        Constraint::Length(2),            // Bottom padding + border
    ])
    .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_messages(frame, app, chunks[1]);
    render_input(frame, app, chunks[2]);
    render_footer(frame, chunks[3]);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().gray());
    frame.render_widget(block, area);
}

fn render_header(frame: &mut Frame, app: &ChatApp, area: Rect) {
    // Title line
    let title = Line::from(vec![
        Span::styled("grove ", Style::new().green().bold()),
        Span::styled(format!("v{}", VERSION), Style::new().green()),
    ]);

    // Status info
    let server_status = match &app.server_status {
        ServerStatus::Starting => Span::styled(" · Starting...", Style::new().yellow()),
        ServerStatus::Running { port } => {
            Span::styled(format!(" · http://localhost:{}", port), Style::new().gray())
        }
        ServerStatus::Error(e) => Span::styled(format!(" · {}", e), Style::new().red()),
    };

    let header_line = Line::from(vec![
        title.spans[0].clone(),
        title.spans[1].clone(),
        server_status,
    ]);

    // Render header with bottom border
    let block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::new().gray());

    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(header_line), inner);
}

fn render_messages(frame: &mut Frame, app: &ChatApp, area: Rect) {
    // Leave 1 column on right for scrollbar
    let content_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    };
    let visible_height = area.height as usize;

    // Build message lines
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        let (prefix, style) = match msg.role {
            Role::User => ("❯ ", Style::new().bold().white()),
            Role::Assistant => ("  ", Style::new().white()),
            Role::System => ("  ", Style::new().gray().italic()),
        };

        let timestamp = msg.timestamp.format("%H:%M").to_string();

        for (i, line) in msg.content.lines().enumerate() {
            let mut spans = vec![];

            if i == 0 {
                spans.push(Span::styled(format!("{} ", timestamp), Style::new().gray().dim()));
                spans.push(Span::styled(prefix, style));
            } else {
                spans.push(Span::raw("       ")); // Indent continuation
            }

            spans.push(Span::styled(line, style));
            lines.push(Line::from(spans));
        }

        lines.push(Line::raw("")); // Spacing
    }

    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);

    // scroll_offset is distance from bottom (0 = at bottom)
    // scroll_from_top is what Paragraph expects
    let scroll_from_top = if app.scroll_offset >= max_scroll {
        0 // At top
    } else {
        max_scroll - app.scroll_offset // Convert to top-based
    };

    let paragraph = Paragraph::new(lines)
        .scroll((scroll_from_top as u16, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, content_area);

    // Render scrollbar
    let scrollbar_area = Rect {
        x: area.x + area.width - 1,
        y: area.y,
        width: 1,
        height: area.height,
    };

    if max_scroll > 0 {
        let mut scrollbar_state = ScrollbarState::new(max_scroll)
            .position(scroll_from_top);

        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(Style::new().fg(Color::DarkGray))
            .thumb_symbol("█")
            .thumb_style(Style::new().fg(Color::DarkGray));

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_input(frame: &mut Frame, app: &ChatApp, area: Rect) {
    // Top border line only
    let border = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::new().gray());
    frame.render_widget(border, area);

    // Input area (below the border line)
    let input_y = area.y + 1;

    // Draw prompt
    let prompt = Span::styled("❯ ", Style::new().white());
    frame.render_widget(
        Paragraph::new(prompt),
        Rect { x: area.x, y: input_y, width: 2, height: 1 },
    );

    // Input text area
    let input_area = Rect {
        x: area.x + 2,
        y: input_y,
        width: area.width.saturating_sub(3),
        height: 1,
    };

    // Show placeholder when empty, otherwise show input
    if app.input.is_empty() && !app.show_autocomplete() {
        let placeholder = Span::styled(
            "Type a message or /help for commands...",
            Style::new().fg(Color::DarkGray),
        );
        frame.render_widget(Paragraph::new(placeholder), input_area);
    } else {
        frame.render_widget(&app.input, input_area);
    }

    // Render autocomplete below input if showing
    let display_height = app.get_autocomplete_display_height();
    if display_height > 0 {
        let filtered = app.filtered_commands();
        let autocomplete_area = Rect {
            x: area.x + 2,
            y: input_y + 1,
            width: area.width.saturating_sub(3),
            height: display_height as u16,
        };
        render_autocomplete(frame, app, &filtered, autocomplete_area);
    }
}

fn render_autocomplete(frame: &mut Frame, app: &ChatApp, commands: &[(&str, &str)], area: Rect) {
    // Only show max 6 items, scrolling to keep selection visible
    let max_visible = 6;
    let total = commands.len();
    let selected = app.autocomplete_index;

    let start = if total <= max_visible {
        0
    } else if selected < max_visible / 2 {
        0
    } else if selected >= total - max_visible / 2 {
        total - max_visible
    } else {
        selected - max_visible / 2
    };

    let visible_commands: Vec<_> = commands.iter().skip(start).take(max_visible).collect();

    // Render command list (no border)
    let items: Vec<Line> = visible_commands
        .iter()
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let actual_index = start + i;
            let is_selected = actual_index == selected;

            Line::from(vec![
                Span::styled(
                    format!("{:<28}", cmd),
                    if is_selected {
                        Style::new().white().bold()
                    } else {
                        Style::new().fg(Color::Rgb(180, 180, 255))
                    },
                ),
                Span::styled(
                    *desc,
                    if is_selected {
                        Style::new().white()
                    } else {
                        Style::new().gray()
                    },
                ),
            ])
        })
        .collect();

    let list = Paragraph::new(items);
    frame.render_widget(list, area);
}
