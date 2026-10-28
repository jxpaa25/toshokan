//! Long strips through `ratatui-image`, for terminals without the kitty protocol and inside
//! tmux: the part of the strip on screen is put together from the pages it shows and turned
//! into one terminal image on a worker thread, again for every scroll position. Pages just
//! below the screen are decoded ahead of time.

use std::collections::HashMap;
use std::sync::Arc;

use image::imageops;
use image::{DynamicImage, RgbImage};
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use super::ViewState;
use super::layout::{StripLayout, StripSlice};
use crate::library::prepare::resize_rgb;
use crate::library::{LocalChapter, pages};

/// Everything that decides what the image on screen looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StripKey {
    cols: u16,
    height: u16,
    /// The part of each page shown, with the page's rows.
    slices: Vec<(StripSlice, u32)>,
}

/// Decodes pages and, unless only reading ahead, puts the screen's image together.
pub struct StripRenderJob {
    reader_id: u64,
    chapter: LocalChapter,
    pages: Vec<(usize, String)>,
    cached: HashMap<usize, Arc<RgbImage>>,
    target: Option<StripKey>,
    picker: Picker,
}

pub struct StripRendered {
    pub(super) reader_id: u64,
    decoded: Vec<(usize, Arc<RgbImage>)>,
    failed: Vec<(usize, String)>,
    view: Option<(StripKey, Result<Protocol, String>)>,
}

impl StripRenderJob {
    pub fn run(self) -> StripRendered {
        let mut images = self.cached;
        let mut decoded = Vec::new();
        let mut failed = Vec::new();
        for (page, name) in &self.pages {
            match pages::load(&self.chapter, name) {
                Ok(image) => {
                    let image = Arc::new(image.into_rgb8());
                    images.insert(*page, image.clone());
                    decoded.push((*page, image));
                }
                Err(err) => failed.push((*page, err)),
            }
        }
        let view = self.target.map(|key| {
            let result = compose(&self.picker, &images, &key);
            (key, result)
        });
        StripRendered {
            reader_id: self.reader_id,
            decoded,
            failed,
            view,
        }
    }
}

/// The screen's part of the strip as one image: each slice of a page scaled to the strip's
/// width, one under the other. Pages that couldn't be read stay blank.
fn compose(
    picker: &Picker,
    images: &HashMap<usize, Arc<RgbImage>>,
    key: &StripKey,
) -> Result<Protocol, String> {
    let font = picker.font_size();
    let (cell_width, cell_height) = (u32::from(font.width), u32::from(font.height));
    let width = u32::from(key.cols) * cell_width;
    let mut canvas = RgbImage::new(width, u32::from(key.height) * cell_height);
    let top = key.slices.first().map_or(0, |(slice, _)| slice.y);
    for (slice, page_rows) in &key.slices {
        let Some(image) = images.get(&slice.page) else {
            continue;
        };
        // The rows of the page this slice shows, in the page's own pixels.
        let scale = f64::from(image.height()) / f64::from(*page_rows);
        let from = ((f64::from(slice.row) * scale) as u32).min(image.height() - 1);
        let to = ((f64::from(slice.row + u32::from(slice.rows)) * scale).ceil() as u32)
            .clamp(from + 1, image.height());
        let part = imageops::crop_imm(image.as_ref(), 0, from, image.width(), to - from).to_image();
        let part = resize_rgb(part, (width, u32::from(slice.rows) * cell_height));
        let y = u32::from(slice.y - top) * cell_height;
        imageops::replace(&mut canvas, &part, 0, i64::from(y));
    }
    let size = Size::new(key.cols, key.height);
    picker
        .new_protocol(DynamicImage::ImageRgb8(canvas), size, Resize::Fit(None))
        .map_err(|err| err.to_string())
}

pub struct FallbackStrip {
    picker: Picker,
    /// Decoded pages near the screen.
    cache: HashMap<usize, Arc<RgbImage>>,
    failed: HashMap<usize, String>,
    shown: Option<(StripKey, Protocol)>,
    pending: Option<StripKey>,
    /// The pages being read ahead, so they're asked for once.
    reading_ahead: Vec<usize>,
    requests: Vec<StripRenderJob>,
}

impl FallbackStrip {
    pub fn new(picker: Picker) -> Self {
        FallbackStrip {
            picker,
            cache: HashMap::new(),
            failed: HashMap::new(),
            shown: None,
            pending: None,
            reading_ahead: Vec::new(),
            requests: Vec::new(),
        }
    }

    /// The cell size images are made for.
    pub fn cell(&self) -> (u16, u16) {
        let font = self.picker.font_size();
        (font.width, font.height)
    }

    pub fn take_requests(&mut self) -> Vec<StripRenderJob> {
        std::mem::take(&mut self.requests)
    }

    pub fn on_rendered(
        &mut self,
        state: &ViewState,
        strip: &StripLayout,
        scroll: u32,
        rendered: StripRendered,
    ) {
        self.cache.extend(rendered.decoded);
        self.failed.extend(rendered.failed);
        let keep = strip.pages_between(scroll, scroll + 2 * u32::from(strip.height.max(1)));
        self.cache.retain(|page, _| keep.contains(page));
        self.reading_ahead.retain(|page| keep.contains(page));
        if let Some((key, result)) = rendered.view
            && self.pending.as_ref() == Some(&key)
        {
            self.pending = None;
            match result {
                Ok(protocol) => self.shown = Some((key, protocol)),
                Err(err) => {
                    for (slice, _) in &key.slices {
                        self.failed.entry(slice.page).or_insert_with(|| err.clone());
                    }
                }
            }
        }
        self.read_ahead(state, strip, scroll);
    }

    /// Decodes the pages of the next screen down.
    fn read_ahead(&mut self, state: &ViewState, strip: &StripLayout, scroll: u32) {
        let height = u32::from(strip.height.max(1));
        let pages: Vec<(usize, String)> = strip
            .pages_between(scroll + height, scroll + 2 * height)
            .filter(|page| {
                !self.cache.contains_key(page)
                    && !self.failed.contains_key(page)
                    && !self.reading_ahead.contains(page)
            })
            .map(|page| (page, state.pages[page].clone()))
            .collect();
        if pages.is_empty() {
            return;
        }
        self.reading_ahead
            .extend(pages.iter().map(|(page, _)| page));
        let job = self.job(state, pages, None);
        self.requests.push(job);
    }

    fn job(
        &self,
        state: &ViewState,
        pages: Vec<(usize, String)>,
        target: Option<StripKey>,
    ) -> StripRenderJob {
        let cached = match &target {
            Some(key) => key
                .slices
                .iter()
                .filter_map(|(slice, _)| Some((slice.page, self.cache.get(&slice.page)?.clone())))
                .collect(),
            None => HashMap::new(),
        };
        StripRenderJob {
            reader_id: state.reader_id,
            chapter: state.chapter.clone(),
            pages,
            cached,
            target,
            picker: self.picker.clone(),
        }
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        state: &ViewState,
        strip: &StripLayout,
        scroll: u32,
    ) {
        let slices = strip.visible(scroll);
        let key = StripKey {
            cols: strip.cols,
            height: strip.height,
            slices: slices.iter().map(|&s| (s, strip.rows(s.page))).collect(),
        };
        let area = Rect::new(strip.x, strip.y, strip.cols, strip.height);
        match &self.shown {
            // While the new position renders, the last one stays up if it's the same size.
            Some((shown, protocol)) if shown.cols == key.cols && shown.height == key.height => {
                frame.render_widget(Image::new(protocol), area);
            }
            _ => {
                let line = Rect::new(strip.x, strip.y + strip.height / 2, strip.cols, 1);
                frame.render_widget(Paragraph::new("Loading page…".dark_gray()).centered(), line);
            }
        }
        for slice in &slices {
            if let Some(err) = self.failed.get(&slice.page) {
                let line = Rect::new(strip.x, slice.y + slice.rows / 2, strip.cols, 1);
                let message = format!("Couldn't show page {}: {err}", slice.page + 1).red();
                frame.render_widget(Paragraph::new(message).centered(), line);
            }
        }
        let up_to_date = self.shown.as_ref().is_some_and(|(shown, _)| *shown == key);
        if !up_to_date && self.pending.as_ref() != Some(&key) {
            let pages = slices
                .iter()
                .filter(|s| !self.cache.contains_key(&s.page) && !self.failed.contains_key(&s.page))
                .map(|s| (s.page, state.pages[s.page].clone()))
                .collect();
            let job = self.job(state, pages, Some(key.clone()));
            self.requests.push(job);
            self.pending = Some(key);
        }
    }
}
