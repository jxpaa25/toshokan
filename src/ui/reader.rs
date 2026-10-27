//! Reader: shows a downloaded chapter's pages as images in the terminal.
//!
//! Decoding and scaling pages is slow, so it happens on a background thread: rendering asks
//! for the current view, the app runs the job with [`render_job`], and the result comes back
//! through [`ReaderScreen::on_rendered`]. The next view's pages are decoded ahead of time.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crossterm::event::{KeyCode, KeyEvent};
use image::imageops::{self, FilterType};
use image::{DynamicImage, RgbaImage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect, Size};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Image, Resize};

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

/// Everything that decides what the image on screen looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewKey {
    pages: Vec<usize>,
    size: (u16, u16),
    fit: Fit,
    scroll: u16,
    direction: Direction,
}

/// Background work: decode pages, and unless this is only a prefetch, scale and encode them
/// for the terminal.
pub struct RenderJob {
    reader_id: u64,
    chapter: LocalChapter,
    pages: Vec<(usize, String)>,
    cached: HashMap<usize, Arc<DynamicImage>>,
    target: Option<ViewKey>,
    picker: Picker,
}

/// The terminal image for a view and its largest scroll offset, or why it failed.
type Encoded = Result<(Protocol, u16), String>;

pub struct Rendered {
    reader_id: u64,
    decoded: Vec<(usize, Arc<DynamicImage>)>,
    view: Option<(ViewKey, Encoded)>,
}

impl fmt::Debug for Rendered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rendered")
            .field("reader_id", &self.reader_id)
            .field("decoded", &self.decoded.len())
            .finish_non_exhaustive()
    }
}

/// Runs a [`RenderJob`]. Blocking; call it from a background thread.
pub fn render_job(job: RenderJob) -> Rendered {
    let mut decoded = Vec::new();
    let mut images = Vec::new();
    let mut failure = None;
    for (index, name) in &job.pages {
        match job.cached.get(index) {
            Some(image) => images.push(image.clone()),
            None => match pages::load(&job.chapter, name) {
                Ok(image) => {
                    let image = Arc::new(image);
                    decoded.push((*index, image.clone()));
                    images.push(image);
                }
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            },
        }
    }
    let view = job.target.map(|key| {
        let result = match failure {
            Some(err) => Err(err),
            None => encode(&job.picker, &images, &key),
        };
        (key, result)
    });
    Rendered {
        reader_id: job.reader_id,
        decoded,
        view,
    }
}

/// Returns the terminal image for the view and the largest useful scroll offset.
fn encode(picker: &Picker, images: &[Arc<DynamicImage>], key: &ViewKey) -> Encoded {
    let image = compose(images, key.direction);
    let area = Size::new(key.size.0, key.size.1);
    let (image, max_scroll) = match key.fit {
        Fit::Page => (image, 0),
        Fit::Width => crop_to_width(image, area, picker.font_size(), key.scroll),
    };
    let protocol = picker
        .new_protocol(image, area, Resize::Scale(Some(FilterType::CatmullRom)))
        .map_err(|err| err.to_string())?;
    Ok((protocol, max_scroll))
}

/// One page as is, or two side by side at the same height. Right-to-left puts the later page
/// on the left, like an open manga volume.
fn compose(images: &[Arc<DynamicImage>], direction: Direction) -> DynamicImage {
    let [first, second] = images else {
        return images[0].as_ref().clone();
    };
    let height = first.height().max(second.height());
    let at_height = |image: &DynamicImage| {
        if image.height() == height {
            image.to_rgba8()
        } else {
            image
                .resize(u32::MAX, height, FilterType::CatmullRom)
                .to_rgba8()
        }
    };
    let (earlier, later) = (at_height(first), at_height(second));
    let (left, right) = match direction {
        Direction::Rtl => (later, earlier),
        Direction::Ltr => (earlier, later),
    };
    let mut canvas = RgbaImage::new(left.width() + right.width(), height);
    imageops::replace(&mut canvas, &left, 0, 0);
    imageops::replace(&mut canvas, &right, i64::from(left.width()), 0);
    DynamicImage::ImageRgba8(canvas)
}

/// The part of `image`, scaled to the width of `area`, that's visible `scroll` rows down.
/// Also returns the largest scroll that still shows something new.
fn crop_to_width(
    image: DynamicImage,
    area: Size,
    font: FontSize,
    scroll: u16,
) -> (DynamicImage, u16) {
    let view_width = f64::from(area.width) * f64::from(font.width);
    let view_height = f64::from(area.height) * f64::from(font.height);
    let scale = view_width / f64::from(image.width());
    let scaled_height = f64::from(image.height()) * scale;
    let max_scroll = ((scaled_height - view_height) / f64::from(font.height))
        .ceil()
        .max(0.0) as u16;
    let scroll = scroll.min(max_scroll);
    let top = ((f64::from(scroll) * f64::from(font.height) / scale) as u32).min(image.height() - 1);
    let height = ((view_height / scale) as u32).clamp(1, image.height() - top);
    (image.crop_imm(0, top, image.width(), height), max_scroll)
}

/// Pages grouped into what's shown at once.
fn page_views(count: usize, layout: PageLayout) -> Vec<Vec<usize>> {
    match layout {
        PageLayout::Single => (0..count).map(|page| vec![page]).collect(),
        PageLayout::Spread if count == 0 => Vec::new(),
        PageLayout::Spread => {
            let mut views = vec![vec![0]];
            views.extend((1..count).step_by(2).map(|page| {
                if page + 1 < count {
                    vec![page, page + 1]
                } else {
                    vec![page]
                }
            }));
            views
        }
    }
}

pub struct ReaderScreen {
    id: u64,
    manga: LibraryManga,
    chapter: usize,
    pages: Vec<String>,
    /// Set when the chapter can't be opened at all.
    error: Option<String>,
    layout: PageLayout,
    fit: Fit,
    direction: Direction,
    view: usize,
    scroll: u16,
    max_scroll: u16,
    picker: Picker,
    cache: HashMap<usize, Arc<DynamicImage>>,
    shown: Option<(ViewKey, Protocol)>,
    failed: Option<(ViewKey, String)>,
    pending: Option<ViewKey>,
    prefetched_view: Option<usize>,
    requests: Vec<RenderJob>,
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
        picker: Picker,
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
        let screen = ReaderScreen {
            id: NEXT_READER_ID.fetch_add(1, Ordering::Relaxed),
            direction: state.direction(&manga.key),
            manga,
            chapter,
            pages,
            error,
            layout,
            fit,
            view,
            scroll: 0,
            max_scroll: 0,
            picker,
            cache: HashMap::new(),
            shown: None,
            failed: None,
            pending: None,
            prefetched_view: None,
            requests: Vec::new(),
            end_prompt: false,
            notice: None,
        };
        let action = screen.progress_action();
        (screen, action)
    }

    /// The manga key and chapter file name being read.
    pub fn position(&self) -> (&str, &str) {
        (&self.manga.key, &self.local_chapter().file_name)
    }

    /// Layout and fit, so the next chapter opens the same way.
    pub fn preferences(&self) -> (PageLayout, Fit) {
        (self.layout, self.fit)
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Background work requested while rendering.
    pub fn take_requests(&mut self) -> Vec<RenderJob> {
        std::mem::take(&mut self.requests)
    }

    pub fn on_rendered(&mut self, rendered: Rendered) {
        if rendered.reader_id != self.id {
            return;
        }
        self.cache.extend(rendered.decoded);
        self.trim_cache();
        let Some((key, result)) = rendered.view else {
            return;
        };
        if self.pending.as_ref() != Some(&key) {
            return;
        }
        self.pending = None;
        match result {
            Ok((protocol, max_scroll)) => {
                self.max_scroll = max_scroll;
                self.scroll = self.scroll.min(max_scroll);
                self.shown = Some((key, protocol));
                self.prefetch_next();
            }
            Err(err) => self.failed = Some((key, err)),
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
        let last_view = self.views().len().saturating_sub(1);
        match key.code {
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') | KeyCode::PageDown => {
                if self.view < last_view {
                    self.go_to(self.view + 1)
                } else {
                    self.end_prompt = self.error.is_none();
                    None
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::PageUp => {
                self.go_to(self.view.saturating_sub(1))
            }
            KeyCode::Home => self.go_to(0),
            KeyCode::End => self.go_to(last_view),
            KeyCode::Down | KeyCode::Char('j') if self.fit == Fit::Width => {
                self.scroll = (self.scroll + SCROLL_STEP).min(self.max_scroll);
                None
            }
            KeyCode::Up | KeyCode::Char('k') if self.fit == Fit::Width => {
                self.scroll = self.scroll.saturating_sub(SCROLL_STEP);
                None
            }
            KeyCode::Char('s') => {
                let page = self.first_page();
                self.layout = match self.layout {
                    PageLayout::Single => PageLayout::Spread,
                    PageLayout::Spread => PageLayout::Single,
                };
                self.view = self
                    .views()
                    .iter()
                    .position(|view| view.contains(&page))
                    .unwrap_or(0);
                self.scroll = 0;
                self.prefetched_view = None;
                self.progress_action()
            }
            KeyCode::Char('f') => {
                self.fit = match self.fit {
                    Fit::Page => Fit::Width,
                    Fit::Width => Fit::Page,
                };
                self.scroll = 0;
                None
            }
            KeyCode::Char('r') => {
                self.direction = self.direction.toggled();
                Some(Action::SetDirection {
                    manga: self.manga.key.clone(),
                    direction: self.direction,
                })
            }
            KeyCode::Esc | KeyCode::Backspace => Some(Action::CloseReader),
            KeyCode::Char('q') => Some(Action::Quit),
            _ => None,
        }
    }

    fn go_to(&mut self, view: usize) -> Option<Action> {
        if view == self.view {
            return None;
        }
        self.view = view;
        self.scroll = 0;
        self.trim_cache();
        self.progress_action()
    }

    fn next_chapter(&self) -> Option<Action> {
        let next = self.chapter + 1;
        (next < self.manga.chapters.len()).then(|| Action::OpenReader {
            manga: self.manga.clone(),
            chapter: next,
        })
    }

    fn views(&self) -> Vec<Vec<usize>> {
        page_views(self.pages.len(), self.layout)
    }

    fn first_page(&self) -> usize {
        self.views()
            .get(self.view)
            .and_then(|view| view.first().copied())
            .unwrap_or(0)
    }

    fn local_chapter(&self) -> &LocalChapter {
        &self.manga.chapters[self.chapter]
    }

    /// Saves the current page; the chapter counts as read once its last page is on screen.
    fn progress_action(&self) -> Option<Action> {
        let view = self.views().get(self.view)?.clone();
        Some(Action::SaveProgress {
            manga: self.manga.key.clone(),
            chapter: self.local_chapter().file_name.clone(),
            page: view[0],
            finished: view.contains(&(self.pages.len() - 1)),
        })
    }

    /// Drops decoded pages that aren't in the previous, current or next view.
    fn trim_cache(&mut self) {
        let views = self.views();
        let keep: Vec<usize> = views
            .iter()
            .skip(self.view.saturating_sub(1))
            .take(3)
            .flatten()
            .copied()
            .collect();
        self.cache.retain(|page, _| keep.contains(page));
    }

    fn prefetch_next(&mut self) {
        let next = self.view + 1;
        if self.prefetched_view == Some(next) {
            return;
        }
        let Some(view) = self.views().get(next).cloned() else {
            return;
        };
        self.prefetched_view = Some(next);
        let pages: Vec<(usize, String)> = view
            .into_iter()
            .filter(|page| !self.cache.contains_key(page))
            .map(|page| (page, self.pages[page].clone()))
            .collect();
        if !pages.is_empty() {
            self.requests.push(self.job(pages, None));
        }
    }

    fn job(&self, pages: Vec<(usize, String)>, target: Option<ViewKey>) -> RenderJob {
        let cached = pages
            .iter()
            .filter_map(|(page, _)| Some((*page, self.cache.get(page)?.clone())))
            .collect();
        RenderJob {
            reader_id: self.id,
            chapter: self.local_chapter().clone(),
            pages,
            cached,
            target,
            picker: self.picker.clone(),
        }
    }

    fn view_key(&self, area: Rect) -> Option<ViewKey> {
        let pages = self.views().get(self.view)?.clone();
        // Direction and scroll only change how spreads and fit-width look; leaving them out
        // otherwise avoids pointless re-renders.
        let direction = if pages.len() == 2 {
            self.direction
        } else {
            Direction::default()
        };
        let scroll = if self.fit == Fit::Width {
            self.scroll
        } else {
            0
        };
        Some(ViewKey {
            pages,
            size: (area.width, area.height),
            fit: self.fit,
            scroll,
            direction,
        })
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let [image_area, status] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        self.render_page(frame, image_area);
        self.render_status(frame, status);
        if self.end_prompt {
            self.render_end_prompt(frame, image_area);
        }
    }

    fn render_page(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(err) = &self.error {
            let message = Paragraph::new(format!("Couldn't open this chapter: {err}").red())
                .wrap(Wrap { trim: true })
                .centered();
            frame.render_widget(message, centered(area, area.width, 2));
            return;
        }
        let Some(key) = self.view_key(area) else {
            return;
        };
        if let Some((failed_key, err)) = &self.failed
            && *failed_key == key
        {
            let message = Paragraph::new(format!("Couldn't show this page: {err}").red())
                .wrap(Wrap { trim: true })
                .centered();
            frame.render_widget(message, centered(area, area.width, 2));
            return;
        }

        let up_to_date = self.shown.as_ref().is_some_and(|(shown, _)| *shown == key);
        match &self.shown {
            // While the new view renders, keep the previous one up if it still fits.
            Some((_, protocol)) if up_to_date || fits(protocol.size(), area) => {
                let size = protocol.size();
                frame.render_widget(
                    Image::new(protocol),
                    centered(area, size.width, size.height),
                );
            }
            _ => {
                let loading = Paragraph::new("Loading page…".dark_gray()).centered();
                frame.render_widget(loading, centered(area, area.width, 1));
            }
        }
        if !up_to_date && self.pending.as_ref() != Some(&key) {
            let pages = key
                .pages
                .iter()
                .map(|&page| (page, self.pages[page].clone()))
                .collect();
            self.requests.push(self.job(pages, Some(key.clone())));
            self.pending = Some(key);
            self.failed = None;
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let views = self.views();
        let position = match views.get(self.view).map(Vec::as_slice) {
            Some([page]) => format!("{}/{}", page + 1, self.pages.len()),
            Some([first, second]) => format!("{}–{}/{}", first + 1, second + 1, self.pages.len()),
            _ => "–".to_owned(),
        };
        let layout = match self.layout {
            PageLayout::Single => "Single",
            PageLayout::Spread => "Spread",
        };
        let fit = match self.fit {
            Fit::Page => "Fit page",
            Fit::Width => "Fit width",
        };
        let info = Line::from(vec![
            Span::raw(format!(" {} ", self.manga.title)).bold(),
            Span::raw(format!("· {} · ", self.local_chapter().name)),
            Span::raw(position).cyan(),
            Span::raw(format!(" · {} · {layout} · {fit} ", self.direction.label())).dark_gray(),
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
        let next = self.manga.chapters.get(self.chapter + 1);
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

fn fits(size: Size, area: Rect) -> bool {
    size.width <= area.width && size.height <= area.height
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ChapterKind;
    use crate::library::state::Progress;
    use crossterm::event::KeyModifiers;
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

    fn progress(action: Option<Action>) -> (usize, bool) {
        match action {
            Some(Action::SaveProgress { page, finished, .. }) => (page, finished),
            other => panic!("expected progress, got {other:?}"),
        }
    }

    #[test]
    fn spreads_put_the_cover_alone() {
        assert_eq!(
            page_views(5, PageLayout::Spread),
            [vec![0], vec![1, 2], vec![3, 4]]
        );
        assert_eq!(
            page_views(4, PageLayout::Spread),
            [vec![0], vec![1, 2], vec![3]]
        );
        assert_eq!(page_views(2, PageLayout::Single), [vec![0], vec![1]]);
    }

    #[test]
    fn right_to_left_spreads_put_the_later_page_on_the_left() {
        let earlier = Arc::new(DynamicImage::new_rgb8(10, 20));
        let mut later = DynamicImage::new_rgb8(5, 10);
        later.as_mut_rgb8().unwrap().fill(255);
        let later = Arc::new(later);

        let spread = compose(&[earlier.clone(), later.clone()], Direction::Rtl).to_rgba8();
        assert_eq!(
            spread.dimensions(),
            (20, 20),
            "the shorter page is scaled up"
        );
        assert_eq!(spread.get_pixel(0, 0).0[0], 255);
        assert_eq!(spread.get_pixel(19, 0).0[0], 0);

        let spread = compose(&[earlier, later], Direction::Ltr).to_rgba8();
        assert_eq!(spread.get_pixel(0, 0).0[0], 0);
    }

    #[test]
    fn fit_width_crops_the_visible_part() {
        let font = FontSize::new(10, 20);
        let page = DynamicImage::new_rgb8(100, 400);
        // 10 cells wide = 100 px, so the page keeps its size; 5 rows = 100 px visible.
        let (crop, max_scroll) = crop_to_width(page.clone(), Size::new(10, 5), font, 0);
        assert_eq!((crop.width(), crop.height(), max_scroll), (100, 100, 15));
        let (crop, _) = crop_to_width(page, Size::new(10, 5), font, 99);
        assert_eq!(crop.height(), 100, "scroll is clamped to the bottom");
    }

    #[test]
    fn resumes_and_marks_read_on_the_last_page() {
        let dir = tempfile::tempdir().unwrap();
        let manga = manga(dir.path());
        let mut state = state(dir.path());
        state.record("Test", "A", 3, false);

        let (mut reader, action) = ReaderScreen::new(
            manga,
            0,
            &state,
            Picker::halfblocks(),
            PageLayout::Single,
            Fit::Page,
        );
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
        let (_, action) = ReaderScreen::new(
            manga(dir.path()),
            0,
            &state,
            Picker::halfblocks(),
            PageLayout::Single,
            Fit::Page,
        );
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
        let (mut reader, _) = ReaderScreen::new(
            manga(dir.path()),
            0,
            &state(dir.path()),
            Picker::halfblocks(),
            PageLayout::Single,
            Fit::Page,
        );
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
        assert!(crate::ui::buffer_text(&terminal).contains("Loading page…"));
        assert!(crate::ui::buffer_text(&terminal).contains("2–3/5 · RTL · Spread · Fit page"));

        let jobs = reader.take_requests();
        assert_eq!(jobs.len(), 1);
        reader.on_rendered(render_job(jobs.into_iter().next().unwrap()));
        assert!(reader.shown.is_some());
        let prefetch = reader.take_requests();
        assert_eq!(
            prefetch.len(),
            1,
            "the next spread is decoded ahead of time"
        );
        assert!(prefetch[0].target.is_none());
    }
}
