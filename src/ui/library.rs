//! Library screen: manga in the download folder and their chapters, with reading progress.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::{display_path, key_hints, panel, truncate_start};
use crate::app::Action;
use crate::library::state::ReadingState;
use crate::library::{self, LibraryManga};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Manga,
    Chapters,
}

pub struct LibraryScreen {
    root: PathBuf,
    manga: Vec<LibraryManga>,
    manga_list: ListState,
    chapter_list: ListState,
    focus: Focus,
    error: Option<String>,
    notice: Option<String>,
}

impl LibraryScreen {
    pub fn new(root: PathBuf) -> Self {
        LibraryScreen {
            root,
            manga: Vec::new(),
            manga_list: ListState::default(),
            chapter_list: ListState::default(),
            focus: Focus::Manga,
            error: None,
            notice: None,
        }
    }

    /// Re-reads the download folder, keeping the selected manga when it's still there.
    pub fn rescan(&mut self, state: &ReadingState) {
        let selected_key = self.selected_manga().map(|m| m.key.clone());
        match library::scan(&self.root) {
            Ok(manga) => {
                self.manga = manga;
                self.error = None;
            }
            Err(err) => {
                self.manga.clear();
                self.error = Some(err.to_string());
            }
        }
        let index = selected_key
            .and_then(|key| self.manga.iter().position(|m| m.key == key))
            .unwrap_or(0);
        if self.manga.is_empty() {
            self.manga_list.select(None);
            self.focus = Focus::Manga;
        } else {
            self.select_manga(index, state);
        }
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Selects a manga and points its chapter list at the first unread chapter.
    fn select_manga(&mut self, index: usize, state: &ReadingState) {
        self.manga_list.select(Some(index));
        let chapter = self.manga.get(index).map(|manga| {
            manga
                .chapters
                .iter()
                .position(|c| !is_read(state, manga, &c.file_name))
                .unwrap_or(0)
        });
        self.chapter_list.select(chapter);
    }

    /// Moves the selection to a chapter, e.g. the one just closed in the reader.
    pub fn show_chapter(&mut self, manga_key: &str, file_name: &str) {
        let Some(manga) = self.manga.iter().position(|m| m.key == manga_key) else {
            return;
        };
        let chapter = self.manga[manga]
            .chapters
            .iter()
            .position(|c| c.file_name == file_name);
        if let Some(chapter) = chapter {
            self.manga_list.select(Some(manga));
            self.chapter_list.select(Some(chapter));
            self.focus = Focus::Chapters;
        }
    }

    fn selected_manga(&self) -> Option<&LibraryManga> {
        self.manga.get(self.manga_list.selected()?)
    }

    pub fn handle_key(&mut self, key: KeyEvent, state: &ReadingState) -> Option<Action> {
        self.notice = None;
        match key.code {
            KeyCode::Esc => return Some(Action::CloseOverlay),
            KeyCode::Char('q') => return Some(Action::Quit),
            _ => {}
        }
        match self.focus {
            Focus::Manga => {
                let selected = self.manga_list.selected()?;
                let last = self.manga.len() - 1;
                let index = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
                    KeyCode::Home | KeyCode::Char('g') => 0,
                    KeyCode::End | KeyCode::Char('G') => last,
                    KeyCode::Enter | KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                        self.focus = Focus::Chapters;
                        return None;
                    }
                    _ => return None,
                };
                self.select_manga(index, state);
                None
            }
            Focus::Chapters => {
                let manga = self.selected_manga()?;
                let selected = self.chapter_list.selected()?;
                let last = manga.chapters.len() - 1;
                let index = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
                    KeyCode::PageUp => selected.saturating_sub(10),
                    KeyCode::PageDown => (selected + 10).min(last),
                    KeyCode::Home | KeyCode::Char('g') => 0,
                    KeyCode::End | KeyCode::Char('G') => last,
                    KeyCode::Enter => {
                        return Some(Action::OpenReader {
                            manga: manga.clone(),
                            chapter: selected,
                        });
                    }
                    KeyCode::Tab | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace => {
                        self.focus = Focus::Manga;
                        return None;
                    }
                    _ => return None,
                };
                self.chapter_list.select(Some(index));
                None
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, state: &ReadingState) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let root = truncate_start(
            &display_path(&self.root),
            (area.width as usize).saturating_sub(15),
        );
        let saving_to = Line::from(vec![
            " Reading from ".dark_gray(),
            Span::raw(root),
            " ".into(),
        ]);

        if self.manga.is_empty() {
            let message = match &self.error {
                Some(err) => vec![Line::from(format!("Couldn't read the download folder: {err}").red())],
                None => vec![
                    Line::from("Nothing here yet."),
                    Line::from(
                        format!(
                            "Downloads, plus any .cbz files or image folders you put in {}, show up here.",
                            display_path(&self.root)
                        )
                        .dark_gray(),
                    ),
                ],
            };
            let block = panel(" Library ", true).title_bottom(saving_to.right_aligned());
            let paragraph = Paragraph::new(message)
                .wrap(Wrap { trim: true })
                .block(block);
            frame.render_widget(paragraph, body);
        } else {
            let [manga_area, chapters_area] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(body);
            self.render_manga(frame, manga_area, state);
            let block = panel(" Chapters ", self.focus == Focus::Chapters)
                .title_bottom(saving_to.right_aligned());
            self.render_chapters(frame, chapters_area, block, state);
        }

        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&[
                ("Enter", "read"),
                ("Tab", "switch pane"),
                ("F3", "downloads"),
                ("Esc", "back"),
                ("q", "quit"),
            ]),
        };
        frame.render_widget(Paragraph::new(status), footer);
    }

    fn render_manga(&mut self, frame: &mut Frame, area: Rect, state: &ReadingState) {
        let items: Vec<ListItem> = self
            .manga
            .iter()
            .map(|manga| {
                let unread = manga
                    .chapters
                    .iter()
                    .filter(|c| !is_read(state, manga, &c.file_name))
                    .count();
                let count = manga.chapters.len();
                let plural = if count == 1 { "" } else { "s" };
                let mut summary = format!("{count} chapter{plural}");
                if unread > 0 && unread < count {
                    summary.push_str(&format!(" · {unread} unread"));
                } else if unread == 0 {
                    summary.push_str(" · all read");
                }
                ListItem::new(vec![
                    Line::from(manga.title.as_str().bold()),
                    Line::from(summary.dark_gray()),
                ])
            })
            .collect();
        let block = panel(" Library ", self.focus == Focus::Manga);
        frame.render_stateful_widget(list(items, block), area, &mut self.manga_list);
    }

    fn render_chapters(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        block: Block,
        state: &ReadingState,
    ) {
        let Some(manga) = self.manga_list.selected().and_then(|i| self.manga.get(i)) else {
            frame.render_widget(block, area);
            return;
        };
        let items: Vec<ListItem> = manga
            .chapters
            .iter()
            .map(|chapter| {
                let progress = state.progress(&manga.key, &chapter.file_name);
                // A fixed-width column, so long names can't push the progress out of view.
                let status = match progress {
                    Some(p) if p.read => "✓ read  ".green(),
                    Some(p) if p.page > 0 => format!("p. {:<4} ", p.page + 1).cyan(),
                    _ => "        ".into(),
                };
                ListItem::new(Line::from(vec![status, Span::raw(chapter.name.as_str())]))
            })
            .collect();
        frame.render_stateful_widget(list(items, block), area, &mut self.chapter_list);
    }
}

fn is_read(state: &ReadingState, manga: &LibraryManga, chapter: &str) -> bool {
    state.progress(&manga.key, chapter).is_some_and(|p| p.read)
}

fn list<'a>(items: Vec<ListItem<'a>>, block: Block<'a>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn lists_progress_and_opens_the_first_unread_chapter() {
        let dir = tempfile::tempdir().unwrap();
        for chapter in ["Ch. 0001.cbz", "Ch. 0002.cbz", "Ch. 0003.cbz"] {
            let path = dir.path().join("Berserk").join(chapter);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        }
        let (mut state, _) = ReadingState::load(dir.path().join("state.json"));
        state.record("Berserk", "Ch. 0001.cbz", 20, true);
        state.record("Berserk", "Ch. 0002.cbz", 6, false);

        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &state))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("3 chapters · 2 unread"));
        assert!(text.contains("✓ read  Ch. 0001"));
        assert!(text.contains("▶ p. 7    Ch. 0002"));

        screen.handle_key(key(KeyCode::Enter), &state);
        match screen.handle_key(key(KeyCode::Enter), &state) {
            Some(Action::OpenReader { chapter, manga }) => {
                assert_eq!(chapter, 1);
                assert_eq!(manga.key, "Berserk");
            }
            other => panic!("expected the reader, got {other:?}"),
        }
    }

    #[test]
    fn explains_an_empty_library() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().join("Manga"));
        screen.rescan(&state);
        assert!(screen.handle_key(key(KeyCode::Enter), &state).is_none());

        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &state))
            .unwrap();
        assert!(buffer_text(&terminal).contains("Nothing here yet."));
    }
}
