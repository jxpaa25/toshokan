//! Screens and the drawing helpers they share.

pub mod cover;
pub mod delete_dialog;
pub mod download_dialog;
pub mod downloads;
pub mod filters;
pub mod input;
pub mod kitty;
pub mod library;
pub mod manga;
pub mod reader;
pub mod search;

use std::path::Path;

use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

use crate::api::ContentRating;

pub(crate) fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .title(title)
        .border_style(border_style(focused))
}

/// The style of a [`panel`]'s border.
pub(crate) fn border_style(focused: bool) -> Style {
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    Style::new().fg(border)
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

/// e.g. "Band of the Hawks & TAMT", or "No group" for an upload without one.
pub(crate) fn groups_label(groups: &[String]) -> String {
    if groups.is_empty() {
        "No group".to_owned()
    } else {
        groups.join(" & ")
    }
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

/// Cuts `text` from the start to fit in `width` columns, e.g. "…/Manga/Berserk".
pub(crate) fn truncate_start(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if text.width() <= width {
        return text.to_owned();
    }
    let mut kept = 0;
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if kept + char_width + 1 > width {
            break;
        }
        kept += char_width;
        start = index;
    }
    format!("…{}", &text[start..])
}

/// `path` with the home directory shortened to `~`.
pub(crate) fn display_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return Path::new("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

/// A size the way file managers show it, e.g. "820 KB", "2.4 MB", "130 MB", "1.2 GB".
pub(crate) fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    let mb = bytes / KB / KB;
    if mb < 1.0 {
        format!("{} KB", (bytes / KB).ceil())
    } else if mb < 10.0 {
        format!("{mb:.1} MB")
    } else if mb < KB {
        format!("{mb:.0} MB")
    } else {
        format!("{:.1} GB", mb / KB)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 KB");
        assert_eq!(format_size(1500), "2 KB");
        assert_eq!(format_size(2_500_000), "2.4 MB");
        assert_eq!(format_size(136_000_000), "130 MB");
        assert_eq!(format_size(1_300_000_000), "1.2 GB");
    }
}
