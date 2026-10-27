//! Downloads screen: every queued chapter this session and how far along it is.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use super::{display_path, key_hints, panel, truncate_start};
use crate::app::Action;
use crate::download::{Event, Job, Update};

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
}

impl DownloadsScreen {
    pub fn new(root: &std::path::Path) -> Self {
        DownloadsScreen {
            entries: Vec::new(),
            list: ListState::default(),
            root: display_path(root),
            notice: None,
        }
    }

    pub fn add(&mut self, job: Job) {
        self.entries.push(Entry {
            job,
            state: State::Queued,
        });
        if self.list.selected().is_none() {
            self.list.select(Some(0));
        }
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
        let selected = self.list.selected().unwrap_or(0);
        let last = self.entries.len().saturating_sub(1);
        let cursor = match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Char('r') => return self.retry(|index| index == selected),
            KeyCode::Char('R') => return self.retry(|_| true),
            KeyCode::Char('x') => {
                self.entries.retain(|entry| !entry.is_complete());
                let cursor = selected.min(self.entries.len().saturating_sub(1));
                self.list
                    .select((!self.entries.is_empty()).then_some(cursor));
                return None;
            }
            KeyCode::Esc | KeyCode::Backspace => return Some(Action::CloseDownloads),
            KeyCode::Char('q') => return Some(Action::Quit),
            _ => return None,
        };
        self.list.select(Some(cursor));
        None
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
        if self.active_count() > 0 {
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
                .map(|entry| entry_item(entry, spinner))
                .collect();
            let list = List::new(items)
                .block(block)
                .highlight_symbol("▶ ")
                .highlight_style(
                    Style::new()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_stateful_widget(list, body, &mut self.list);
        }

        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&[
                ("r", "retry"),
                ("R", "retry all"),
                ("x", "clear finished"),
                ("Esc", "back"),
                ("q", "quit"),
            ]),
        };
        frame.render_widget(Paragraph::new(status), footer);
    }
}

fn entry_item<'a>(entry: &'a Entry, spinner: &'a str) -> ListItem<'a> {
    let status: Vec<Span> = match &entry.state {
        State::Queued => vec!["· queued".dark_gray()],
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
}
