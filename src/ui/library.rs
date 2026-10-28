//! Library screen: manga in the download folder and their chapters, with reading progress
//! and the chapters released on MangaDex since. Chapters and manga are deleted from here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use super::{display_path, groups_label, key_hints, panel, truncate_start};
use crate::app::{Action, LookUp};
use crate::library::delete::Deletion;
use crate::library::state::ReadingState;
use crate::library::updates::{Found, NewChapter, Outcome};
use crate::library::{self, LibraryManga, LocalChapter};

/// Rows of new chapters listed under a manga's chapters before the rest are summed up.
const NEW_ROWS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Manga,
    Chapters,
}

/// What the last check for new chapters found for one manga.
enum Check {
    NotFound,
    Failed(String),
    Found(Found),
}

pub struct LibraryScreen {
    root: PathBuf,
    manga: Vec<LibraryManga>,
    manga_list: ListState,
    chapter_list: ListState,
    focus: Focus,
    error: Option<String>,
    notice: Option<String>,
    /// By manga key. Kept across rescans; a new check replaces them one by one.
    checks: HashMap<String, Check>,
    /// Manga checked so far and in total, while a check runs.
    checking: Option<(usize, usize)>,
    /// File names of the selected manga's chapters marked for deleting.
    marked: HashSet<String>,
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
            checks: HashMap::new(),
            checking: None,
            marked: HashSet::new(),
        }
    }

    /// Re-reads the download folder, keeping the selected manga when it's still there, or
    /// else the place in the list.
    pub fn rescan(&mut self, state: &ReadingState) {
        let selected_key = self.selected_manga().map(|m| m.key.clone());
        let previous = self.manga_list.selected().unwrap_or(0);
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
        let kept = selected_key.and_then(|key| self.manga.iter().position(|m| m.key == key));
        let index = kept.unwrap_or(previous.min(self.manga.len().saturating_sub(1)));
        match kept.and_then(|index| self.manga.get(index)) {
            Some(manga) => {
                let names: HashSet<&String> = manga.chapters.iter().map(|c| &c.file_name).collect();
                self.marked.retain(|name| names.contains(name));
            }
            None => self.marked.clear(),
        }
        if self.manga.is_empty() {
            self.manga_list.select(None);
            self.focus = Focus::Manga;
        } else {
            self.select_manga(index, state);
        }
    }

    /// Rescans after chapters were added while the screen is open, staying on the same
    /// chapter.
    pub fn refresh(&mut self, state: &ReadingState) {
        let selected = self.selected_manga().and_then(|manga| {
            let chapter = manga.chapters.get(self.chapter_list.selected()?)?;
            Some((manga.key.clone(), chapter.file_name.clone()))
        });
        let focus = self.focus;
        self.rescan(state);
        if let Some((manga, chapter)) = selected {
            self.show_chapter(&manga, &chapter);
            self.focus = focus;
        }
        if self.chapter_list.selected().is_none() {
            self.focus = Focus::Manga;
        }
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub fn check_started(&mut self, total: usize) {
        self.checking = (total > 0).then_some((0, total));
    }

    pub fn on_checked(&mut self, key: String, result: Result<Outcome, String>) {
        let check = match result {
            Ok(Outcome::NotFound) => Check::NotFound,
            Ok(Outcome::Found(found)) => Check::Found(found),
            Err(err) => Check::Failed(err),
        };
        self.checks.insert(key, check);
        if let Some((done, total)) = &mut self.checking {
            *done += 1;
            if done >= total {
                self.checking = None;
            }
        }
    }

    pub fn check_stopped(&mut self) {
        self.checking = None;
    }

    pub fn is_checking(&self) -> bool {
        self.checking.is_some()
    }

    /// How many manga the checks found new chapters for.
    pub fn with_new_chapters(&self) -> usize {
        self.checks
            .iter()
            .filter(|(key, check)| match check {
                Check::Found(found) => match self.manga.iter().find(|m| &m.key == *key) {
                    Some(manga) => !found.missing(&manga.chapters).is_empty(),
                    // Not scanned yet, so as it was when checked.
                    None => !found.chapters.is_empty(),
                },
                _ => false,
            })
            .count()
    }

    fn found(&self, manga: &LibraryManga) -> Option<&Found> {
        match self.checks.get(&manga.key)? {
            Check::Found(found) => Some(found),
            _ => None,
        }
    }

    /// Selects a manga and points its chapter list at the first unread chapter.
    fn select_manga(&mut self, index: usize, state: &ReadingState) {
        self.manga_list.select(Some(index));
        let chapter = self
            .manga
            .get(index)
            .filter(|manga| !manga.chapters.is_empty())
            .map(|manga| {
                manga
                    .chapters
                    .iter()
                    .position(|c| !is_read(state, manga, &c.file_name))
                    .unwrap_or(0)
            });
        self.chapter_list.select(chapter);
    }

    /// Forgets what the last check found for a manga that was deleted.
    pub fn forget(&mut self, manga_key: &str) {
        self.checks.remove(manga_key);
    }

    /// Moves the selection to a chapter, e.g. the one just closed in the reader.
    pub fn show_chapter(&mut self, manga_key: &str, file_name: &str) {
        let Some(manga) = self.manga.iter().position(|m| m.key == manga_key) else {
            return;
        };
        if self.manga_list.selected() != Some(manga) {
            self.marked.clear();
        }
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
        let online = match key.code {
            KeyCode::Esc => return Some(Action::CloseOverlay),
            KeyCode::Char('q') => return Some(Action::Quit),
            KeyCode::Char('u') => return Some(Action::CheckForNew),
            KeyCode::Char('d') => self.download_new(),
            KeyCode::Char('m') => self.open_online(),
            KeyCode::Char('c') => return self.continue_reading(state),
            KeyCode::Char('x') | KeyCode::Delete => return self.delete(),
            _ => None,
        };
        if let Some(online) = online {
            return match online {
                Ok(action) => {
                    self.notice = Some("Asking MangaDex…".to_owned());
                    Some(action)
                }
                Err(notice) => {
                    self.notice = Some(notice);
                    None
                }
            };
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
                // Marks are for one manga's chapters.
                if index != selected {
                    self.marked.clear();
                }
                self.select_manga(index, state);
                None
            }
            Focus::Chapters => {
                // Also from a manga with no chapters left, so before needing a selection.
                if matches!(
                    key.code,
                    KeyCode::Tab | KeyCode::Left | KeyCode::Char('h') | KeyCode::Backspace
                ) {
                    self.focus = Focus::Manga;
                    return None;
                }
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
                    KeyCode::Char(' ') => {
                        let name = manga.chapters[selected].file_name.clone();
                        if !self.marked.remove(&name) {
                            self.marked.insert(name);
                        }
                        (selected + 1).min(last)
                    }
                    KeyCode::Char('a') => {
                        if self.marked.len() == manga.chapters.len() {
                            self.marked.clear();
                        } else {
                            self.marked =
                                manga.chapters.iter().map(|c| c.file_name.clone()).collect();
                        }
                        return None;
                    }
                    _ => return None,
                };
                self.chapter_list.select(Some(index));
                None
            }
        }
    }

    /// Opens the selected manga where it was left: the furthest chapter partway through,
    /// else the first unread one.
    fn continue_reading(&mut self, state: &ReadingState) -> Option<Action> {
        let manga = self.selected_manga()?.clone();
        let progress = |chapter: &LocalChapter| state.progress(&manga.key, &chapter.file_name);
        let chapter = manga
            .chapters
            .iter()
            .rposition(|c| progress(c).is_some_and(|p| !p.read && p.page > 0))
            .or_else(|| {
                manga
                    .chapters
                    .iter()
                    .position(|c| !progress(c).is_some_and(|p| p.read))
            });
        let Some(chapter) = chapter else {
            self.notice = Some(if manga.chapters.is_empty() {
                format!("No chapters of {} on disk", manga.title)
            } else {
                format!("Every chapter of {} is read", manga.title)
            });
            return None;
        };
        self.chapter_list.select(Some(chapter));
        self.focus = Focus::Chapters;
        Some(Action::OpenReader { manga, chapter })
    }

    /// Asks to delete the selected manga, or the chapters marked in it.
    fn delete(&mut self) -> Option<Action> {
        let manga = self.selected_manga()?.clone();
        let deletion = match self.focus {
            Focus::Manga => Deletion::Manga(manga),
            Focus::Chapters => {
                let chapters: Vec<LocalChapter> = manga
                    .chapters
                    .iter()
                    .filter(|c| self.marked.contains(&c.file_name))
                    .cloned()
                    .collect();
                if chapters.is_empty() {
                    self.notice = Some("Mark chapters with Space (or a for all) first".to_owned());
                    return None;
                }
                Deletion::Chapters { manga, chapters }
            }
        };
        Some(Action::ConfirmDelete(deletion))
    }

    /// Queues the selected manga's new chapters by the group of its newest chapter. Returns
    /// what to tell the user when there's nothing to queue.
    fn download_new(&self) -> Option<Result<Action, String>> {
        let manga = self.selected_manga()?;
        let found = match self.checks.get(&manga.key) {
            Some(Check::Found(found)) => found,
            Some(Check::NotFound) => return Some(Err(not_found(manga))),
            Some(Check::Failed(_)) => {
                return Some(Err(
                    "Checking for new chapters failed; press u to try again".to_owned(),
                ));
            }
            None if self.checking.is_some() => {
                return Some(Err("Still checking MangaDex for new chapters…".to_owned()));
            }
            None => return Some(Err("Press u to check MangaDex for new chapters".to_owned())),
        };
        let missing = found.missing(&manga.chapters);
        if missing.is_empty() {
            return Some(Err("No new chapters on MangaDex".to_owned()));
        }
        let Some(groups) = &found.groups else {
            return Some(Err(
                "Can't tell which group your chapters are from; press m to choose online"
                    .to_owned(),
            ));
        };
        let group = groups_label(groups);
        let (by_group, others): (Vec<&NewChapter>, Vec<&NewChapter>) =
            missing.into_iter().partition(|c| c.upload().is_some());
        if by_group.is_empty() {
            return Some(Err(format!(
                "{group} hasn't released the new chapters; press m to pick another group"
            )));
        }
        let note = (!others.is_empty()).then(|| {
            let labels: Vec<String> = others.iter().map(|c| c.slot.label()).collect();
            format!("Not by {group}, left out: {}", labels.join(", "))
        });
        Some(Ok(Action::LookUpManga {
            id: found.link.id.clone(),
            then: LookUp::Download {
                dir: manga.dir.clone()?,
                chapters: by_group
                    .iter()
                    .filter_map(|c| c.upload().cloned())
                    .collect(),
                note,
            },
        }))
    }

    /// Opens the selected manga's page on MangaDex, in the language it was downloaded in.
    fn open_online(&self) -> Option<Result<Action, String>> {
        let manga = self.selected_manga()?;
        let link = self
            .found(manga)
            .map(|found| &found.link)
            .or(manga.link.as_ref());
        Some(match (link, &manga.dir) {
            (Some(link), Some(dir)) => Ok(Action::LookUpManga {
                id: link.id.clone(),
                then: LookUp::Open {
                    dir: dir.clone(),
                    language: link.language.clone(),
                },
            }),
            (_, None) => Err("Loose chapters aren't linked to MangaDex".to_owned()),
            (None, Some(_)) => Err(not_found(manga)),
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, state: &ReadingState, spinner: &str) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let root = display_path(&self.root);
        // `room` is the space left on a panel's bottom border.
        let reading_from = |room: usize| {
            let root = truncate_start(&root, room.saturating_sub(15));
            Line::from(vec![
                " Reading from ".dark_gray(),
                Span::raw(root),
                " ".into(),
            ])
            .right_aligned()
        };

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
            let block = panel(" Library ", true)
                .title_bottom(reading_from((body.width as usize).saturating_sub(2)));
            let paragraph = Paragraph::new(message)
                .wrap(Wrap { trim: true })
                .block(block);
            frame.render_widget(paragraph, body);
        } else {
            let [manga_area, chapters_area] =
                Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .areas(body);
            self.render_manga(frame, manga_area, state, spinner);
            let status = self.check_status();
            let used = 2 + status.as_ref().map_or(0, |status| status.width() + 1);
            let title = match self.marked.len() {
                0 => " Chapters ".to_owned(),
                marked => format!(" Chapters · {marked} marked "),
            };
            let mut block = panel(&title, self.focus == Focus::Chapters).title_bottom(
                reading_from((chapters_area.width as usize).saturating_sub(used)),
            );
            if let Some(status) = status {
                block = block.title_bottom(status);
            }
            self.render_chapters(frame, chapters_area, block, state);
        }

        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&self.hints()),
        };
        frame.render_widget(Paragraph::new(status), footer);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints = match self.focus {
            Focus::Manga => vec![("Enter", "chapters"), ("c", "continue"), ("x", "delete")],
            Focus::Chapters if self.marked.is_empty() => {
                vec![("Enter", "read"), ("c", "continue"), ("Space", "mark")]
            }
            Focus::Chapters => vec![("Enter", "read"), ("Space", "mark"), ("x", "delete marked")],
        };
        hints.extend([
            ("d", "download new"),
            ("m", "MangaDex page"),
            ("u", "check for new"),
            ("Esc", "back"),
            ("q", "quit"),
        ]);
        hints
    }

    /// What the check said about the selected manga, unless it found new chapters, which
    /// get a panel of their own.
    fn check_status(&self) -> Option<Line<'static>> {
        let manga = self.selected_manga()?;
        let status = match self.checks.get(&manga.key) {
            None if self.checking.is_some() && manga.dir.is_some() => {
                " Checking MangaDex… ".dark_gray()
            }
            None => return None,
            Some(Check::NotFound) => " Not found on MangaDex ".dark_gray(),
            Some(Check::Failed(err)) => format!(" Couldn't check MangaDex: {err} ").red(),
            Some(Check::Found(found)) if found.missing(&manga.chapters).is_empty() => {
                " Up to date with MangaDex ".dark_gray()
            }
            Some(Check::Found(_)) => return None,
        };
        Some(Line::from(status))
    }

    fn render_manga(&mut self, frame: &mut Frame, area: Rect, state: &ReadingState, spinner: &str) {
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
                if count == 0 {
                    summary = "no chapters on disk".to_owned();
                } else if unread > 0 && unread < count {
                    summary.push_str(&format!(" · {unread} unread"));
                } else if unread == 0 {
                    summary.push_str(" · all read");
                }
                let mut summary = vec![summary.dark_gray()];
                match self.checks.get(&manga.key) {
                    Some(Check::Found(found)) => {
                        let new = found.missing(&manga.chapters).len();
                        if new > 0 {
                            summary.push(" · ".dark_gray());
                            summary.push(format!("{new} new").green().bold());
                        }
                    }
                    Some(Check::Failed(_)) => summary.push(" · couldn't check".red()),
                    _ => {}
                }
                ListItem::new(vec![
                    Line::from(manga.title.as_str().bold()),
                    Line::from(summary),
                ])
            })
            .collect();
        let mut title = " Library ".to_owned();
        match self.checking {
            Some((done, total)) => {
                title.push_str(&format!("· checking MangaDex {done}/{total} {spinner} "))
            }
            None => {
                let new = self.with_new_chapters();
                if new > 0 {
                    title.push_str(&format!("· {new} with new chapters "));
                }
            }
        }
        let block = panel(&title, self.focus == Focus::Manga);
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
        let new = self
            .found(manga)
            .map(|found| (found, found.missing(&manga.chapters)))
            .filter(|(_, missing)| !missing.is_empty());
        let area = match new {
            Some((found, missing)) => {
                let rows = missing.len().min(NEW_ROWS) as u16;
                let [chapters, new] =
                    Layout::vertical([Constraint::Min(3), Constraint::Length(rows + 2)])
                        .areas(area);
                render_new(frame, new, found, &missing);
                chapters
            }
            None => area,
        };
        if manga.chapters.is_empty() {
            let message = vec![
                Line::from("No chapters on disk."),
                Line::from(
                    "It's still checked for new chapters, which show up here; x on the manga \
                     deletes it for good."
                        .dark_gray(),
                ),
            ];
            let paragraph = Paragraph::new(message)
                .wrap(Wrap { trim: true })
                .block(block);
            frame.render_widget(paragraph, area);
            return;
        }
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
                let mut spans = vec![status, Span::raw(chapter.name.as_str())];
                if !self.marked.is_empty() {
                    let mark = if self.marked.contains(&chapter.file_name) {
                        "✗ ".red().bold()
                    } else {
                        "  ".into()
                    };
                    spans.insert(0, mark);
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        frame.render_stateful_widget(list(items, block), area, &mut self.chapter_list);
    }
}

/// Lists new chapters on MangaDex: the release `d` would download, or who else released it.
fn render_new(frame: &mut Frame, area: Rect, found: &Found, missing: &[&NewChapter]) {
    let shown = if missing.len() > NEW_ROWS {
        NEW_ROWS - 1
    } else {
        missing.len()
    };
    let mut lines: Vec<Line> = missing[..shown]
        .iter()
        .map(|chapter| {
            let mut spans = vec![Span::raw(chapter.slot.label()).bold(), Span::raw("  ")];
            match chapter.upload() {
                Some(upload) => spans.push(Span::raw(groups_label(&upload.groups)).dark_gray()),
                None => {
                    let groups: Vec<String> = chapter
                        .slot
                        .uploads
                        .iter()
                        .map(|upload| groups_label(&upload.groups))
                        .collect();
                    spans.push(format!("by {}", groups.join(", ")).yellow());
                }
            }
            let title = chapter
                .upload()
                .and_then(|u| u.title.as_deref())
                .or_else(|| chapter.slot.uploads.iter().find_map(|u| u.title.as_deref()));
            if let Some(title) = title {
                spans.push(Span::raw(format!(" · {title}")));
            }
            Line::from(spans)
        })
        .collect();
    if shown < missing.len() {
        lines.push(Line::from(
            format!("… and {} more", missing.len() - shown).dark_gray(),
        ));
    }
    let title = format!(" {} new on MangaDex ", missing.len());
    let mut block = panel(&title, false).border_style(Style::new().fg(Color::Green));
    if missing.iter().any(|chapter| chapter.upload().is_none()) {
        let hint = match &found.groups {
            Some(groups) => format!(" Not by {}: press m to choose ", groups_label(groups)),
            None => " Press m to choose a group ".to_owned(),
        };
        block = block.title_bottom(Line::from(hint.yellow()));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn not_found(manga: &LibraryManga) -> String {
    format!(
        "Couldn't find \"{}\" on MangaDex; download a chapter of it from search to link it",
        manga.title
    )
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
    use crate::api::{Chapter, ChapterSlot};
    use crate::library::link::MangaLink;
    use crate::library::updates;
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
            .draw(|frame| screen.render(frame, frame.area(), &state, "⠋"))
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

    /// Berserk with chapters 1 to 4 and Akira with one, in `dir`.
    fn two_manga(dir: &std::path::Path) {
        let files = (1..=4)
            .map(|n| format!("Berserk/Ch. {n:04}.cbz"))
            .chain(["Akira/Ch. 0001.cbz".to_owned()]);
        for file in files {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        }
    }

    fn opened(action: Option<Action>) -> usize {
        match action {
            Some(Action::OpenReader { chapter, .. }) => chapter,
            other => panic!("expected the reader, got {other:?}"),
        }
    }

    #[test]
    fn continues_where_the_manga_was_left() {
        let dir = tempfile::tempdir().unwrap();
        two_manga(dir.path());
        let (mut state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);
        screen.handle_key(key(KeyCode::Down), &state);

        state.record("Berserk", "Ch. 0001.cbz", 20, true);
        assert_eq!(
            opened(screen.handle_key(key(KeyCode::Char('c')), &state)),
            1
        );
        // A chapter left partway counts first, even past unread ones.
        state.record("Berserk", "Ch. 0003.cbz", 5, false);
        assert_eq!(
            opened(screen.handle_key(key(KeyCode::Char('c')), &state)),
            2
        );
        assert_eq!(screen.focus, Focus::Chapters);
        assert_eq!(screen.chapter_list.selected(), Some(2));

        for chapter in ["Ch. 0002.cbz", "Ch. 0003.cbz", "Ch. 0004.cbz"] {
            state.record("Berserk", chapter, 20, true);
        }
        assert!(screen.handle_key(key(KeyCode::Char('c')), &state).is_none());
        assert!(render(&mut screen, &state).contains("Every chapter of Berserk is read"));
    }

    #[test]
    fn marks_chapters_and_asks_before_deleting() {
        let dir = tempfile::tempdir().unwrap();
        two_manga(dir.path());
        let (state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);
        let press = |screen: &mut LibraryScreen, code| screen.handle_key(key(code), &state);

        match press(&mut screen, KeyCode::Char('x')) {
            Some(Action::ConfirmDelete(Deletion::Manga(manga))) => assert_eq!(manga.key, "Akira"),
            other => panic!("expected to delete Akira, got {other:?}"),
        }
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Enter);
        assert!(press(&mut screen, KeyCode::Char('x')).is_none());
        assert!(
            render(&mut screen, &state).contains("Mark chapters with Space (or a for all) first")
        );

        press(&mut screen, KeyCode::Char(' '));
        press(&mut screen, KeyCode::Down);
        press(&mut screen, KeyCode::Char(' '));
        let text = render(&mut screen, &state);
        assert!(text.contains("Chapters · 2 marked"), "{text}");
        assert!(text.contains("✗         Ch. 0001"));
        assert!(text.contains("          Ch. 0002"));
        assert!(text.contains("x delete marked"));
        match press(&mut screen, KeyCode::Delete) {
            Some(Action::ConfirmDelete(Deletion::Chapters { manga, chapters })) => {
                assert_eq!(manga.key, "Berserk");
                let names: Vec<_> = chapters.iter().map(|c| c.name.as_str()).collect();
                assert_eq!(names, ["Ch. 0001", "Ch. 0003"]);
            }
            other => panic!("expected to delete two chapters, got {other:?}"),
        }

        press(&mut screen, KeyCode::Char('a'));
        assert_eq!(screen.marked.len(), 4);
        press(&mut screen, KeyCode::Char('a'));
        assert!(screen.marked.is_empty());

        // Marks belong to one manga.
        press(&mut screen, KeyCode::Char(' '));
        press(&mut screen, KeyCode::Tab);
        press(&mut screen, KeyCode::Up);
        assert!(screen.marked.is_empty());
    }

    #[test]
    fn keeps_a_followed_manga_with_no_chapters_left() {
        let dir = tempfile::tempdir().unwrap();
        let berserk = dir.path().join("Berserk");
        fs::create_dir(&berserk).unwrap();
        let link = MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: Some("Ch. 0002 [B]".to_owned()),
        };
        link.save(&berserk).unwrap();
        let (state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);
        let text = render(&mut screen, &state);
        assert!(text.contains("no chapters on disk"), "{text}");
        assert!(text.contains("It's still checked for new chapters"));

        let slots = vec![slot("3", vec![upload("3b", "3", "B")])];
        let found = updates::compare(&[], slots, link);
        screen.on_checked("Berserk".to_owned(), Ok(Outcome::Found(found)));
        let text = render(&mut screen, &state);
        assert!(text.contains("no chapters on disk · 1 new"));
        assert!(text.contains("1 new on MangaDex"));
        match screen.handle_key(key(KeyCode::Char('d')), &state) {
            Some(Action::LookUpManga { id, .. }) => assert_eq!(id, "m1"),
            other => panic!("expected the new chapter to be queued, got {other:?}"),
        }

        assert!(screen.handle_key(key(KeyCode::Char('c')), &state).is_none());
        assert!(render(&mut screen, &state).contains("No chapters of Berserk on disk"));
        screen.handle_key(key(KeyCode::Enter), &state);
        assert!(screen.handle_key(key(KeyCode::Enter), &state).is_none());
        screen.handle_key(key(KeyCode::Tab), &state);
        assert_eq!(
            screen.focus,
            Focus::Manga,
            "an empty chapter list can be left"
        );

        // Deleting the last chapters while looking at them goes back to the manga.
        screen.focus = Focus::Chapters;
        screen.refresh(&state);
        assert_eq!(screen.focus, Focus::Manga);
    }

    fn upload(id: &str, number: &str, group: &str) -> Chapter {
        Chapter {
            id: id.to_owned(),
            volume: None,
            number: Some(number.to_owned()),
            title: None,
            language: "en".to_owned(),
            pages: 20,
            groups: vec![group.to_owned()],
            publish_at: String::new(),
        }
    }

    fn slot(number: &str, uploads: Vec<Chapter>) -> ChapterSlot {
        ChapterSlot {
            volume: None,
            number: Some(number.to_owned()),
            uploads,
        }
    }

    fn render(screen: &mut LibraryScreen, state: &ReadingState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 16)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), state, "⠋"))
            .unwrap();
        buffer_text(&terminal)
    }

    #[test]
    fn marks_new_chapters_and_queues_the_ones_by_the_same_group() {
        let dir = tempfile::tempdir().unwrap();
        let manga_dir = dir.path().join("Berserk");
        for chapter in ["Ch. 0001 [A].cbz", "Ch. 0002 [A].cbz"] {
            fs::create_dir_all(&manga_dir).unwrap();
            fs::write(manga_dir.join(chapter), b"").unwrap();
        }
        let (state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);
        screen.check_started(1);
        assert!(render(&mut screen, &state).contains("Library · checking MangaDex 0/1 ⠋"));

        let slots = vec![
            slot("2", vec![upload("2a", "2", "A")]),
            slot("3", vec![upload("3a", "3", "A"), upload("3b", "3", "B")]),
            slot("4", vec![upload("4b", "4", "B")]),
        ];
        let link = MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: None,
        };
        let found = updates::compare(&screen.manga[0].chapters, slots, link);
        screen.on_checked("Berserk".to_owned(), Ok(Outcome::Found(found)));
        assert!(!screen.is_checking());
        assert_eq!(screen.with_new_chapters(), 1);

        let text = render(&mut screen, &state);
        assert!(text.contains("Library · 1 with new chapters"), "{text}");
        assert!(text.contains("2 chapters · 2 new"));
        assert!(text.contains("2 new on MangaDex"));
        assert!(text.contains("Ch. 3  A"));
        assert!(text.contains("Ch. 4  by B"));
        assert!(text.contains("Not by A: press m to choose"));

        match screen.handle_key(key(KeyCode::Char('d')), &state) {
            Some(Action::LookUpManga {
                id,
                then:
                    LookUp::Download {
                        dir,
                        chapters,
                        note,
                    },
            }) => {
                assert_eq!(id, "m1");
                assert_eq!(dir, manga_dir);
                let ids: Vec<_> = chapters.iter().map(|c| c.id.as_str()).collect();
                assert_eq!(ids, ["3a"]);
                assert_eq!(note.as_deref(), Some("Not by A, left out: Ch. 4"));
            }
            other => panic!("expected new chapters to download, got {other:?}"),
        }
        assert!(matches!(
            screen.handle_key(key(KeyCode::Char('m')), &state),
            Some(Action::LookUpManga { then: LookUp::Open { ref language, ref dir }, .. })
                if language == "en" && dir == &manga_dir
        ));

        // Chapter 3 finished downloading; only 4 is left, and A didn't release it.
        fs::write(manga_dir.join("Ch. 0003 [A].cbz"), b"").unwrap();
        screen.refresh(&state);
        assert!(render(&mut screen, &state).contains("3 chapters · 1 new"));
        assert!(screen.handle_key(key(KeyCode::Char('d')), &state).is_none());
        assert_eq!(
            screen.notice.as_deref(),
            Some("A hasn't released the new chapters; press m to pick another group")
        );
    }

    #[test]
    fn explains_why_nothing_can_be_queued() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Mine")).unwrap();
        fs::write(dir.path().join("Mine/Ch. 0001.cbz"), b"").unwrap();
        let (state, _) = ReadingState::load(dir.path().join("state.json"));
        let mut screen = LibraryScreen::new(dir.path().to_owned());
        screen.rescan(&state);

        assert!(screen.handle_key(key(KeyCode::Char('d')), &state).is_none());
        assert_eq!(
            screen.notice.as_deref(),
            Some("Press u to check MangaDex for new chapters")
        );
        screen.on_checked("Mine".to_owned(), Ok(Outcome::NotFound));
        assert!(render(&mut screen, &state).contains("Not found on MangaDex"));
        assert!(screen.handle_key(key(KeyCode::Char('m')), &state).is_none());
        assert!(screen.notice.unwrap().starts_with("Couldn't find \"Mine\""));
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
            .draw(|frame| screen.render(frame, frame.area(), &state, "⠋"))
            .unwrap();
        assert!(buffer_text(&terminal).contains("Nothing here yet."));
    }
}
