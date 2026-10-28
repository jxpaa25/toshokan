//! Long strips through the kitty protocol. Each page is scaled to fill its cells exactly, so
//! pages meet without a seam, and cut into images of at most a few hundred rows ([`tiles`]).
//! Pages from a screen above to two screens below are kept in the terminal, so scrolling only
//! redraws placeholder cells.

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;

use super::ViewState;
use super::kitty_view::{Payload, terminal_cell};
use super::layout::{StripLayout, StripSlice, tiles};
use crate::library::prepare::fill_cells;
use crate::library::{LocalChapter, pages};
use crate::ui::kitty::{self, Medium, Placeholders};

/// Pages prepared at once, as for single pages.
const MAX_IN_FLIGHT: usize = 3;

/// Decodes a page, scales it to its cells in the strip and cuts it into tiles, writing each to
/// a file for [`Medium::TempFile`]. Blocking; run it on a worker thread.
pub struct StripJob {
    reader_id: u64,
    page: usize,
    name: String,
    chapter: LocalChapter,
    cols: u16,
    rows: u32,
    cell: (u16, u16),
    medium: Medium,
}

pub struct StripPrepared {
    pub(super) reader_id: u64,
    pub(super) page: usize,
    /// The columns and rows it was prepared for; useless at any other size.
    cells: (u16, u32),
    result: Result<Vec<PreparedTile>, String>,
}

struct PreparedTile {
    rows: u16,
    size: (u32, u32),
    payload: Payload,
}

impl StripJob {
    pub fn run(self) -> StripPrepared {
        let (cell_width, cell_height) = (u32::from(self.cell.0), u32::from(self.cell.1));
        let result = pages::load(&self.chapter, &self.name).map(|image| {
            let size = (u32::from(self.cols) * cell_width, self.rows * cell_height);
            let page = fill_cells(image, size);
            let row_height = f64::from(page.height) / f64::from(self.rows);
            let line = page.width as usize * 3;
            let height = page.height as usize;
            tiles(self.rows, self.cell.1)
                .into_iter()
                .map(|(start, rows)| {
                    // Rounded outwards, like the page itself: a tile is never shorter than its
                    // cells, so tiles meet without a gap.
                    let edge = |row: u32| f64::from(row) * row_height;
                    let top = (edge(start).floor() as usize).min(height - 1);
                    let bottom =
                        (edge(start + u32::from(rows)).ceil() as usize).clamp(top + 1, height);
                    let rgb = page.rgb[top * line..bottom * line].to_vec();
                    PreparedTile {
                        rows,
                        size: (page.width, (bottom - top) as u32),
                        payload: Payload::new(self.medium.temp_file(), rgb),
                    }
                })
                .collect()
        });
        StripPrepared {
            reader_id: self.reader_id,
            page: self.page,
            cells: (self.cols, self.rows),
            result,
        }
    }
}

/// A page held by the terminal: one image per tile.
struct SentPage {
    /// The columns and rows it fills.
    cells: (u16, u32),
    /// Each tile's image id and rows, top to bottom.
    tiles: Vec<(u32, u16)>,
}

pub struct StripView {
    medium: Medium,
    /// Pixels per cell, to scale pages to the strip's width.
    cell: (u16, u16),
    area: Rect,
    sent: HashMap<usize, SentPage>,
    in_flight: HashSet<usize>,
    failed: HashMap<usize, String>,
    /// Pages to prepare, most urgent first. Rebuilt every frame.
    queue: Vec<usize>,
    output: Vec<u8>,
}

impl StripView {
    pub fn new(medium: Medium, cell: (u16, u16)) -> Self {
        StripView {
            medium,
            cell,
            area: Rect::default(),
            sent: HashMap::new(),
            in_flight: HashSet::new(),
            failed: HashMap::new(),
            queue: Vec::new(),
            output: Vec::new(),
        }
    }

    /// The cell size to lay out the strip in `area` with, checked again when the area changes.
    pub fn cell(&mut self, area: Rect) -> (u16, u16) {
        if area != self.area {
            self.area = area;
            if let Some(cell) = terminal_cell() {
                self.cell = cell;
            }
        }
        self.cell
    }

    /// Works out what the terminal needs for the strip scrolled to `scroll`: drops pages that
    /// are far off screen and queues the ones near it, those on screen first, then below, then
    /// above.
    pub fn prepare_frame(&mut self, strip: &StripLayout, scroll: u32) {
        let near = near(strip, scroll);
        let gone: Vec<usize> = self
            .sent
            .keys()
            .filter(|page| !near.contains(page))
            .copied()
            .collect();
        for page in gone {
            for (id, _) in self.sent.remove(&page).expect("listed above").tiles {
                kitty::delete(&mut self.output, id);
            }
        }
        let on_screen: Vec<usize> = strip.visible(scroll).iter().map(|s| s.page).collect();
        let below = near
            .iter()
            .filter(|&&page| on_screen.last().is_some_and(|&p| page > p));
        let above = near
            .iter()
            .filter(|&&page| on_screen.first().is_some_and(|&p| page < p));
        self.queue = on_screen
            .iter()
            .chain(below)
            .chain(above)
            .copied()
            .filter(|page| {
                !self.in_flight.contains(page)
                    && !self.failed.contains_key(page)
                    && self
                        .sent
                        .get(page)
                        .is_none_or(|sent| !fits(sent.cells, strip, *page))
            })
            .collect();
    }

    /// Hands out queued pages to prepare, up to the in-flight limit.
    pub fn take_requests(&mut self, state: &ViewState, strip: &StripLayout) -> Vec<StripJob> {
        let free = MAX_IN_FLIGHT.saturating_sub(self.in_flight.len());
        let queue = std::mem::take(&mut self.queue);
        queue
            .into_iter()
            .take(free)
            .map(|page| {
                self.in_flight.insert(page);
                StripJob {
                    reader_id: state.reader_id,
                    page,
                    name: state.pages[page].clone(),
                    chapter: state.chapter.clone(),
                    cols: strip.cols,
                    rows: strip.rows(page),
                    cell: self.cell,
                    medium: self.medium.clone(),
                }
            })
            .collect()
    }

    /// Sends a prepared page to the terminal, unless it's no longer near or the strip changed
    /// size since. Images it replaces keep their ids.
    pub fn on_prepared(&mut self, strip: &StripLayout, scroll: u32, prepared: StripPrepared) {
        self.in_flight.remove(&prepared.page);
        let page = prepared.page;
        if !fits(prepared.cells, strip, page) || !near(strip, scroll).contains(&page) {
            // Dropping it removes its files.
            return;
        }
        let tiles = match prepared.result {
            Ok(tiles) => tiles,
            Err(err) => {
                self.failed.insert(page, err);
                return;
            }
        };
        let mut ids: Vec<u32> = self
            .sent
            .remove(&page)
            .map(|sent| sent.tiles.into_iter().map(|(id, _)| id).collect())
            .unwrap_or_default();
        for id in ids.iter().skip(tiles.len()) {
            kitty::delete(&mut self.output, *id);
        }
        ids.resize_with(tiles.len(), kitty::next_id);
        let sent = tiles
            .into_iter()
            .zip(ids)
            .map(|(tile, id)| {
                let placed = (strip.cols, tile.rows);
                tile.payload
                    .transmit(&mut self.output, id, tile.size, placed);
                (id, tile.rows)
            })
            .collect();
        self.sent.insert(
            page,
            SentPage {
                cells: prepared.cells,
                tiles: sent,
            },
        );
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Deletes every page from the terminal; for when the reader closes or leaves strip mode.
    pub fn close(&mut self) -> Vec<u8> {
        for (_, sent) in self.sent.drain() {
            for (id, _) in sent.tiles {
                kitty::delete(&mut self.output, id);
            }
        }
        self.take_output()
    }

    pub fn render(&self, frame: &mut Frame, strip: &StripLayout, scroll: u32) {
        for slice in strip.visible(scroll) {
            match self.sent.get(&slice.page) {
                Some(sent) if fits(sent.cells, strip, slice.page) => {
                    draw_tiles(frame, strip, &slice, sent)
                }
                _ => {
                    let message = match self.failed.get(&slice.page) {
                        Some(err) => format!("Couldn't show page {}: {err}", slice.page + 1).red(),
                        None => "Loading page…".dark_gray(),
                    };
                    let line = Rect::new(strip.x, slice.y + slice.rows / 2, strip.cols, 1);
                    frame.render_widget(Paragraph::new(message).centered(), line);
                }
            }
        }
    }
}

/// Placeholder cells for the part of each of the page's tiles that `slice` shows.
fn draw_tiles(frame: &mut Frame, strip: &StripLayout, slice: &StripSlice, sent: &SentPage) {
    let (from, to) = (slice.row, slice.row + u32::from(slice.rows));
    let mut start = 0;
    for &(id, rows) in &sent.tiles {
        let end = start + u32::from(rows);
        let (top, bottom) = (from.max(start), to.min(end));
        if top < bottom {
            let y = slice.y + (top - from) as u16;
            let area = Rect::new(strip.x, y, strip.cols, (bottom - top) as u16);
            let row_offset = (top - start) as u16;
            frame.render_widget(Placeholders { id, row_offset }, area);
        }
        start = end;
    }
}

/// Whether a page made for `cells` fills its place in the strip as it's laid out now.
fn fits(cells: (u16, u32), strip: &StripLayout, page: usize) -> bool {
    cells == (strip.cols, strip.rows(page))
}

/// The pages kept in the terminal: those with rows from a screen above to two screens below
/// the one shown.
pub(super) fn near(strip: &StripLayout, scroll: u32) -> HashSet<usize> {
    let height = u32::from(strip.height.max(1));
    strip
        .pages_between(scroll.saturating_sub(height), scroll + 3 * height)
        .collect()
}
