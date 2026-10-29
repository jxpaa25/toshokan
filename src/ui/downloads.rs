//! Downloads screen: every chapter queued this session, or left queued or failed by the last
//! one, and how far along it is.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{ListItem, ListState, Paragraph};

use super::{display_path, key_hints, panel, render_list, selectable_list, truncate_start};
use crate::app::Action;
use crate::download::{Event, Job, SavedChapter, SavedQueue, Update};

const BAR_WIDTH: usize = 12;

enum State {
    Queued,
    Running { done: usize, total: usize },
    Finished,
    AlreadyDownloaded,
    Failed(String),
}

struct Entry {
    job: Job,
    state: State,
}

impl Entry {
    fn is_active(&self) -> bool {
        matches!(self.state, State::Queued | State::Running { .. })
    }

    fn is_complete(&self) -> bool {
        matches!(self.state, State::Finished | State::AlreadyDownloaded)
    }
}

pub struct DownloadsScreen {
    entries: Vec<Entry>,
    list: ListState,
    root: String,
    notice: Option<String>,
    paused: bool,
    /// Set after a first C, which asks for confirmation.
    cancel_all_armed: bool,
}

impl DownloadsScreen {
    pub fn new(root: &std::path::Path) -> Self {
        DownloadsScreen {
            entries: Vec::new(),
            list: ListState::default(),
            root: display_path(root),
            notice: None,
            paused: false,
            cancel_all_armed: false,
        }
    }

    /// Shows another download folder as where chapters are saved, and points failed chapters
    /// that were going into `old` at the same place in `new`, for when they're retried.
    pub fn set_root(&mut self, old: &std::path::Path, new: &std::path::Path) {
        self.root = display_path(new);
        for entry in &mut self.entries {
            if let (State::Failed(_), Ok(rest)) = (&entry.state, entry.job.dir.strip_prefix(old)) {
                entry.job.dir = new.join(rest);
            }
        }
    }

    pub fn add(&mut self, job: Job) {
        self.push(job, State::Queued);
    }

    /// Adds a chapter left by the last run: queued again, or failed with `error`.
    pub fn restore(&mut self, job: Job, error: Option<String>) {
        self.push(job, error.map_or(State::Queued, State::Failed));
    }

    fn push(&mut self, job: Job, state: State) {
        self.entries.push(Entry { job, state });
        if self.list.selected().is_none() {
            self.list.select(Some(0));
        }
    }

    /// What the next run starts with: whether paused, and the chapters queued, downloading
    /// or failed.
    pub fn saved(&self) -> SavedQueue {
        let chapters = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.state {
                State::Queued | State::Running { .. } => Some(SavedChapter::new(&entry.job, None)),
                State::Failed(err) => Some(SavedChapter::new(&entry.job, Some(err.clone()))),
                State::Finished | State::AlreadyDownloaded => None,
            })
            .collect();
        SavedQueue {
            paused: self.paused,
            chapters,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Whether a chapter is downloading, or queued or paused, into `dir`.
    pub fn is_saving_into(&self, dir: &std::path::Path) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_active() && entry.job.dir == dir)
    }

    /// Whether a chapter is downloading or waiting to, and downloads aren't paused.
    pub fn is_downloading(&self) -> bool {
        !self.paused && self.active_count() > 0
    }

    pub fn on_update(&mut self, update: Update) {
        let Some(entry) = self.entries.iter_mut().find(|e| e.job.id == update.job_id) else {
            return;
        };
        entry.state = match update.event {
            Event::Progress { done, total } => State::Running { done, total },
            Event::Finished(_) => State::Finished,
            Event::AlreadyDownloaded(_) => State::AlreadyDownloaded,
            Event::Failed(err) => State::Failed(err),
        };
    }

    /// Chapters queued or downloading.
    pub fn active_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.is_active())
            .count()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.notice = None;
        let cancel_all_confirmed = std::mem::take(&mut self.cancel_all_armed);
        let selected = self.list.selected().unwrap_or(0);
        let last = self.entries.len().saturating_sub(1);
        let cursor = match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Char('r') => return self.retry(|index| index == selected),
            KeyCode::Char('R') => return self.retry(|_| true),
            KeyCode::Char('c') | KeyCode::Delete => return self.cancel(selected),
            KeyCode::Char('C') => return self.cancel_all(cancel_all_confirmed),
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                self.notice = Some(if self.paused {
                    "Downloads paused · p resumes".to_owned()
                } else {
                    "Downloads resumed".to_owned()
                });
                return Some(Action::PauseDownloads(self.paused));
            }
            KeyCode::Char('x') => {
                self.remove(Entry::is_complete);
                return None;
            }
            KeyCode::Esc | KeyCode::Backspace => return Some(Action::CloseOverlay),
            KeyCode::Char('q') => return Some(Action::Quit),
            _ => return None,
        };
        self.list.select(Some(cursor));
        None
    }

    /// Cancels the selected chapter, deleting the pages it saved; a failed one is dropped
    /// the same way. Saved chapters stay.
    fn cancel(&mut self, selected: usize) -> Option<Action> {
        let entry = self.entries.get(selected)?;
        if entry.is_complete() {
            self.notice = Some("Already saved · x clears finished chapters".to_owned());
            return None;
        }
        let job = entry.job.clone();
        self.remove(|entry| entry.job.id == job.id);
        Some(Action::CancelDownloads(vec![job]))
    }

    /// Cancels everything queued or downloading, once confirmed with a second C.
    fn cancel_all(&mut self, confirmed: bool) -> Option<Action> {
        let count = self.active_count();
        if count == 0 {
            self.notice = Some("Nothing to cancel".to_owned());
            return None;
        }
        if !confirmed {
            self.cancel_all_armed = true;
            let plural = if count == 1 { "" } else { "s" };
            self.notice = Some(format!(
                "Cancel {count} chapter{plural} and delete their pages? Press C again"
            ));
            return None;
        }
        let jobs = self
            .entries
            .iter()
            .filter(|entry| entry.is_active())
            .map(|entry| entry.job.clone())
            .collect();
        self.remove(Entry::is_active);
        Some(Action::CancelDownloads(jobs))
    }

    /// Removes the entries matching `which`, keeping the cursor in the list.
    fn remove(&mut self, which: impl Fn(&Entry) -> bool) {
        let selected = self.list.selected().unwrap_or(0);
        self.entries.retain(|entry| !which(entry));
        let cursor = selected.min(self.entries.len().saturating_sub(1));
        self.list
            .select((!self.entries.is_empty()).then_some(cursor));
    }

    /// Re-queues failed entries matching `which`. Pages already saved are kept.
    fn retry(&mut self, which: impl Fn(usize) -> bool) -> Option<Action> {
        let mut jobs = Vec::new();
        for (index, entry) in self.entries.iter_mut().enumerate() {
            if which(index) && matches!(entry.state, State::Failed(_)) {
                entry.state = State::Queued;
                jobs.push(entry.job.clone());
            }
        }
        if jobs.is_empty() {
            self.notice = Some("Nothing to retry".to_owned());
            return None;
        }
        Some(Action::Enqueue(jobs))
    }

    /// The keys that do something right now, so the line fits.
    fn hints(&self, complete: usize, failed: usize) -> Vec<(&'static str, &'static str)> {
        let active = self.active_count();
        let selected = self
            .list
            .selected()
            .and_then(|index| self.entries.get(index));
        let mut hints = Vec::new();
        if selected.is_some_and(|entry| matches!(entry.state, State::Failed(_))) {
            hints.push(("r", "retry"));
        }
        if failed > 1 {
            hints.push(("R", "retry all"));
        }
        if selected.is_some_and(|entry| !entry.is_complete()) {
            hints.push(("c", "cancel"));
        }
        if active > 1 {
            hints.push(("C", "cancel all"));
        }
        if self.paused {
            hints.push(("p", "resume"));
        } else if active > 0 {
            hints.push(("p", "pause"));
        }
        if complete > 0 {
            hints.push(("x", "clear finished"));
        }
        hints.extend([("Esc", "back"), ("q", "quit")]);
        hints
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, spinner: &str) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);

        let complete = self.entries.iter().filter(|e| e.is_complete()).count();
        let failed = self
            .entries
            .iter()
            .filter(|e| matches!(e.state, State::Failed(_)))
            .count();
        let mut title = format!(" Downloads · {complete}/{} done ", self.entries.len());
        if failed > 0 {
            title.push_str(&format!("· {failed} failed "));
        }
        if self.paused {
            title.push_str("· paused ");
        } else if self.active_count() > 0 {
            title.push_str(&format!("{spinner} "));
        }
        let root = truncate_start(&self.root, (area.width as usize).saturating_sub(15));
        let block = panel(&title, true).title_bottom(
            Line::from(vec![" Saving to ".dark_gray(), Span::raw(root), " ".into()])
                .right_aligned(),
        );

        if self.entries.is_empty() {
            let message = Paragraph::new(vec![
                Line::from("Nothing downloaded yet.".dark_gray()),
                Line::from(
                    "Select chapters on a manga's page with Space, then press d.".dark_gray(),
                ),
            ])
            .block(block);
            frame.render_widget(message, body);
        } else {
            let items: Vec<ListItem> = self
                .entries
                .iter()
                .map(|entry| entry_item(entry, spinner, self.paused))
                .collect();
            render_list(
                frame,
                selectable_list(items).block(block),
                body,
                &mut self.list,
            );
        }

        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&self.hints(complete, failed)),
        };
        frame.render_widget(Paragraph::new(status), footer);
    }
}

fn entry_item<'a>(entry: &'a Entry, spinner: &'a str, paused: bool) -> ListItem<'a> {
    let status: Vec<Span> = match &entry.state {
        State::Queued => vec!["· queued".dark_gray()],
        State::Running { done, total } if paused => vec![
            Span::raw(format!("‖ {}", progress_bar(*done, *total))).dark_gray(),
            Span::raw(format!(" {done}/{total}")),
        ],
        State::Running { done, total } => vec![
            Span::raw(format!("{spinner} ")).cyan(),
            Span::raw(progress_bar(*done, *total)).cyan(),
            Span::raw(format!(" {done}/{total}")),
        ],
        State::Finished => vec!["✓ done".green()],
        State::AlreadyDownloaded => vec!["✓ already saved".green()],
        State::Failed(_) => vec!["✗ failed".red().bold()],
    };
    let status_width: usize = status.iter().map(Span::width).sum();
    let mut first = status;
    first.push(Span::raw(" ".repeat(24usize.saturating_sub(status_width))));
    first.push(Span::raw(entry.job.manga.title.as_str()).bold());
    first.push(Span::raw(format!(" · {}", entry.job.chapter.label())));
    if !entry.job.chapter.groups.is_empty() {
        first.push(Span::raw(format!(" [{}]", entry.job.chapter.groups.join(" & "))).dark_gray());
    }

    let mut lines = vec![Line::from(first)];
    if let State::Failed(err) = &entry.state {
        lines.push(Line::from(format!("  {err} (r to retry)").red()));
    }
    ListItem::new(lines)
}

fn progress_bar(done: usize, total: usize) -> String {
    let filled = (done * BAR_WIDTH)
        .checked_div(total)
        .unwrap_or(0)
        .min(BAR_WIDTH);
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Chapter, ContentRating, Manga, Quality};
    use crate::download::OutputFormat;
    use crate::ui::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn job(id: u64, number: &str) -> Job {
        Job {
            id,
            dir: PathBuf::from("/tmp/manga/Berserk"),
            manga: Manga {
                id: "m1".to_owned(),
                title: "Berserk".to_owned(),
                description: None,
                status: None,
                year: None,
                content_rating: ContentRating::Erotica,
                languages: vec![],
                authors: vec![],
                cover_file: None,
            },
            chapter: Chapter {
                id: format!("c{id}"),
                volume: None,
                number: Some(number.to_owned()),
                title: None,
                language: "en".to_owned(),
                pages: 10,
                groups: vec!["TAMT".to_owned()],
                publish_at: String::new(),
            },
            quality: Quality::DataSaver,
            format: OutputFormat::Cbz,
        }
    }

    fn update(job_id: u64, event: Event) -> Update {
        Update { job_id, event }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tracks_progress_and_retries_failures() {
        let mut screen = DownloadsScreen::new(std::path::Path::new("/tmp/manga"));
        screen.add(job(1, "1"));
        screen.add(job(2, "2"));
        screen.add(job(3, "3"));
        assert_eq!(screen.active_count(), 3);

        screen.on_update(update(
            1,
            Event::Finished(PathBuf::from("/tmp/manga/1.cbz")),
        ));
        screen.on_update(update(2, Event::Progress { done: 6, total: 12 }));
        screen.on_update(update(3, Event::Failed("2 of 10 pages failed".to_owned())));
        assert_eq!(screen.active_count(), 1);

        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), "⠋"))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Downloads · 1/3 done · 1 failed"));
        assert!(text.contains("✓ done"));
        assert!(text.contains("⠋ ██████░░░░░░ 6/12"));
        assert!(text.contains("Berserk · Ch. 3 [TAMT]"));
        assert!(text.contains("2 of 10 pages failed (r to retry)"));
        assert!(text.contains("Saving to /tmp/manga"));

        match screen.handle_key(key(KeyCode::Char('R'))) {
            Some(Action::Enqueue(jobs)) => assert_eq!(jobs.len(), 1),
            other => panic!("expected a retry, got {other:?}"),
        }
        assert_eq!(screen.active_count(), 2);

        screen.handle_key(key(KeyCode::Char('x')));
        assert_eq!(screen.entries.len(), 2, "finished entries are cleared");
    }

    #[test]
    fn a_new_folder_takes_the_failed_chapters_along() {
        let mut screen = DownloadsScreen::new(std::path::Path::new("/tmp/manga"));
        screen.add(job(1, "1"));
        screen.on_update(update(1, Event::Failed("timed out".to_owned())));
        screen.add(job(2, "2"));
        screen.on_update(update(
            2,
            Event::Finished(PathBuf::from("/tmp/manga/2.cbz")),
        ));
        let new = std::path::Path::new("/mnt/books");
        screen.set_root(std::path::Path::new("/tmp/manga"), new);
        assert_eq!(screen.entries[0].job.dir, new.join("Berserk"));
        assert_eq!(
            screen.entries[1].job.dir,
            PathBuf::from("/tmp/manga/Berserk")
        );
        assert!(draw(&mut screen).contains("Saving to /mnt/books"));
    }

    fn draw(screen: &mut DownloadsScreen) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), "⠋"))
            .unwrap();
        buffer_text(&terminal)
    }

    fn cancelled(action: Option<Action>) -> Vec<u64> {
        match action {
            Some(Action::CancelDownloads(jobs)) => jobs.iter().map(|job| job.id).collect(),
            other => panic!("expected a cancel, got {other:?}"),
        }
    }

    #[test]
    fn pauses_cancels_and_remembers_the_queue() {
        let mut screen = DownloadsScreen::new(std::path::Path::new("/tmp/manga"));
        screen.add(job(1, "1"));
        screen.add(job(2, "2"));
        screen.restore(job(3, "3"), Some("2 of 10 pages failed".to_owned()));
        screen.add(job(4, "4"));
        screen.on_update(update(1, Event::Progress { done: 6, total: 12 }));
        assert!(screen.is_downloading());
        let text = draw(&mut screen);
        assert!(text.contains("c cancel  C cancel all  p pause"));

        assert!(matches!(
            screen.handle_key(key(KeyCode::Char('p'))),
            Some(Action::PauseDownloads(true))
        ));
        assert!(!screen.is_downloading());
        screen.handle_key(key(KeyCode::Down));
        let text = draw(&mut screen);
        assert!(text.contains("Downloads · 0/4 done · 1 failed · paused"));
        assert!(text.contains("‖ ██████░░░░░░ 6/12"), "{text}");
        assert!(text.contains("p resume"));

        let saved = screen.saved();
        assert!(saved.paused);
        let errors: Vec<_> = saved
            .chapters
            .iter()
            .map(|chapter| chapter.error.as_deref())
            .collect();
        assert_eq!(errors, [None, None, Some("2 of 10 pages failed"), None]);

        assert_eq!(cancelled(screen.handle_key(key(KeyCode::Char('c')))), [2]);
        assert_eq!(cancelled(screen.handle_key(key(KeyCode::Delete))), [3]);
        assert_eq!(screen.entries.len(), 2);

        // Cancelling everything asks first, and any other key calls it off.
        assert!(screen.handle_key(key(KeyCode::Char('C'))).is_none());
        assert!(
            draw(&mut screen).contains("Cancel 2 chapters and delete their pages? Press C again")
        );
        screen.handle_key(key(KeyCode::Up));
        assert!(screen.handle_key(key(KeyCode::Char('C'))).is_none());
        assert_eq!(
            cancelled(screen.handle_key(key(KeyCode::Char('C')))),
            [1, 4]
        );
        assert!(screen.entries.is_empty());
        assert!(screen.saved().chapters.is_empty());

        // Chapters already saved aren't cancelled; that's for the library.
        screen.add(job(5, "5"));
        screen.on_update(update(
            5,
            Event::Finished(PathBuf::from("/tmp/manga/5.cbz")),
        ));
        assert!(screen.handle_key(key(KeyCode::Char('c'))).is_none());
        assert!(draw(&mut screen).contains("Already saved · x clears finished chapters"));
        assert!(screen.saved().chapters.is_empty());
    }
}
