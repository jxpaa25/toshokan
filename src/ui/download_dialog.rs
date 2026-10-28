//! Popup asking how to download the chapters selected on the manga screen.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph};

use super::{display_path, key_hints, panel, truncate_start};
use crate::api::{Chapter, Manga, Quality};
use crate::app::Action;
use crate::download::OutputFormat;

const WIDTH: u16 = 60;
const HEIGHT: u16 = 12;

pub struct DownloadDialog {
    manga: Manga,
    chapters: Vec<Chapter>,
    quality: Quality,
    format: OutputFormat,
    /// The manga folder the chapters are saved in.
    dir: PathBuf,
    /// Shown above the keys, e.g. which chapters were left out.
    note: Option<String>,
}

impl DownloadDialog {
    /// `quality` and `format` are the preselected choices, usually the previous download's.
    pub fn new(
        manga: Manga,
        chapters: Vec<Chapter>,
        quality: Quality,
        format: OutputFormat,
        dir: &Path,
    ) -> Self {
        DownloadDialog {
            manga,
            chapters,
            quality,
            format,
            dir: dir.to_owned(),
            note: None,
        }
    }

    pub fn with_note(mut self, note: Option<String>) -> Self {
        self.note = note;
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Char('k')
            | KeyCode::Char('j')
            | KeyCode::Tab => {
                self.quality = match self.quality {
                    Quality::Data => Quality::DataSaver,
                    Quality::DataSaver => Quality::Data,
                };
                None
            }
            KeyCode::Char('f') => {
                self.format = self.format.toggled();
                None
            }
            KeyCode::Enter => Some(Action::StartDownload {
                manga: self.manga.clone(),
                chapters: std::mem::take(&mut self.chapters),
                quality: self.quality,
                format: self.format,
                dir: self.dir.clone(),
            }),
            KeyCode::Esc | KeyCode::Char('q') => Some(Action::CloseDialog),
            _ => None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let [area] = Layout::horizontal([Constraint::Length(WIDTH)])
            .flex(Flex::Center)
            .areas(area);
        let [area] = Layout::vertical([Constraint::Length(HEIGHT)])
            .flex(Flex::Center)
            .areas(area);

        let count = self.chapters.len();
        let plural = if count == 1 { "" } else { "s" };
        let title = format!(" Download {count} chapter{plural} ");
        let block = panel(&title, true);

        let option = |quality: Quality, name: &'static str, note: &'static str| {
            let chosen = self.quality == quality;
            let marker = if chosen { "▶ ".cyan() } else { "  ".into() };
            let name = Span::raw(format!("{name:<14}"));
            Line::from(vec![
                Span::raw("  "),
                marker,
                if chosen { name.bold() } else { name },
                note.dark_gray(),
            ])
        };
        let mut lines = vec![
            Line::default(),
            Line::from("  Quality".bold()),
            option(Quality::Data, "Original", "full resolution, 1–3 MB a page"),
            option(
                Quality::DataSaver,
                "Data saver",
                "compressed, ~300 KB a page",
            ),
            Line::default(),
            Line::from(vec!["  Format   ".bold(), Span::raw(self.format.label())]),
            Line::from(vec![
                "  Save to  ".bold(),
                // The label and borders take 13 columns.
                Span::raw(truncate_start(
                    &display_path(&self.dir),
                    WIDTH as usize - 13,
                )),
            ]),
        ];
        if let Some(note) = &self.note {
            lines.push(Line::from(format!("  {note}").yellow()));
        }
        lines.push(Line::default());
        lines.push(key_hints(&[
            ("Enter", "start"),
            ("↑↓", "quality"),
            ("f", "format"),
            ("Esc", "cancel"),
        ]));
        let text = Text::from(lines);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(text).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn dialog() -> DownloadDialog {
        let manga = Manga {
            id: "m1".to_owned(),
            title: "Berserk".to_owned(),
            description: None,
            status: None,
            year: None,
            content_rating: ContentRating::Safe,
            languages: vec![],
            authors: vec![],
            cover_file: None,
        };
        DownloadDialog::new(
            manga,
            vec![],
            Quality::Data,
            OutputFormat::Cbz,
            Path::new("/tmp/Berserk"),
        )
    }

    #[test]
    fn starts_with_the_chosen_quality_and_format() {
        let mut dialog = dialog();
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Char('f')));
        match dialog.handle_key(key(KeyCode::Enter)) {
            Some(Action::StartDownload {
                quality,
                format,
                dir,
                ..
            }) => {
                assert_eq!(quality, Quality::DataSaver);
                assert_eq!(format, OutputFormat::Folder);
                assert_eq!(dir, Path::new("/tmp/Berserk"));
            }
            other => panic!("expected a download, got {other:?}"),
        }
    }

    #[test]
    fn shows_a_note() {
        let dialog = dialog().with_note(Some("Not by A, left out: Ch. 4".to_owned()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 14)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        let text = crate::ui::buffer_text(&terminal);
        assert!(text.contains("Not by A, left out: Ch. 4"));
        assert!(text.contains("Enter start"), "the keys still fit");
    }

    #[test]
    fn escape_cancels() {
        assert!(matches!(
            dialog().handle_key(key(KeyCode::Esc)),
            Some(Action::CloseDialog)
        ));
    }
}
