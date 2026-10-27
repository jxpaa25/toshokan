//! Screens and the drawing helpers they share.

pub mod input;
pub mod manga;
pub mod search;

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use crate::api::ContentRating;

pub(crate) fn panel(title: &str, focused: bool) -> Block<'_> {
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Block::bordered()
        .title(title)
        .border_style(Style::new().fg(border))
}

pub(crate) fn rating_span(rating: ContentRating) -> Span<'static> {
    let span = Span::raw(rating.as_str());
    match rating {
        ContentRating::Safe => span.green(),
        ContentRating::Suggestive => span.yellow(),
        ContentRating::Erotica | ContentRating::Pornographic => span.red(),
    }
}

pub(crate) fn join_spans(spans: Vec<Span<'_>>) -> Vec<Span<'_>> {
    let mut joined = Vec::with_capacity(spans.len() * 2);
    for (i, span) in spans.into_iter().enumerate() {
        if i > 0 {
            joined.push(Span::raw(" · "));
        }
        joined.push(span);
    }
    joined
}

pub(crate) fn key_hints<'a>(keys: &[(&'a str, &'a str)]) -> Line<'a> {
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(Span::raw(*key).cyan().bold());
        spans.push(Span::raw(format!(" {label}  ")).dark_gray());
    }
    Line::from(spans)
}

/// Descriptions often end with a `---` rule followed by markdown links; those just add noise here.
pub(crate) fn strip_link_section(description: &str) -> String {
    description
        .split("\n---")
        .next()
        .unwrap_or(description)
        .trim_end()
        .to_owned()
}

#[cfg(test)]
pub(crate) fn buffer_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
