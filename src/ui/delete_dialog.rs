//! Popup asking before chapters or a whole manga are deleted from the library.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::{format_size, key_hints, panel};
use crate::app::Action;
use crate::library::delete::Deletion;

const WIDTH: u16 = 60;
/// Chapter names listed before the rest are counted.
const NAMES: usize = 5;

pub struct DeleteDialog {
    deletion: Deletion,
    size: u64,
}

impl DeleteDialog {
    /// Works out how much space the deletion frees, which reads the folder.
    pub fn new(deletion: Deletion) -> Self {
        let size = deletion.size();
        DeleteDialog { deletion, size }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') => Some(Action::Delete {
                deletion: self.deletion.clone(),
                size: self.size,
            }),
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => Some(Action::CloseDialog),
            _ => None,
        }
    }

    fn title_and_lines(&self) -> (String, Vec<Line<'_>>) {
        let manga = self.deletion.manga();
        let size = format_size(self.size);
        let mut lines = vec![Line::default()];
        let title = match &self.deletion {
            Deletion::Manga(manga) if manga.dir.is_none() => {
                lines.push(Line::from(format!("  {}", manga.title).bold()));
                lines.push(Line::from(format!("  {size}")));
                lines.push(Line::default());
                lines.push(Line::from("  Deleted for good, with its reading progress."));
                " Delete chapter ".to_owned()
            }
            Deletion::Manga(manga) => {
                let count = manga.chapters.len();
                let plural = if count == 1 { "" } else { "s" };
                lines.push(Line::from(format!("  {}", manga.title).bold()));
                lines.push(Line::from(format!("  {count} chapter{plural} · {size}")));
                lines.push(Line::default());
                lines.push(Line::from(
                    "  Its chapters, cover and reading progress are deleted",
                ));
                lines.push(Line::from("  for good."));
                if manga.link.is_some() {
                    lines.push(Line::from("  It's no longer checked for new chapters."));
                }
                " Delete manga ".to_owned()
            }
            Deletion::Chapters { chapters, .. } => {
                let count = chapters.len();
                lines.push(Line::from(vec![
                    Span::raw(format!("  {}", manga.title)).bold(),
                    Span::raw(format!(" · {size}")),
                ]));
                let shown = if count > NAMES { NAMES - 1 } else { count };
                for chapter in &chapters[..shown] {
                    lines.push(Line::from(format!("    {}", chapter.name).dark_gray()));
                }
                if shown < count {
                    lines.push(Line::from(
                        format!("    … and {} more", count - shown).dark_gray(),
                    ));
                }
                lines.push(Line::default());
                let their = if count == 1 { "its" } else { "their" };
                lines.push(Line::from(format!(
                    "  Deleted for good, with {their} reading progress."
                )));
                if self.deletion.keeps_following() {
                    lines.push(Line::from(
                        "  The manga stays in the library and is still checked",
                    ));
                    lines.push(Line::from("  for new chapters."));
                } else if count == manga.chapters.len() {
                    lines.push(Line::from("  The manga leaves the library."));
                }
                let plural = if count == 1 { "" } else { "s" };
                format!(" Delete {count} chapter{plural} ")
            }
        };
        lines.push(Line::default());
        lines.push(key_hints(&[("y", "delete"), ("n", "keep")]));
        (title, lines)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let (title, lines) = self.title_and_lines();
        let height = lines.len() as u16 + 2;
        let [area] = Layout::horizontal([Constraint::Length(WIDTH)])
            .flex(Flex::Center)
            .areas(area);
        let [area] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(area);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(panel(&title, true)), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::link::MangaLink;
    use crate::library::{ChapterKind, LibraryManga, LocalChapter};
    use crate::ui::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::{Path, PathBuf};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn manga(chapters: usize) -> LibraryManga {
        LibraryManga {
            key: "Berserk".to_owned(),
            title: "Berserk".to_owned(),
            chapters: (1..=chapters)
                .map(|number| LocalChapter {
                    name: format!("Ch. {number:04} [TAMT]"),
                    file_name: format!("Ch. {number:04} [TAMT].cbz"),
                    path: PathBuf::from(format!("/nowhere/Berserk/Ch. {number:04} [TAMT].cbz")),
                    kind: ChapterKind::Cbz,
                })
                .collect(),
            dir: Some(Path::new("/nowhere/Berserk").to_owned()),
            link: Some(MangaLink {
                id: "m1".to_owned(),
                language: "en".to_owned(),
                newest_chapter: None,
            }),
        }
    }

    fn draw(dialog: &DeleteDialog) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        terminal
            .draw(|frame| dialog.render(frame, frame.area()))
            .unwrap();
        buffer_text(&terminal)
    }

    #[test]
    fn names_what_goes_and_asks_first() {
        let berserk = manga(8);
        let mut dialog = DeleteDialog {
            deletion: Deletion::Chapters {
                chapters: berserk.chapters.clone(),
                manga: berserk.clone(),
            },
            size: 136_000_000,
        };
        let text = draw(&dialog);
        assert!(text.contains("Delete 8 chapters"), "{text}");
        assert!(text.contains("Berserk · 130 MB"));
        assert!(text.contains("Ch. 0004 [TAMT]"));
        assert!(!text.contains("Ch. 0005 [TAMT]"));
        assert!(text.contains("… and 4 more"));
        assert!(text.contains("stays in the library and is still checked"));
        assert!(text.contains("y delete  n keep"));

        assert!(dialog.handle_key(key(KeyCode::Enter)).is_none());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            Some(Action::CloseDialog)
        ));
        match dialog.handle_key(key(KeyCode::Char('y'))) {
            Some(Action::Delete { deletion, size }) => {
                assert_eq!(deletion.chapters().len(), 8);
                assert_eq!(size, 136_000_000);
            }
            other => panic!("expected a deletion, got {other:?}"),
        }

        let dialog = DeleteDialog {
            deletion: Deletion::Manga(berserk),
            size: 1_300_000_000,
        };
        let text = draw(&dialog);
        assert!(text.contains("Delete manga"));
        assert!(text.contains("8 chapters · 1.2 GB"));
        assert!(text.contains("no longer checked for new chapters"));
    }
}
