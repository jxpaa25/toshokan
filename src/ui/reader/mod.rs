//! Reader: shows a downloaded chapter's pages as images in the terminal, one page or spread
//! at a time, or as one long strip scrolled through (webtoons).
//!
//! Terminals with the kitty graphics protocol (Ghostty, kitty) get pages sent ahead of time
//! and scaled by the terminal ([`kitty_view`], [`kitty_strip`]); everything else goes through
//! `ratatui-image` ([`fallback`], [`fallback_strip`]). Either way the slow work happens on
//! worker threads: the screen hands out jobs with [`ReaderScreen::take_requests`], the app runs
//! them with [`ReaderJob::run`], and results come back through [`ReaderScreen::on_job_done`].

mod fallback;
mod fallback_strip;
#[cfg(test)]
mod ghostty_sim;
mod kitty_strip;
mod kitty_view;
pub mod layout;

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui_image::picker::Picker;

use self::fallback::{FallbackView, RenderJob, Rendered, centered};
use self::fallback_strip::{FallbackStrip, StripRenderJob, StripRendered};
use self::kitty_strip::{StripJob, StripPrepared, StripView};
use self::kitty_view::{KittyView, PrepareJob, Prepared};
use self::layout::{StripLayout, looks_like_strip, page_views, strip_samples, strip_width};
use super::kitty::Medium;
use super::{key_hints, panel};
use crate::app::Action;
use crate::library::state::{Direction, ReadingState};
use crate::library::{LibraryManga, LocalChapter, pages};

/// Rows moved per scroll key press or wheel step in fit-width and strip mode.
const SCROLL_STEP: u16 = 3;
/// Rows of the last screen kept in view when a strip moves on by a screen.
const SCREEN_OVERLAP: u16 = 2;
/// The wheel turns a page at most this often, so one flick doesn't skip several.
const WHEEL_TURN_GAP: Duration = Duration::from_millis(300);

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
    Measure(MeasureJob),
    Strip(StripJob),
    StripRender(StripRenderJob),
}

impl ReaderJob {
    pub fn run(self) -> ReaderResult {
        match self {
            ReaderJob::Prepare(job) => ReaderResult::Prepared(job.run()),
            ReaderJob::Render(job) => ReaderResult::Rendered(job.run()),
            ReaderJob::Measure(job) => ReaderResult::Measured(job.run()),
            ReaderJob::Strip(job) => ReaderResult::StripPrepared(job.run()),
            ReaderJob::StripRender(job) => ReaderResult::StripRendered(job.run()),
        }
    }
}

pub enum ReaderResult {
    Prepared(Prepared),
    Rendered(Rendered),
    Measured(Measured),
    StripPrepared(StripPrepared),
    StripRendered(StripRendered),
}

impl ReaderResult {
    fn reader_id(&self) -> u64 {
        match self {
            ReaderResult::Prepared(prepared) => prepared.reader_id,
            ReaderResult::Rendered(rendered) => rendered.reader_id,
            ReaderResult::Measured(measured) => measured.reader_id,
            ReaderResult::StripPrepared(prepared) => prepared.reader_id,
            ReaderResult::StripRendered(rendered) => rendered.reader_id,
        }
    }
}

impl fmt::Debug for ReaderResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            ReaderResult::Prepared(_) => "Prepared",
            ReaderResult::Rendered(_) => "Rendered",
            ReaderResult::Measured(_) => "Measured",
            ReaderResult::StripPrepared(_) => "StripPrepared",
            ReaderResult::StripRendered(_) => "StripRendered",
        };
        write!(f, "ReaderResult::{kind}(reader {})", self.reader_id())
    }
}

/// Reads every page's size from its header, to lay out a long strip.
pub struct MeasureJob {
    reader_id: u64,
    chapter: LocalChapter,
    pages: Vec<String>,
}

pub struct Measured {
    reader_id: u64,
    sizes: Vec<Option<(u32, u32)>>,
}

impl MeasureJob {
    fn run(self) -> Measured {
        Measured {
            reader_id: self.reader_id,
            sizes: pages::measure(&self.chapter, &self.pages),
        }
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
    KittyStrip(StripView),
    FallbackStrip(FallbackStrip),
}

impl Display {
    fn new(graphics: &Graphics, strip: bool) -> Self {
        let font = graphics.picker.font_size();
        let cell = (font.width, font.height);
        match (&graphics.kitty, strip) {
            (Some(medium), false) => Display::Kitty(KittyView::new(medium.clone(), cell)),
            (Some(medium), true) => Display::KittyStrip(StripView::new(medium.clone(), cell)),
            (None, false) => Display::Fallback(FallbackView::new(graphics.picker.clone())),
            (None, true) => Display::FallbackStrip(FallbackStrip::new(graphics.picker.clone())),
        }
    }

    /// Removes the display's images from the terminal; returns what to write.
    fn close(&mut self) -> Vec<u8> {
        match self {
            Display::Kitty(view) => view.close(),
            Display::KittyStrip(view) => view.close(),
            Display::Fallback(_) | Display::FallbackStrip(_) => Vec::new(),
        }
    }
}

/// The popup offering another chapter, shown on going on past either end of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prompt {
    /// Before the first page.
    Previous,
    /// After the last page.
    Next,
}

/// Where the reader is in a long strip.
struct Strip {
    /// Every page's pixel size once measured; `None` for a page that can't be read.
    sizes: Option<Vec<Option<(u32, u32)>>>,
    measuring: bool,
    /// The strip's width in pixels, from the pages' sizes.
    width: u32,
    /// The page at the top of the screen and how far down it the screen starts (0..1): a
    /// share of the page, so resizing keeps the reader's place. 1 on the last page means the
    /// bottom of the strip, for a chapter opened at its end.
    page: usize,
    offset: f64,
    /// This frame's layout, once the pages are measured, and how far it's scrolled.
    layout: Option<StripLayout>,
    scroll: u32,
}

impl Strip {
    fn at(page: usize) -> Self {
        Strip {
            sizes: None,
            measuring: false,
            width: 1,
            page,
            offset: 0.0,
            layout: None,
            scroll: 0,
        }
    }
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
    /// Set in long-strip mode, where `views` and `view` only remember the page to go back to.
    strip: Option<Strip>,
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

    /// The page at the top of the screen.
    fn first_page(&self) -> usize {
        if let Some(strip) = &self.strip {
            return match &strip.layout {
                Some(layout) => layout.locate(strip.scroll).0,
                None => strip.page,
            };
        }
        self.views
            .get(self.view)
            .and_then(|view| view.first().copied())
            .unwrap_or(0)
    }

    /// Saves the current page; the chapter counts as read once its last page is on screen.
    fn progress_action(&self) -> Option<Action> {
        let (page, finished) = match &self.strip {
            // At the end the last page is saved, so a finished chapter starts over next time.
            Some(_) if self.strip_at_end() => (self.pages.len() - 1, true),
            Some(_) => (self.first_page(), false),
            None => {
                let view = self.views.get(self.view)?;
                (view[0], view.contains(&(self.pages.len() - 1)))
            }
        };
        Some(Action::SaveProgress {
            manga: self.manga.key.clone(),
            chapter: self.local_chapter().file_name.clone(),
            page,
            finished,
        })
    }

    /// Lays out the strip in `area` with `cell`-sized cells, keeping the reader's place.
    fn lay_out_strip(&mut self, area: Rect, cell: (u16, u16)) {
        let fit = self.fit;
        let Some(strip) = &mut self.strip else {
            return;
        };
        let Some(sizes) = &strip.sizes else {
            return;
        };
        let layout = StripLayout::new(sizes, strip.width, fit, area, cell);
        let page = strip.page.min(sizes.len().saturating_sub(1));
        let within = (strip.offset * f64::from(layout.rows(page))).round() as u32;
        strip.scroll = (layout.start(page) + within).min(layout.max_scroll());
        strip.layout = Some(layout);
    }

    /// The strip's layout and scroll, once it's laid out.
    fn strip_position(&self) -> Option<(&StripLayout, u32)> {
        let strip = self.strip.as_ref()?;
        Some((strip.layout.as_ref()?, strip.scroll))
    }

    /// Scrolls the strip by `delta` rows, as far as it goes. Returns whether it moved.
    fn scroll_strip(&mut self, delta: i64) -> bool {
        let Some(strip) = &mut self.strip else {
            return false;
        };
        let Some(layout) = &strip.layout else {
            return false;
        };
        let max = i64::from(layout.max_scroll());
        let scroll = (i64::from(strip.scroll) + delta).clamp(0, max) as u32;
        if scroll == strip.scroll {
            return false;
        }
        let (page, row) = layout.locate(scroll);
        strip.scroll = scroll;
        strip.page = page;
        strip.offset = f64::from(row) / f64::from(layout.rows(page));
        true
    }

    /// Whether the bottom of the strip is on screen.
    fn strip_at_end(&self) -> bool {
        match (self.strip_position(), &self.strip) {
            (Some((layout, scroll)), _) => scroll >= layout.max_scroll(),
            // Not laid out yet: only when opened at the end.
            (None, Some(strip)) => strip.offset >= 1.0,
            (None, None) => false,
        }
    }
}

pub struct ReaderScreen {
    reading: Reading,
    display: Display,
    /// To make the other display when switching between pages and strip.
    graphics: Graphics,
    /// What a display that was replaced left to write to the terminal.
    output: Vec<u8>,
    prompt: Option<Prompt>,
    notice: Option<String>,
    /// The last page turned with the mouse wheel.
    wheel_turn: Option<Instant>,
    /// The strip position last saved, so scrolling within a page doesn't save it again.
    saved: Option<(usize, bool)>,
}

impl ReaderScreen {
    /// Opens `manga.chapters[chapter]` where the user left off, or at its last page with
    /// `at_end`, and returns the progress to record for opening it.
    pub fn new(
        manga: LibraryManga,
        chapter: usize,
        at_end: bool,
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
            _ if at_end => last_view,
            Some(progress) => match view_of(progress.page) {
                Some(view) if progress.read && view == last_view => 0,
                Some(view) => view,
                None => 0,
            },
            None => 0,
        };
        let strip = error.is_none()
            && state
                .strip(&manga.key)
                .unwrap_or_else(|| is_long_strip(local, &pages));
        let first_page = views.get(view).map_or(0, |view| view[0]);
        let strip = strip.then(|| Strip {
            offset: if at_end { 1.0 } else { 0.0 },
            ..Strip::at(first_page)
        });
        let display = Display::new(graphics, strip.is_some());
        let mut screen = ReaderScreen {
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
                strip,
            },
            display,
            graphics: graphics.clone(),
            output: Vec::new(),
            prompt: None,
            notice: None,
            wheel_turn: None,
            saved: None,
        };
        let action = screen.reading.progress_action();
        let reading = &screen.reading;
        let saved = reading
            .strip
            .is_some()
            .then(|| (first_page, reading.strip_at_end()));
        screen.saved = saved;
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
        if !self.reading.can_show() {
            return;
        }
        let (image_area, _) = split(area);
        match &mut self.display {
            Display::Kitty(view) => {
                let max_scroll = view.prepare_frame(&self.reading.state(), image_area);
                self.reading.max_scroll = max_scroll;
                self.reading.scroll = self.reading.scroll.min(max_scroll);
            }
            Display::KittyStrip(view) => {
                let cell = view.cell(image_area);
                self.reading.lay_out_strip(image_area, cell);
                if let Some((layout, scroll)) = self.reading.strip_position() {
                    view.prepare_frame(layout, scroll);
                }
            }
            Display::Fallback(_) | Display::FallbackStrip(_) => {}
        }
    }

    /// Escape sequences to write to the terminal before the next frame.
    pub fn take_output(&mut self) -> Vec<u8> {
        let mut output = std::mem::take(&mut self.output);
        match &mut self.display {
            Display::Kitty(view) => output.extend(view.take_output()),
            Display::KittyStrip(view) => output.extend(view.take_output()),
            Display::Fallback(_) | Display::FallbackStrip(_) => {}
        }
        output
    }

    /// Work to run on worker threads.
    pub fn take_requests(&mut self) -> Vec<ReaderJob> {
        let can_show = self.reading.can_show();
        let mut jobs = Vec::new();
        if let Some(strip) = &mut self.reading.strip
            && can_show
            && strip.sizes.is_none()
            && !strip.measuring
        {
            strip.measuring = true;
            jobs.push(ReaderJob::Measure(MeasureJob {
                reader_id: self.reading.id,
                chapter: self.reading.manga.chapters[self.reading.chapter].clone(),
                pages: self.reading.pages.clone(),
            }));
        }
        let state = self.reading.state();
        match &mut self.display {
            Display::Kitty(view) if can_show => jobs.extend(
                view.take_requests(&state)
                    .into_iter()
                    .map(ReaderJob::Prepare),
            ),
            Display::Kitty(_) => {}
            Display::Fallback(view) => {
                jobs.extend(view.take_requests().into_iter().map(ReaderJob::Render))
            }
            Display::KittyStrip(view) => {
                if let Some((layout, _)) = self.reading.strip_position() {
                    let requests = view.take_requests(&state, layout);
                    jobs.extend(requests.into_iter().map(ReaderJob::Strip));
                }
            }
            Display::FallbackStrip(view) => {
                jobs.extend(view.take_requests().into_iter().map(ReaderJob::StripRender))
            }
        }
        jobs
    }

    pub fn on_job_done(&mut self, result: ReaderResult) {
        // A result for a reader that's gone is dropped, which also removes its page file.
        if result.reader_id() != self.reading.id {
            return;
        }
        if let ReaderResult::Measured(measured) = result {
            if let Some(strip) = &mut self.reading.strip {
                strip.width = strip_width(&measured.sizes);
                strip.sizes = Some(measured.sizes);
                strip.measuring = false;
            }
            return;
        }
        let state = self.reading.state();
        let strip = self.reading.strip_position();
        match (&mut self.display, result) {
            (Display::Kitty(view), ReaderResult::Prepared(prepared)) => {
                view.on_prepared(&state, prepared)
            }
            (Display::Fallback(view), ReaderResult::Rendered(rendered)) => {
                view.on_rendered(&state, rendered);
                self.reading.max_scroll = view.max_scroll();
                self.reading.scroll = self.reading.scroll.min(self.reading.max_scroll);
            }
            (Display::KittyStrip(view), ReaderResult::StripPrepared(prepared)) => {
                if let Some((layout, scroll)) = strip {
                    view.on_prepared(layout, scroll, prepared);
                }
            }
            (Display::FallbackStrip(view), ReaderResult::StripRendered(rendered)) => {
                if let Some((layout, scroll)) = strip {
                    view.on_rendered(&state, layout, scroll, rendered);
                }
            }
            _ => {}
        }
    }

    /// Removes this reader's pages from the terminal; returns what to write.
    pub fn close(&mut self) -> Vec<u8> {
        let mut output = std::mem::take(&mut self.output);
        output.extend(self.display.close());
        output
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.notice = None;
        if let Some(prompt) = self.prompt.take() {
            match key.code {
                KeyCode::Enter => return self.open_offered(prompt),
                KeyCode::Esc => return None,
                _ => {}
            }
        }
        let reading = &mut self.reading;
        match key.code {
            KeyCode::Char('w') => self.toggle_strip(),
            KeyCode::Char('f') => {
                reading.fit = match reading.fit {
                    Fit::Page => Fit::Width,
                    Fit::Width => Fit::Page,
                };
                // A strip keeps its place; a page starts again from its top.
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
            code if reading.strip.is_some() => self.strip_key(code),
            code => self.page_key(code),
        }
    }

    /// The mouse wheel scrolls a strip or a page in fit-width mode, and turns pages otherwise
    /// (including past either end of a fit-width page).
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        let down = match mouse.kind {
            MouseEventKind::ScrollDown => true,
            MouseEventKind::ScrollUp => false,
            _ => return None,
        };
        self.notice = None;
        if let Some(prompt) = self.prompt {
            // Going on past the end keeps the popup up; going back closes it.
            if down == (prompt == Prompt::Next) {
                return None;
            }
            self.prompt = None;
        }
        let step = i64::from(SCROLL_STEP) * if down { 1 } else { -1 };
        if self.reading.strip.is_some() {
            return self.scroll_strip(step, true);
        }
        let reading = &mut self.reading;
        if reading.fit == Fit::Width {
            let max = i64::from(reading.max_scroll);
            let scroll = (i64::from(reading.scroll) + step).clamp(0, max) as u16;
            if scroll != reading.scroll {
                reading.scroll = scroll;
                return None;
            }
        }
        if self
            .wheel_turn
            .is_some_and(|turned| turned.elapsed() < WHEEL_TURN_GAP)
        {
            return None;
        }
        self.wheel_turn = Some(Instant::now());
        if down {
            return self.next_view();
        }
        let action = self.previous_view();
        if self.reading.fit == Fit::Width && action.is_some() {
            // Scrolling up reaches the bottom of the page before.
            self.reading.scroll = u16::MAX;
        }
        action
    }

    fn page_key(&mut self, code: KeyCode) -> Option<Action> {
        let last_view = self.reading.views.len().saturating_sub(1);
        let reading = &mut self.reading;
        match code {
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::PageDown => {
                self.next_view()
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp => self.previous_view(),
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
            _ => None,
        }
    }

    fn strip_key(&mut self, code: KeyCode) -> Option<Action> {
        let (layout, _) = self.reading.strip_position()?;
        let screen = i64::from(layout.height.saturating_sub(SCREEN_OVERLAP).max(1));
        let far = i64::from(layout.total_rows());
        let step = i64::from(SCROLL_STEP);
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.scroll_strip(step, true),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_strip(-step, true),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::PageDown => {
                self.scroll_strip(screen, true)
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp => {
                self.scroll_strip(-screen, true)
            }
            KeyCode::Home => self.scroll_strip(-far, false),
            KeyCode::End => self.scroll_strip(far, false),
            KeyCode::Char('s') => {
                self.notice = Some("Long strips have no spreads; w shows single pages".to_owned());
                None
            }
            _ => None,
        }
    }

    /// Scrolls the strip and saves the position when it reaches another page. Going on past
    /// either end offers the next or previous chapter when `prompt` is set.
    fn scroll_strip(&mut self, delta: i64, prompt: bool) -> Option<Action> {
        if !self.reading.scroll_strip(delta) {
            if prompt && self.reading.strip_position().is_some() {
                self.prompt = Some(if delta > 0 {
                    Prompt::Next
                } else {
                    Prompt::Previous
                });
            } else {
                return None;
            }
        }
        let position = (self.reading.first_page(), self.reading.strip_at_end());
        if self.saved == Some(position) {
            return None;
        }
        self.saved = Some(position);
        self.reading.progress_action()
    }

    /// Switches between the long strip and single pages or spreads, at the same page.
    fn toggle_strip(&mut self) -> Option<Action> {
        if self.reading.error.is_some() {
            return None;
        }
        let page = self.reading.first_page();
        let reading = &mut self.reading;
        let strip = reading.strip.is_none();
        if strip {
            reading.strip = Some(Strip::at(page));
        } else {
            reading.strip = None;
            reading.view = reading
                .views
                .iter()
                .position(|view| view.contains(&page))
                .unwrap_or(0);
            reading.scroll = 0;
        }
        self.output.extend(self.display.close());
        self.display = Display::new(&self.graphics, strip);
        self.saved = strip.then_some((page, false));
        Some(Action::SetStrip {
            manga: self.reading.manga.key.clone(),
            strip,
        })
    }

    /// The next view, or the end-of-chapter prompt after the last one.
    fn next_view(&mut self) -> Option<Action> {
        if self.reading.view + 1 < self.reading.views.len() {
            self.go_to(self.reading.view + 1)
        } else {
            self.prompt = self.reading.error.is_none().then_some(Prompt::Next);
            None
        }
    }

    /// The previous view, or the start-of-chapter prompt before the first one.
    fn previous_view(&mut self) -> Option<Action> {
        if self.reading.view > 0 {
            self.go_to(self.reading.view - 1)
        } else {
            self.prompt = self.reading.error.is_none().then_some(Prompt::Previous);
            None
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

    /// The chapter `prompt` offers, unless there's none that way.
    fn offered(&self, prompt: Prompt) -> Option<usize> {
        let chapter = match prompt {
            Prompt::Previous => self.reading.chapter.checked_sub(1)?,
            Prompt::Next => self.reading.chapter + 1,
        };
        (chapter < self.reading.manga.chapters.len()).then_some(chapter)
    }

    fn open_offered(&self, prompt: Prompt) -> Option<Action> {
        Some(Action::OpenReader {
            manga: self.reading.manga.clone(),
            chapter: self.offered(prompt)?,
            // Going back carries on from the previous chapter's last page.
            at_end: prompt == Prompt::Previous,
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let (image_area, status) = split(area);
        if let Display::FallbackStrip(view) = &self.display {
            let cell = view.cell();
            self.reading.lay_out_strip(image_area, cell);
        }
        if let Some(err) = &self.reading.error {
            let message = Paragraph::new(format!("Couldn't open this chapter: {err}").red())
                .wrap(Wrap { trim: true })
                .centered();
            frame.render_widget(message, centered(image_area, image_area.width, 2));
        } else if self.reading.can_show() {
            let state = self.reading.state();
            let strip = self.reading.strip_position();
            match (&mut self.display, strip) {
                (Display::Kitty(view), _) => view.render(frame, image_area, &state),
                (Display::Fallback(view), _) => view.render(frame, image_area, &state),
                (Display::KittyStrip(view), Some((layout, scroll))) => {
                    view.render(frame, layout, scroll)
                }
                (Display::FallbackStrip(view), Some((layout, scroll))) => {
                    view.render(frame, &state, layout, scroll)
                }
                // Still measuring the pages.
                (Display::KittyStrip(_) | Display::FallbackStrip(_), None) => {
                    let loading = Paragraph::new("Loading page…".dark_gray()).centered();
                    frame.render_widget(loading, centered(image_area, image_area.width, 1));
                }
            }
        }
        self.render_status(frame, status);
        if let Some(prompt) = self.prompt {
            self.render_prompt(frame, image_area, prompt);
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let reading = &self.reading;
        let position = match reading.views.get(reading.view).map(Vec::as_slice) {
            _ if reading.strip.is_some() && reading.can_show() => {
                format!("{}/{}", reading.first_page() + 1, reading.pages.len())
            }
            Some([page]) => format!("{}/{}", page + 1, reading.pages.len()),
            Some([first, second]) => {
                format!("{}–{}/{}", first + 1, second + 1, reading.pages.len())
            }
            _ => "–".to_owned(),
        };
        let layout = match reading.layout {
            _ if reading.strip.is_some() => "Strip",
            PageLayout::Single => "Single",
            PageLayout::Spread => "Spread",
        };
        let fit = match (reading.fit, reading.strip.is_some()) {
            (Fit::Page, false) => "Fit page",
            (Fit::Width, false) => "Fit width",
            (Fit::Page, true) => "Page width",
            (Fit::Width, true) => "Full width",
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
            None if reading.strip.is_some() => key_hints(&[
                ("↑↓", "scroll"),
                ("Space", "screen"),
                ("f", "width"),
                ("w", "pages"),
                ("Esc", "back"),
            ]),
            None => key_hints(&[
                ("←→", "page"),
                ("s", "spread"),
                ("f", "fit"),
                ("w", "strip"),
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

    fn render_prompt(&self, frame: &mut Frame, area: Rect, prompt: Prompt) {
        let (heading, title, label, read, none) = match prompt {
            Prompt::Previous => (
                "Start of chapter",
                " Start ",
                "Previous",
                "read previous chapter",
                "This is the first downloaded chapter.",
            ),
            Prompt::Next => (
                "End of chapter",
                " Done ",
                "Next",
                "read next chapter",
                "This is the last downloaded chapter.",
            ),
        };
        let offered = self
            .offered(prompt)
            .map(|chapter| &self.reading.manga.chapters[chapter]);
        let text = match offered {
            Some(chapter) => Text::from(vec![
                Line::from(heading.bold()),
                Line::from(format!("{label}: {}", chapter.name)),
                Line::default(),
                key_hints(&[("Enter", read), ("Esc", "stay")]),
            ]),
            None => Text::from(vec![
                Line::from(heading.bold()),
                Line::from(none.dark_gray()),
                Line::default(),
                key_hints(&[("Esc", "stay")]),
            ]),
        };
        let width = (text.width() as u16 + 4).min(area.width);
        let popup = centered(area, width, 6);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(text).centered().block(panel(title, true)),
            popup,
        );
    }
}

/// Whether a chapter is a long strip, going by the shapes of a few of its pages.
fn is_long_strip(chapter: &LocalChapter, pages: &[String]) -> bool {
    let sampled: Vec<String> = strip_samples(pages.len())
        .into_iter()
        .map(|page| pages[page].clone())
        .collect();
    let sizes: Vec<(u32, u32)> = pages::measure(chapter, &sampled)
        .into_iter()
        .flatten()
        .collect();
    looks_like_strip(&sizes)
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
            dir: None,
            link: None,
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
            false,
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
        assert_eq!(reader.prompt, Some(Prompt::Next));
        match reader.handle_key(key(KeyCode::Enter)) {
            Some(Action::OpenReader {
                chapter: 1,
                at_end: false,
                ..
            }) => {}
            other => panic!("expected the next chapter, got {other:?}"),
        }
    }

    #[test]
    fn before_the_first_page_offers_the_previous_chapter_at_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        let mut draw = |reader: &mut ReaderScreen| {
            terminal
                .draw(|frame| reader.render(frame, frame.area()))
                .unwrap();
            crate::ui::buffer_text(&terminal)
        };

        let (mut first, _) = open(dir.path(), &state, None);
        assert!(first.handle_key(key(KeyCode::Left)).is_none());
        assert_eq!(first.prompt, Some(Prompt::Previous));
        let text = draw(&mut first);
        assert!(text.contains("Start of chapter"), "{text}");
        assert!(text.contains("This is the first downloaded chapter."));
        assert!(first.handle_key(key(KeyCode::Enter)).is_none());
        assert_eq!(first.prompt, None);

        let (mut second, _) = ReaderScreen::new(
            manga(dir.path()),
            1,
            false,
            &state,
            &graphics(None),
            PageLayout::Single,
            Fit::Page,
        );
        second.handle_key(key(KeyCode::Left));
        let text = draw(&mut second);
        assert!(text.contains("Previous: A"), "{text}");
        assert!(text.contains("Enter read previous chapter"));
        let (manga, chapter) = match second.handle_key(key(KeyCode::Enter)) {
            Some(Action::OpenReader {
                manga,
                chapter,
                at_end: true,
            }) => (manga, chapter),
            other => panic!("expected the previous chapter at its end, got {other:?}"),
        };
        assert_eq!(chapter, 0);

        // It opens on the last page, which counts as reading it.
        let (reader, action) = ReaderScreen::new(
            manga,
            chapter,
            true,
            &state,
            &graphics(None),
            PageLayout::Single,
            Fit::Page,
        );
        assert_eq!(reader.reading.view, 4);
        assert_eq!(progress(action), (4, true));
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

    /// A manga whose one chapter is three 200x1200 strip pages, stored in `dir`.
    fn strip_manga(dir: &Path) -> LibraryManga {
        let path = dir.join("Strip");
        std::fs::create_dir_all(&path).unwrap();
        for page in 1..=3 {
            DynamicImage::new_rgb8(200, 1200)
                .save(path.join(format!("{page:03}.png")))
                .unwrap();
        }
        LibraryManga {
            key: "Webtoon".to_owned(),
            title: "Webtoon".to_owned(),
            chapters: vec![LocalChapter {
                name: "Strip".to_owned(),
                file_name: "Strip".to_owned(),
                path,
                kind: ChapterKind::Folder,
            }],
            dir: None,
            link: None,
        }
    }

    fn open_strip(dir: &Path, state: &ReadingState) -> ReaderScreen {
        let (reader, _) = ReaderScreen::new(
            strip_manga(dir),
            0,
            false,
            state,
            &graphics(None),
            PageLayout::Single,
            Fit::Page,
        );
        reader
    }

    #[test]
    fn long_strips_are_detected_unless_the_manga_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = state(dir.path());
        assert!(open_strip(dir.path(), &state).reading.strip.is_some());
        let (pages, _) = open(dir.path(), &state, None);
        assert!(pages.reading.strip.is_none(), "manga pages aren't a strip");

        let mut reader = open_strip(dir.path(), &state);
        match reader.handle_key(key(KeyCode::Char('w'))) {
            Some(Action::SetStrip { manga, strip }) => {
                assert_eq!((manga.as_str(), strip), ("Webtoon", false));
                state.set_strip(&manga, strip);
            }
            other => panic!("expected the strip setting, got {other:?}"),
        }
        assert!(open_strip(dir.path(), &state).reading.strip.is_none());
    }

    #[test]
    fn fallback_strips_render_in_the_background() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = open_strip(dir.path(), &state(dir.path()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        let mut draw = |reader: &mut ReaderScreen| {
            terminal
                .draw(|frame| reader.render(frame, frame.area()))
                .unwrap();
            crate::ui::buffer_text(&terminal)
        };
        let run_jobs = |reader: &mut ReaderScreen| {
            let jobs = reader.take_requests();
            let count = jobs.len();
            for job in jobs {
                let result = job.run();
                reader.on_job_done(result);
            }
            count
        };

        let text = draw(&mut reader);
        assert!(text.contains("Loading page…"));
        assert!(text.contains("1/3 · RTL · Strip · Page width"));
        assert_eq!(run_jobs(&mut reader), 1, "measuring the pages");
        draw(&mut reader);
        assert!(run_jobs(&mut reader) >= 1, "putting the screen together");
        let text = draw(&mut reader);
        assert!(!text.contains("Loading page…"), "{text}");

        // 200 px pages in 10 px cells: a 20-column strip of 60-row pages.
        let (layout, _) = reader.reading.strip_position().unwrap();
        assert_eq!((layout.cols, layout.rows(0)), (20, 60));
        reader.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(reader.reading.strip.as_ref().unwrap().scroll, 17);
        match reader.handle_key(key(KeyCode::End)) {
            Some(Action::SaveProgress { page, finished, .. }) => {
                assert_eq!((page, finished), (2, true))
            }
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn the_wheel_turns_one_page_per_flick() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reader, _) = open(dir.path(), &state(dir.path()), None);
        let wheel = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            progress(reader.handle_mouse(wheel(MouseEventKind::ScrollDown))),
            (1, false)
        );
        assert!(
            reader
                .handle_mouse(wheel(MouseEventKind::ScrollDown))
                .is_none()
        );
        assert!(reader.handle_mouse(wheel(MouseEventKind::Moved)).is_none());
        assert_eq!(reader.reading.view, 1);

        // Up past the first page offers the previous chapter; more of the same keeps the
        // popup up, the other way closes it.
        reader.wheel_turn = None;
        reader.handle_mouse(wheel(MouseEventKind::ScrollUp));
        reader.wheel_turn = None;
        reader.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(reader.prompt, Some(Prompt::Previous));
        reader.handle_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(reader.prompt, Some(Prompt::Previous));
        reader.wheel_turn = None;
        reader.handle_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!((reader.prompt, reader.reading.view), (None, 1));
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
