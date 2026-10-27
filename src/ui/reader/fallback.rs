//! Showing pages through `ratatui-image`, for terminals without the kitty protocol (sixel,
//! iTerm2, half-blocks) and inside tmux. Each view is turned into one terminal image on a
//! worker thread; the next view's pages are decoded ahead of time.

use std::collections::HashMap;
use std::sync::Arc;

use image::imageops::{self, FilterType};
use image::{DynamicImage, GenericImageView, RgbImage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect, Size};
use ratatui::style::Stylize;
use ratatui::widgets::{Paragraph, Wrap};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FontSize, Image, Resize};

use super::{Fit, ViewState};
use crate::library::prepare::resize_rgb;
use crate::library::state::Direction;
use crate::library::{LocalChapter, pages};

/// Everything that decides what the image on screen looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewKey {
    pages: Vec<usize>,
    size: (u16, u16),
    fit: Fit,
    scroll: u16,
    direction: Direction,
}

/// Decodes pages and, unless only prefetching, turns them into the terminal image for a view.
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
    pub(super) reader_id: u64,
    decoded: Vec<(usize, Arc<DynamicImage>)>,
    view: Option<(ViewKey, Encoded)>,
}

impl RenderJob {
    pub fn run(self) -> Rendered {
        let mut decoded = Vec::new();
        let mut images = Vec::new();
        let mut failure = None;
        for (index, name) in &self.pages {
            match self.cached.get(index) {
                Some(image) => images.push(image.clone()),
                None => match pages::load(&self.chapter, name) {
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
        let view = self.target.map(|key| {
            let result = match failure {
                Some(err) => Err(err),
                None => encode(&self.picker, &images, &key),
            };
            (key, result)
        });
        Rendered {
            reader_id: self.reader_id,
            decoded,
            view,
        }
    }
}

fn encode(picker: &Picker, images: &[Arc<DynamicImage>], key: &ViewKey) -> Encoded {
    let font = picker.font_size();
    let area = Size::new(key.size.0, key.size.1);
    let view = (
        f64::from(area.width) * f64::from(font.width),
        f64::from(area.height) * f64::from(font.height),
    );
    let natives: Vec<(u32, u32)> = images.iter().map(|image| image.dimensions()).collect();
    // Resize each page once, straight to its final share of the view, with the fast resizer.
    let pages: Vec<RgbImage> = images
        .iter()
        .zip(spread_sizes(&natives, view, key.fit))
        .map(|(image, size)| resize_rgb(image.to_rgb8(), size))
        .collect();
    let image = side_by_side(pages, key.direction);
    let (image, max_scroll) = match key.fit {
        Fit::Page => (image, 0),
        Fit::Width => crop_to_width(image, area, font, key.scroll),
    };
    let protocol = picker
        .new_protocol(image, area, Resize::Scale(Some(FilterType::Triangle)))
        .map_err(|err| err.to_string())?;
    Ok((protocol, max_scroll))
}

/// Pixel sizes for a view's pages: one shared height (that of the tallest page, so neither is
/// shrunk and then enlarged again), scaled down as a whole to fit the view's width, and its
/// height too in fit-page mode. Never scaled up; the terminal image step does that.
fn spread_sizes(natives: &[(u32, u32)], view: (f64, f64), fit: Fit) -> Vec<(u32, u32)> {
    let height = natives.iter().map(|size| size.1.max(1)).max().unwrap_or(1);
    let widths: Vec<f64> = natives
        .iter()
        .map(|&(width, page_height)| {
            f64::from(width.max(1)) * f64::from(height) / f64::from(page_height.max(1))
        })
        .collect();
    let total: f64 = widths.iter().sum();
    let mut scale = (view.0 / total).min(1.0);
    if fit == Fit::Page {
        scale = scale.min(view.1 / f64::from(height));
    }
    let shared_height = ((f64::from(height) * scale).round() as u32).max(1);
    widths
        .iter()
        .map(|width| (((width * scale).round() as u32).max(1), shared_height))
        .collect()
}

/// Pages of the same height side by side. Right-to-left puts the later page on the left, like
/// an open manga volume.
fn side_by_side(mut pages: Vec<RgbImage>, direction: Direction) -> DynamicImage {
    if pages.len() == 1 {
        return DynamicImage::ImageRgb8(pages.remove(0));
    }
    if direction == Direction::Rtl {
        pages.reverse();
    }
    let width = pages.iter().map(RgbImage::width).sum();
    let height = pages.iter().map(RgbImage::height).max().unwrap_or(1);
    let mut canvas = RgbImage::new(width, height);
    let mut x = 0;
    for page in &pages {
        imageops::replace(&mut canvas, page, x, 0);
        x += i64::from(page.width());
    }
    DynamicImage::ImageRgb8(canvas)
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

pub struct FallbackView {
    picker: Picker,
    cache: HashMap<usize, Arc<DynamicImage>>,
    shown: Option<(ViewKey, Protocol)>,
    failed: Option<(ViewKey, String)>,
    pending: Option<ViewKey>,
    prefetched_view: Option<usize>,
    requests: Vec<RenderJob>,
    max_scroll: u16,
}

impl FallbackView {
    pub fn new(picker: Picker) -> Self {
        FallbackView {
            picker,
            cache: HashMap::new(),
            shown: None,
            failed: None,
            pending: None,
            prefetched_view: None,
            requests: Vec::new(),
            max_scroll: 0,
        }
    }

    pub fn max_scroll(&self) -> u16 {
        self.max_scroll
    }

    pub fn take_requests(&mut self) -> Vec<RenderJob> {
        std::mem::take(&mut self.requests)
    }

    /// Forgets the prefetched view, e.g. after switching between single pages and spreads.
    pub fn reset_prefetch(&mut self) {
        self.prefetched_view = None;
    }

    pub fn on_rendered(&mut self, state: &ViewState, rendered: Rendered) {
        self.cache.extend(rendered.decoded);
        self.trim_cache(state);
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
                self.shown = Some((key, protocol));
                self.prefetch_next(state);
            }
            Err(err) => self.failed = Some((key, err)),
        }
    }

    /// Drops decoded pages that aren't in the previous, current or next view.
    pub fn trim_cache(&mut self, state: &ViewState) {
        let keep: Vec<usize> = state
            .views
            .iter()
            .skip(state.view.saturating_sub(1))
            .take(3)
            .flatten()
            .copied()
            .collect();
        self.cache.retain(|page, _| keep.contains(page));
    }

    fn prefetch_next(&mut self, state: &ViewState) {
        let next = state.view + 1;
        if self.prefetched_view == Some(next) {
            return;
        }
        let Some(view) = state.views.get(next) else {
            return;
        };
        self.prefetched_view = Some(next);
        let pages: Vec<(usize, String)> = view
            .iter()
            .filter(|page| !self.cache.contains_key(page))
            .map(|&page| (page, state.pages[page].clone()))
            .collect();
        if !pages.is_empty() {
            let job = self.job(state, pages, None);
            self.requests.push(job);
        }
    }

    fn job(
        &self,
        state: &ViewState,
        pages: Vec<(usize, String)>,
        target: Option<ViewKey>,
    ) -> RenderJob {
        let cached = pages
            .iter()
            .filter_map(|(page, _)| Some((*page, self.cache.get(page)?.clone())))
            .collect();
        RenderJob {
            reader_id: state.reader_id,
            chapter: state.chapter.clone(),
            pages,
            cached,
            target,
            picker: self.picker.clone(),
        }
    }

    fn view_key(state: &ViewState, area: Rect) -> ViewKey {
        let pages = state.views[state.view].clone();
        // Direction and scroll only change how spreads and fit-width look; leaving them out
        // otherwise avoids pointless re-renders.
        let direction = if pages.len() == 2 {
            state.direction
        } else {
            Direction::default()
        };
        let scroll = if state.fit == Fit::Width {
            state.scroll
        } else {
            0
        };
        ViewKey {
            pages,
            size: (area.width, area.height),
            fit: state.fit,
            scroll,
            direction,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, state: &ViewState) {
        let key = Self::view_key(state, area);
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
                .map(|&page| (page, state.pages[page].clone()))
                .collect();
            let job = self.job(state, pages, Some(key.clone()));
            self.requests.push(job);
            self.pending = Some(key);
            self.failed = None;
        }
    }
}

fn fits(size: Size, area: Rect) -> bool {
    size.width <= area.width && size.height <= area.height
}

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
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

    #[test]
    fn spread_pages_share_the_taller_height() {
        // A 10x20 page and a 5x10 page in a roomy view: the smaller one is matched to 20 px
        // tall rather than both being shrunk.
        let sizes = spread_sizes(&[(10, 20), (5, 10)], (1000.0, 1000.0), Fit::Page);
        assert_eq!(sizes, [(10, 20), (10, 20)]);
        // Too wide for the view: both shrink together.
        let sizes = spread_sizes(&[(800, 1200), (800, 1200)], (800.0, 2000.0), Fit::Width);
        assert_eq!(sizes, [(400, 600), (400, 600)]);
    }

    #[test]
    fn right_to_left_spreads_put_the_later_page_on_the_left() {
        let earlier = RgbImage::new(10, 20);
        let mut later = RgbImage::new(10, 20);
        later.fill(255);

        let spread = side_by_side(vec![earlier.clone(), later.clone()], Direction::Rtl).to_rgb8();
        assert_eq!(spread.dimensions(), (20, 20));
        assert_eq!(spread.get_pixel(0, 0).0[0], 255);
        assert_eq!(spread.get_pixel(19, 0).0[0], 0);

        let spread = side_by_side(vec![earlier, later], Direction::Ltr).to_rgb8();
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
}
