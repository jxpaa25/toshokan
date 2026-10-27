//! Reader: shows a downloaded chapter's pages as images in the terminal.
//!
//! Terminals with the kitty graphics protocol (Ghostty, kitty) get pages sent ahead of time
//! and scaled by the terminal ([`kitty_view`]); everything else goes through `ratatui-image`
//! ([`fallback`]). Either way the slow work happens on worker threads: the screen hands out
//! jobs with [`ReaderScreen::take_requests`], the app runs them with [`ReaderJob::run`], and
//! results come back through [`ReaderScreen::on_job_done`].

mod fallback;
#[cfg(test)]
mod ghostty_sim;
mod kitty_view;
pub mod layout;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui_image::picker::Picker;

use self::fallback::{FallbackView, RenderJob, Rendered, centered};
use self::kitty_view::{KittyView, PrepareJob, Prepared};
use self::layout::page_views;
use super::kitty::Medium;
use super::{key_hints, panel};
use crate::app::Action;
use crate::library::state::{Direction, ReadingState};
use crate::library::{LibraryManga, LocalChapter, pages};

/// Rows moved per scroll key press in fit-width mode.
const SCROLL_STEP: u16 = 3;

static NEXT_READER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PageLayout {
    #[default]
    Single,
    /// The first page alone (the cover), then pairs.
    Spread,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Fit {
    /// The whole page (or spread) is visible.
    #[default]
    Page,
    /// Fill the width and scroll vertically.
    Width,
}

/// What the terminal can show, found out once at startup.
#[derive(Clone)]
pub struct Graphics {
    pub picker: Picker,
    /// Set when pages can be drawn with the kitty protocol directly.
    pub kitty: Option<Medium>,
}

/// Slow work for a worker thread.
pub enum ReaderJob {
    Prepare(PrepareJob),
    Render(RenderJob),
}

impl ReaderJob {
    pub fn run(self) -> ReaderResult {
        match self {
            ReaderJob::Prepare(job) => ReaderResult::Prepared(job.run()),
            ReaderJob::Render(job) => ReaderResult::Rendered(job.run()),
        }
    }
}

pub enum ReaderResult {
    Prepared(Prepared),
    Rendered(Rendered),
}

impl ReaderResult {
    fn reader_id(&self) -> u64 {
        match self {
            ReaderResult::Prepared(prepared) => prepared.reader_id,
            ReaderResult::Rendered(rendered) => rendered.reader_id,
        }
    }
}

impl fmt::Debug for ReaderResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            ReaderResult::Prepared(_) => "Prepared",
            ReaderResult::Rendered(_) => "Rendered",
        };
        write!(f, "ReaderResult::{kind}(reader {})", self.reader_id())
    }
}

/// What the display code needs to know about the reader.
pub(crate) struct ViewState<'a> {
    pub reader_id: u64,
    pub views: &'a [Vec<usize>],
    pub view: usize,
    pub fit: Fit,
    pub direction: Direction,
    pub scroll: u16,
    pub pages: &'a [String],
    pub chapter: &'a LocalChapter,
}

enum Display {
    Kitty(KittyView),
    Fallback(FallbackView),
}

/// Where the reader is in the chapter, and how it's showing it.
struct Reading {
    id: u64,
    manga: LibraryManga,
    chapter: usize,
    pages: Vec<String>,
    views: Vec<Vec<usize>>,
    /// Set when the chapter can't be opened at all.
    error: Option<String>,
    layout: PageLayout,
    fit: Fit,
    direction: Direction,
    view: usize,
    scroll: u16,
    max_scroll: u16,
}

impl Reading {
    fn state(&self) -> ViewState<'_> {
        ViewState {
            reader_id: self.id,
            views: &self.views,
            view: self.view,
            fit: self.fit,
            direction: self.direction,
            scroll: self.scroll,
            pages: &self.pages,
            chapter: &self.manga.chapters[self.chapter],
        }
    }

    fn can_show(&self) -> bool {
        self.error.is_none() && self.view < self.views.len()
    }

    fn local_chapter(&self) -> &LocalChapter {
        &self.manga.chapters[self.chapter]
    }

    fn first_page(&self) -> usize {
        self.views
            .get(self.view)
            .and_then(|view| view.first().copied())
            .unwrap_or(0)
    }

    /// Saves the current page; the chapter counts as read once its last page is on screen.
    fn progress_action(&self) -> Option<Action> {
        let view = self.views.get(self.view)?;
        Some(Action::SaveProgress {
            manga: self.manga.key.clone(),
            chapter: self.local_chapter().file_name.clone(),
            page: view[0],
            finished: view.contains(&(self.pages.len() - 1)),
        })
    }
}

pub struct ReaderScreen {
    reading: Reading,
    display: Display,
    end_prompt: bool,
    notice: Option<String>,
}

impl ReaderScreen {
    /// Opens `manga.chapters[chapter]` where the user left off, and returns the progress to
    /// record for opening it.
    pub fn new(
        manga: LibraryManga,
        chapter: usize,
        state: &ReadingState,
        graphics: &Graphics,
        layout: PageLayout,
        fit: Fit,
    ) -> (Self, Option<Action>) {
        let local = &manga.chapters[chapter];
        let (pages, error) = match pages::list(local) {
            Ok(pages) => (pages, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        let views = page_views(pages.len(), layout);
        let last_view = views.len().saturating_sub(1);
        let view_of = |page: usize| views.iter().position(|v| v.contains(&page));
        // A finished chapter left on its last page starts over instead.
        let view = match state.progress(&manga.key, &local.file_name) {
            Some(progress) => match view_of(progress.page) {
                Some(view) if progress.read && view == last_view => 0,
                Some(view) => view,
                None => 0,
            },
            None => 0,
        };
        let font = graphics.picker.font_size();
        let display = match &graphics.kitty {
            Some(medium) => {
                Display::Kitty(KittyView::new(medium.clone(), (font.width, font.height)))
            }
            None => Display::Fallback(FallbackView::new(graphics.picker.clone())),
        };
        let screen = ReaderScreen {
            reading: Reading {
                id: NEXT_READER_ID.fetch_add(1, Ordering::Relaxed),
                direction: state.direction(&manga.key),
                manga,
                chapter,
                pages,
                views,
                error,
                layout,
                fit,
                view,
                scroll: 0,
                max_scroll: 0,
            },
            display,
            end_prompt: false,
            notice: None,
        };
        let action = screen.reading.progress_action();
        (screen, action)
    }

    /// The manga key and chapter file name being read.
    pub fn position(&self) -> (&str, &str) {
        (
            &self.reading.manga.key,
            &self.reading.local_chapter().file_name,
        )
    }

    /// Layout and fit, so the next chapter opens the same way.
    pub fn preferences(&self) -> (PageLayout, Fit) {
        (self.reading.layout, self.reading.fit)
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Updates what the terminal holds for the next frame of `area` (the whole screen).
    /// Call before drawing, then write [`Self::take_output`] to the terminal.
    pub fn prepare_frame(&mut self, area: Rect) {
        let Display::Kitty(view) = &mut self.display else {
            return;
        };
        if !self.reading.can_show() {
            return;
        }
        let (image_area, _) = split(area);
        let max_scroll = view.prepare_frame(&self.reading.state(), image_area);
        self.reading.max_scroll = max_scroll;
        self.reading.scroll = self.reading.scroll.min(max_scroll);
    }

    /// Escape sequences to write to the terminal before the next frame.
    pub fn take_output(&mut self) -> Vec<u8> {
        match &mut self.display {
            Display::Kitty(view) => view.take_output(),
            Display::Fallback(_) => Vec::new(),
        }
    }

    /// Work to run on worker threads.
    pub fn take_requests(&mut self) -> Vec<ReaderJob> {
        match &mut self.display {
            Display::Kitty(view) if self.reading.can_show() => view
                .take_requests(&self.reading.state())
                .into_iter()
                .map(ReaderJob::Prepare)
                .collect(),
            Display::Kitty(_) => Vec::new(),
            Display::Fallback(view) => view
                .take_requests()
                .into_iter()
                .map(ReaderJob::Render)
                .collect(),
        }
    }

    pub fn on_job_done(&mut self, result: ReaderResult) {
        // A result for a reader that's gone is dropped, which also removes its page file.
        if result.reader_id() != self.reading.id {
            return;
        }
        let state = self.reading.state();
        match (&mut self.display, result) {
            (Display::Kitty(view), ReaderResult::Prepared(prepared)) => {
                view.on_prepared(&state, prepared)
            }
            (Display::Fallback(view), ReaderResult::Rendered(rendered)) => {
                view.on_rendered(&state, rendered);
                self.reading.max_scroll = view.max_scroll();
                self.reading.scroll = self.reading.scroll.min(self.reading.max_scroll);
            }
            _ => {}
        }
    }

    /// Removes this reader's pages from the terminal; returns what to write.
    pub fn close(&mut self) -> Vec<u8> {
        match &mut self.display {
            Display::Kitty(view) => view.close(),
            Display::Fallback(_) => Vec::new(),
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.notice = None;
        if self.end_prompt {
            self.end_prompt = false;
            match key.code {
                KeyCode::Enter => return self.next_chapter(),
                KeyCode::Esc => return None,
                _ => {}
            }
        }
        let last_view = self.reading.views.len().saturating_sub(1);
        let reading = &mut self.reading;
        match key.code {
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::PageDown => {
                if reading.view < last_view {
                    self.go_to(self.reading.view + 1)
                } else {
                    self.end_prompt = self.reading.error.is_none();
                    None
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp => {
                let previous = reading.view.saturating_sub(1);
                self.go_to(previous)
            }
            KeyCode::Home => self.go_to(0),
            KeyCode::End => self.go_to(last_view),
            KeyCode::Down | KeyCode::Char('j') if reading.fit == Fit::Width => {
                reading.scroll = (reading.scroll + SCROLL_STEP).min(reading.max_scroll);
                None
            }
            KeyCode::Up | KeyCode::Char('k') if reading.fit == Fit::Width => {
                reading.scroll = reading.scroll.saturating_sub(SCROLL_STEP);
                None
            }
            KeyCode::Char('s') => {
                let page = reading.first_page();
                reading.layout = match reading.layout {
                    PageLayout::Single => PageLayout::Spread,
                    PageLayout::Spread => PageLayout::Single,
                };
                reading.views = page_views(reading.pages.len(), reading.layout);
                reading.view = reading
                    .views
                    .iter()
                    .position(|view| view.contains(&page))
                    .unwrap_or(0);
                reading.scroll = 0;
                if let Display::Fallback(view) = &mut self.display {
                    view.reset_prefetch();
                }
                self.reading.progress_action()
            }
            KeyCode::Char('f') => {
                reading.fit = match reading.fit {
                    Fit::Page => Fit::Width,
                    Fit::Width => Fit::Page,
                };
                reading.scroll = 0;
                None
            }
            KeyCode::Char('r') => {
                reading.direction = reading.direction.toggled();
                Some(Action::SetDirection {
                    manga: reading.manga.key.clone(),
                    direction: reading.direction,
                })
            }
            KeyCode::Esc | KeyCode::Backspace => Some(Action::CloseReader),
            KeyCode::Char('q') => Some(Action::Quit),
            _ => None,
        }
    }

    fn go_to(&mut self, view: usize) -> Option<Action> {
        if view == self.reading.view {
            return None;
        }
        self.reading.view = view;
        self.reading.scroll = 0;
        if let Display::Fallback(fallback) = &mut self.display {
            fallback.trim_cache(&self.reading.state());
        }
        self.reading.progress_action()
    }

    fn next_chapter(&self) -> Option<Action> {
        let next = self.reading.chapter + 1;
        (next < self.reading.manga.chapters.len()).then(|| Action::OpenReader {
            manga: self.reading.manga.clone(),
            chapter: next,
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let (image_area, status) = split(area);
        if let Some(err) = &self.reading.error {
            let message = Paragraph::new(format!("Couldn't open this chapter: {err}").red())
                .wrap(Wrap { trim: true })
                .centered();
            frame.render_widget(message, centered(image_area, image_area.width, 2));
        } else if self.reading.can_show() {
            let state = self.reading.state();
            match &mut self.display {
                Display::Kitty(view) => view.render(frame, image_area, &state),
                Display::Fallback(view) => view.render(frame, image_area, &state),
            }
        }
        self.render_status(frame, status);
        if self.end_prompt {
            self.render_end_prompt(frame, image_area);
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let reading = &self.reading;
        let position = match reading.views.get(reading.view).map(Vec::as_slice) {
            Some([page]) => format!("{}/{}", page + 1, reading.pages.len()),
            Some([first, second]) => {
                format!("{}–{}/{}", first + 1, second + 1, reading.pages.len())
            }
            _ => "–".to_owned(),
        };
        let layout = match reading.layout {
            PageLayout::Single => "Single",
            PageLayout::Spread => "Spread",
        };
        let fit = match reading.fit {
            Fit::Page => "Fit page",
            Fit::Width => "Fit width",
        };
        let info = Line::from(vec![
            Span::raw(format!(" {} ", reading.manga.title)).bold(),
            Span::raw(format!("· {} · ", reading.local_chapter().name)),
            Span::raw(position).cyan(),
            Span::raw(format!(
                " · {} · {layout} · {fit} ",
                reading.direction.label()
            ))
            .dark_gray(),
        ]);
        let hints = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&[
                ("←→", "page"),
                ("s", "spread"),
                ("f", "fit"),
                ("r", "direction"),
                ("Esc", "back"),
            ]),
        };
        // Hints only appear when they fit next to the page info.
        let info_width = info.width() as u16;
        if info_width + hints.width() as u16 > area.width {
            frame.render_widget(Paragraph::new(info), area);
            return;
        }
        let [left, right] =
            Layout::horizontal([Constraint::Length(info_width), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new(info), left);
        frame.render_widget(Paragraph::new(hints).right_aligned(), right);
    }

    fn render_end_prompt(&self, frame: &mut Frame, area: Rect) {
        let next = self.reading.manga.chapters.get(self.reading.chapter + 1);
        let text = match next {
            Some(next) => Text::from(vec![
                Line::from("End of chapter".bold()),
                Line::from(format!("Next: {}", next.name)),
                Line::default(),
                key_hints(&[("Enter", "read next chapter"), ("Esc", "stay")]),
            ]),
            None => Text::from(vec![
                Line::from("End of chapter".bold()),
                Line::from("This is the last downloaded chapter.".dark_gray()),
                Line::default(),
                key_hints(&[("Esc", "stay")]),
            ]),
        };
        let width = (text.width() as u16 + 4).min(area.width);
        let popup = centered(area, width, 6);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(text).centered().block(panel(" Done ", true)),
            popup,
        );
    }
}

/// The page area and the status line below it.
fn split(area: Rect) -> (Rect, Rect) {
    let [image, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    (image, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ChapterKind;
    use crate::library::state::Progress;
    use crossterm::event::KeyModifiers;
    use image::DynamicImage;
    use std::path::Path;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A manga with chapters "A" (5 pages) and "B", stored as image folders in `dir`.
    fn manga(dir: &Path) -> LibraryManga {
        let chapter = |name: &str, pages: usize| {
            let path = dir.join(name);
            std::fs::create_dir_all(&path).unwrap();
            for page in 1..=pages {
                DynamicImage::new_rgb8(20, 30)
                    .save(path.join(format!("{page:03}.png")))
                    .unwrap();
            }
            LocalChapter {
                name: name.to_owned(),
                file_name: name.to_owned(),
                path,
                kind: ChapterKind::Folder,
            }
        };
        LibraryManga {
            key: "Test".to_owned(),
            title: "Test".to_owned(),
            chapters: vec![chapter("A", 5), chapter("B", 2)],
        }
    }

    fn state(dir: &Path) -> ReadingState {
        ReadingState::load(dir.join("library.json")).0
    }

    fn graphics(kitty: Option<Medium>) -> Graphics {
        Graphics {
            picker: Picker::halfblocks(),
            kitty,
        }
    }

    fn open(
        dir: &Path,
        state: &ReadingState,
        kitty: Option<Medium>,
    ) -> (ReaderScreen, Option<Action>) {
        ReaderScreen::new(
            manga(dir),
            0,
            state,
            &graphics(kitty),
            PageLayout::Single,
            Fit::Page,
        )
    }

    fn progress(action: Option<Action>) -> (usize, bool) {
        match action {
            Some(Action::SaveProgress { page, finished, .. }) => (page, finished),
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn resumes_and_marks_read_on_the_last_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.record("Test", "A", 3, false);

        let (mut reader, action) = open(dir.path(), &state, None);
        assert_eq!(progress(action), (3, false));
        assert_eq!(progress(reader.handle_key(key(KeyCode::Right))), (4, true));

        // Past the last page the reader offers the next chapter instead of moving on.
        assert!(reader.handle_key(key(KeyCode::Right)).is_none());
        assert!(reader.end_prompt);
        match reader.handle_key(key(KeyCode::Enter)) {
            Some(Action::OpenReader { chapter, .. }) => assert_eq!(chapter, 1),
            other => panic!("expected the next chapter, got {other:?}"),
        }
    }

    #[test]
    fn a_finished_chapter_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        state.record("Test", "A", 4, true);
        let (_, action) = open(dir.path(), &state, None);
        assert_eq!(progress(action), (0, false));
        assert_eq!(
            state.progress("Test", "A"),
            Some(Progress {
                page: 4,
                read: true
            })
        );
    }

    #[test]
    fn switching_to_spreads_keeps_the_page_and_renders_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reader, _) = open(dir.path(), &state(dir.path()), None);
        reader.handle_key(key(KeyCode::Right));
        reader.handle_key(key(KeyCode::Right));
        assert_eq!(
            progress(reader.handle_key(key(KeyCode::Char('s')))),
            (1, false)
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| reader.render(frame, frame.area()))
            .unwrap();
        let text = crate::ui::buffer_text(&terminal);
        assert!(text.contains("Loading page…"));
        assert!(text.contains("2–3/5 · RTL · Spread · Fit page"));

        let jobs = reader.take_requests();
        assert_eq!(jobs.len(), 1);
        let result = jobs.into_iter().next().unwrap().run();
        reader.on_job_done(result);
        let prefetch = reader.take_requests();
        assert_eq!(
            prefetch.len(),
            1,
            "the next spread is decoded ahead of time"
        );
    }

    #[test]
    fn kitty_pages_show_up_as_placeholders_once_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reader, _) = open(dir.path(), &state(dir.path()), Some(Medium::Direct));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        let area = Rect::new(0, 0, 80, 20);

        reader.prepare_frame(area);
        terminal
            .draw(|frame| reader.render(frame, frame.area()))
            .unwrap();
        assert!(crate::ui::buffer_text(&terminal).contains("Loading page…"));

        for job in reader.take_requests() {
            let result = job.run();
            reader.on_job_done(result);
        }
        reader.prepare_frame(area);
        let output = String::from_utf8(reader.take_output()).unwrap();
        assert_eq!(
            output.matches("a=T,U=1").count(),
            3,
            "current page and two ahead"
        );
        terminal
            .draw(|frame| reader.render(frame, frame.area()))
            .unwrap();
        assert!(crate::ui::buffer_text(&terminal).contains('\u{10EEEE}'));

        let closing = String::from_utf8(reader.close()).unwrap();
        assert_eq!(closing.matches("a=d,d=I").count(), 3);
    }

    #[test]
    fn results_for_a_closed_reader_are_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let shm = tempfile::tempdir().unwrap();
        let medium = Medium::TempFile(shm.path().to_owned());
        let (mut old, _) = open(dir.path(), &state(dir.path()), Some(medium.clone()));
        old.prepare_frame(Rect::new(0, 0, 80, 20));
        let jobs = old.take_requests();
        assert!(!jobs.is_empty());

        let (mut new, _) = open(dir.path(), &state(dir.path()), Some(medium));
        for job in jobs {
            new.on_job_done(job.run());
        }
        assert_eq!(std::fs::read_dir(shm.path()).unwrap().count(), 0);
    }
}
