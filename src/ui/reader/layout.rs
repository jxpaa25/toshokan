//! Where pages go: grouping pages into what's shown at once, and fitting them into the reader;
//! or, for long strips, stacking every page at one width and scrolling through them.

use std::ops::RangeInclusive;

use ratatui::layout::Rect;

use super::{Fit, PageLayout};
use crate::library::state::Direction;
use crate::ui::kitty::{MAX_COLUMNS, MAX_ROWS};

/// Pages grouped into what's shown at once.
pub fn page_views(count: usize, layout: PageLayout) -> Vec<Vec<usize>> {
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

/// One page of a view on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRect {
    pub page: usize,
    /// The visible part of the page on screen.
    pub area: Rect,
    /// The whole page in cells; taller than `area` when scrolled in fit-width mode.
    pub cols: u16,
    pub rows: u16,
    /// The page row shown at the top of `area`.
    pub row_offset: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    pub rects: Vec<PageRect>,
    pub max_scroll: u16,
}

/// Fits a view's pages, given in reading order with their pixel sizes, into `area`. Pages of
/// a spread share one height and touch in the middle; right-to-left puts the later one on the
/// left. Pages are as tall as they can be (fit page) or fill the width and scroll (fit width),
/// but never taller or wider than the kitty protocol can address.
pub fn place(
    pages: &[(usize, (u32, u32))],
    direction: Direction,
    fit: Fit,
    area: Rect,
    cell: (u16, u16),
    scroll: u16,
) -> Placement {
    if pages.is_empty() || area.is_empty() {
        return Placement::default();
    }
    let (cell_width, cell_height) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let aspect = |(width, height): (u32, u32)| f64::from(width.max(1)) / f64::from(height.max(1));
    let aspect_sum: f64 = pages.iter().map(|(_, size)| aspect(*size)).sum();

    let mut height = f64::from(area.width) * cell_width / aspect_sum;
    if fit == Fit::Page {
        height = height.min(f64::from(area.height) * cell_height);
    }
    height = height.min(f64::from(MAX_ROWS) * cell_height);
    // Placeholder cells can only address so many columns too; past that, a run that restarts
    // (e.g. after the end-of-chapter popup) can't say where it is. Very wide pages get smaller.
    let widest = pages
        .iter()
        .map(|(_, size)| aspect(*size))
        .fold(0.0, f64::max);
    height = height.min(f64::from(MAX_COLUMNS) * cell_width / widest);
    let rows = ((height / cell_height).round() as u16).clamp(1, MAX_ROWS);
    let mut cols: Vec<u16> = pages
        .iter()
        .map(|(_, size)| {
            ((height * aspect(*size) / cell_width).round() as u16).clamp(1, MAX_COLUMNS)
        })
        .collect();
    // Rounding can overshoot the area by a column.
    while cols.iter().sum::<u16>() > area.width {
        let widest = (0..cols.len()).max_by_key(|&i| cols[i]).expect("not empty");
        if cols[widest] == 1 {
            break;
        }
        cols[widest] -= 1;
    }

    let visible = rows.min(area.height);
    let max_scroll = rows - visible;
    let row_offset = scroll.min(max_scroll);
    let y = area.y + (area.height - visible) / 2;
    let total: u16 = cols.iter().sum();
    let mut x = area.x + area.width.saturating_sub(total) / 2;
    let mut order: Vec<usize> = (0..pages.len()).collect();
    if direction == Direction::Rtl {
        order.reverse();
    }
    let rects = order
        .into_iter()
        .map(|i| {
            let width = cols[i].min(area.right().saturating_sub(x));
            let rect = PageRect {
                page: pages[i].0,
                area: Rect::new(x, y, width, visible),
                cols: cols[i],
                rows,
                row_offset,
            };
            x += width;
            rect
        })
        .collect();
    Placement { rects, max_scroll }
}

/// Pages at least this much taller than wide are pieces of a long strip, not manga pages.
const STRIP_ASPECT: f64 = 1.8;
/// Pages looked at to tell whether a chapter is a long strip.
const STRIP_SAMPLES: usize = 5;
/// Rows a page that can't be read takes up in a strip, for its error message.
pub const BROKEN_ROWS: u32 = 3;
/// Ghostty rejects images wider or taller than this.
const MAX_IMAGE_SIDE: u32 = 10_000;

/// A few pages spread through a chapter of `count` pages, past the cover, to tell what it is.
pub fn strip_samples(count: usize) -> Vec<usize> {
    let first = usize::from(count > 1);
    let mut samples: Vec<usize> = (0..STRIP_SAMPLES)
        .map(|k| first + k * (count - first) / STRIP_SAMPLES)
        .filter(|&page| page < count)
        .collect();
    samples.dedup();
    samples
}

/// Whether pages of these sizes read as a long strip: most of them are much taller than wide.
pub fn looks_like_strip(sizes: &[(u32, u32)]) -> bool {
    let tall = sizes
        .iter()
        .filter(|&&(width, height)| f64::from(height) >= f64::from(width.max(1)) * STRIP_ASPECT)
        .count();
    tall * 2 > sizes.len()
}

/// The width a strip is shown at, in pixels: its pages' usual (median) width.
pub fn strip_width(sizes: &[Option<(u32, u32)>]) -> u32 {
    let mut widths: Vec<u32> = sizes.iter().flatten().map(|size| size.0).collect();
    widths.sort_unstable();
    widths.get(widths.len() / 2).copied().unwrap_or(1).max(1)
}

/// Rows a page of `size` pixels takes up when scaled to `cols` columns.
pub fn page_rows(size: (u32, u32), cols: u16, cell: (u16, u16)) -> u32 {
    let width = f64::from(cols) * f64::from(cell.0.max(1));
    let height = f64::from(size.1.max(1)) * width / f64::from(size.0.max(1));
    ((height / f64::from(cell.1.max(1))).round() as u32).max(1)
}

/// How a strip page of `rows` rows is cut into images, as (first row, rows) pairs:
/// placeholders address at most [`MAX_ROWS`] rows of an image, and Ghostty takes images up to
/// 10000 px tall.
pub fn tiles(rows: u32, cell_height: u16) -> Vec<(u32, u16)> {
    let per_tile = u32::from(MAX_ROWS)
        .min(MAX_IMAGE_SIDE / u32::from(cell_height.max(1)))
        .max(1);
    (0..rows)
        .step_by(per_tile as usize)
        .map(|start| (start, (rows - start).min(per_tile) as u16))
        .collect()
}

/// A long strip: every page scaled to the same width, one under the other, seen through the
/// screen `height` rows at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripLayout {
    /// The strip's columns on screen, centred.
    pub x: u16,
    pub cols: u16,
    /// The screen rows the strip is seen through.
    pub y: u16,
    pub height: u16,
    /// The strip row each page starts at, then the strip's length.
    starts: Vec<u32>,
}

/// The part of a page on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripSlice {
    pub page: usize,
    /// The first row of the page shown, and how many are.
    pub row: u32,
    pub rows: u16,
    /// The screen row it's at.
    pub y: u16,
}

impl StripLayout {
    /// Lays out pages of `sizes` (`None` for one that can't be read) in `area`. The strip is
    /// `width` pixels wide ([`strip_width`]) when that fits, never scaled up (fit page), or
    /// as wide as the area (fit width).
    pub fn new(
        sizes: &[Option<(u32, u32)>],
        width: u32,
        fit: Fit,
        area: Rect,
        cell: (u16, u16),
    ) -> Self {
        let cell_width = u32::from(cell.0.max(1));
        let widest = u16::try_from(MAX_IMAGE_SIDE / cell_width).unwrap_or(u16::MAX);
        let full = area.width.min(MAX_COLUMNS).min(widest).max(1);
        let cols = match fit {
            Fit::Width => full,
            Fit::Page => u16::try_from(width / cell_width)
                .unwrap_or(u16::MAX)
                .clamp(1, full),
        };
        let mut starts = Vec::with_capacity(sizes.len() + 1);
        let mut row = 0;
        for size in sizes {
            starts.push(row);
            row += size.map_or(BROKEN_ROWS, |size| page_rows(size, cols, cell));
        }
        starts.push(row);
        StripLayout {
            x: area.x + area.width.saturating_sub(cols) / 2,
            cols,
            y: area.y,
            height: area.height,
            starts,
        }
    }

    /// The strip's length in rows.
    pub fn total_rows(&self) -> u32 {
        self.starts[self.starts.len() - 1]
    }

    pub fn start(&self, page: usize) -> u32 {
        self.starts[page]
    }

    pub fn rows(&self, page: usize) -> u32 {
        self.starts[page + 1] - self.starts[page]
    }

    /// How far the strip scrolls before its end reaches the bottom of the screen.
    pub fn max_scroll(&self) -> u32 {
        self.total_rows().saturating_sub(u32::from(self.height))
    }

    /// The page at strip row `row` (the last one past the end), and the row within it.
    pub fn locate(&self, row: u32) -> (usize, u32) {
        let pages = self.starts.len() - 1;
        if pages == 0 {
            return (0, 0);
        }
        let page = self.starts[1..]
            .partition_point(|&end| end <= row)
            .min(pages - 1);
        (page, row - self.starts[page].min(row))
    }

    /// The pages with rows in `from..to`.
    pub fn pages_between(&self, from: u32, to: u32) -> RangeInclusive<usize> {
        if self.starts.len() == 1 {
            #[allow(clippy::reversed_empty_ranges)]
            return 1..=0;
        }
        let to = to.min(self.total_rows()).max(from + 1);
        self.locate(from).0..=self.locate(to - 1).0
    }

    /// What's on screen with the strip scrolled `scroll` rows down: part of each page, top
    /// to bottom.
    pub fn visible(&self, scroll: u32) -> Vec<StripSlice> {
        let bottom = (scroll + u32::from(self.height)).min(self.total_rows());
        let mut slices = Vec::new();
        let mut row = scroll;
        while row < bottom {
            let (page, within) = self.locate(row);
            let rows = (self.rows(page) - within).min(bottom - row);
            slices.push(StripSlice {
                page,
                row: within,
                rows: rows as u16,
                y: self.y + (row - scroll) as u16,
            });
            row += rows;
        }
        slices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (u16, u16) = (10, 20);

    #[test]
    fn tells_long_strips_from_manga() {
        assert_eq!(strip_samples(0), Vec::<usize>::new());
        assert_eq!(strip_samples(1), [0]);
        assert_eq!(strip_samples(3), [1, 2]);
        assert_eq!(strip_samples(41), [1, 9, 17, 25, 33]);
        assert!(looks_like_strip(&[(800, 1200), (720, 3000), (720, 1600)]));
        assert!(!looks_like_strip(&[(800, 1200), (720, 3000), (1600, 1200)]));
        assert!(!looks_like_strip(&[]));
        assert_eq!(
            strip_width(&[Some((700, 1)), None, Some((800, 1)), Some((800, 1))]),
            800
        );
        assert_eq!(strip_width(&[None]), 1);
    }

    #[test]
    fn stacks_strip_pages_at_one_width() {
        let sizes = [Some((800, 2000)), Some((400, 500)), None];
        let area = Rect::new(0, 1, 200, 50);
        let strip = StripLayout::new(&sizes, 800, Fit::Page, area, CELL);
        // 800 px = 80 columns; 2000 px = 100 rows; the narrower page is scaled to match.
        assert_eq!((strip.x, strip.cols), (60, 80));
        assert_eq!(
            (strip.rows(0), strip.rows(1), strip.rows(2)),
            (100, 50, BROKEN_ROWS)
        );
        assert_eq!((strip.total_rows(), strip.max_scroll()), (153, 103));
        assert_eq!(
            strip.visible(90),
            [
                StripSlice {
                    page: 0,
                    row: 90,
                    rows: 10,
                    y: 1
                },
                StripSlice {
                    page: 1,
                    row: 0,
                    rows: 40,
                    y: 11
                },
            ]
        );
        assert_eq!(strip.locate(150), (2, 0));
        assert_eq!(strip.locate(999), (2, 849));
        assert_eq!(strip.pages_between(0, 101), 0..=1);

        let full = StripLayout::new(&sizes, 800, Fit::Width, area, CELL);
        assert_eq!((full.x, full.cols, full.rows(0)), (0, 200, 250));
        let narrow = StripLayout::new(&sizes, 800, Fit::Page, Rect::new(0, 0, 50, 20), CELL);
        assert_eq!(narrow.cols, 50, "never wider than the screen");
        assert!(
            StripLayout::new(&[], 800, Fit::Page, area, CELL)
                .pages_between(0, 10)
                .is_empty()
        );
        let short = StripLayout::new(&sizes[1..2], 800, Fit::Page, area, CELL);
        assert_eq!(short.visible(0).len(), 1);
        assert_eq!(short.max_scroll(), 0);
    }

    #[test]
    fn cuts_tall_strip_pages_into_tiles() {
        assert_eq!(tiles(700, 20), [(0, 297), (297, 297), (594, 106)]);
        assert_eq!(
            tiles(500, 40),
            [(0, 250), (250, 250)],
            "under 10000 px each"
        );
        assert_eq!(tiles(1, 20), [(0, 1)]);
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
    fn fits_a_page_and_centres_it() {
        let area = Rect::new(0, 0, 190, 50);
        let placement = place(
            &[(0, (848, 1216))],
            Direction::Rtl,
            Fit::Page,
            area,
            CELL,
            0,
        );
        // 1000 px tall -> 697 px wide -> 70 columns, centred.
        assert_eq!(
            placement.rects,
            [PageRect {
                page: 0,
                area: Rect::new(60, 0, 70, 50),
                cols: 70,
                rows: 50,
                row_offset: 0
            }]
        );
        assert_eq!(placement.max_scroll, 0);
    }

    #[test]
    fn spreads_touch_in_the_middle_in_reading_order() {
        let area = Rect::new(0, 0, 190, 50);
        let pages = [(1, (800, 1200)), (2, (800, 1200))];
        let rtl = place(&pages, Direction::Rtl, Fit::Page, area, CELL, 0);
        let pages_on_screen: Vec<_> = rtl.rects.iter().map(|r| (r.page, r.area.x)).collect();
        assert_eq!(pages_on_screen, [(2, 28), (1, 95)]);
        assert_eq!(rtl.rects[0].area.right(), rtl.rects[1].area.x);

        let ltr = place(&pages, Direction::Ltr, Fit::Page, area, CELL, 0);
        assert_eq!(ltr.rects[0].page, 1);
    }

    #[test]
    fn fit_width_scrolls_within_the_page() {
        let area = Rect::new(0, 0, 100, 40);
        let placement = place(
            &[(0, (1000, 3000))],
            Direction::Rtl,
            Fit::Width,
            area,
            CELL,
            500,
        );
        // 1000 px wide -> 3000 px = 150 rows tall; 40 visible.
        let rect = placement.rects[0];
        assert_eq!((rect.cols, rect.rows, rect.area.height), (100, 150, 40));
        assert_eq!((placement.max_scroll, rect.row_offset), (110, 110));
    }

    #[test]
    fn very_wide_pages_are_limited_to_the_addressable_columns() {
        let area = Rect::new(0, 0, 640, 60);
        let placement = place(
            &[(0, (6000, 1000))],
            Direction::Rtl,
            Fit::Page,
            area,
            CELL,
            0,
        );
        let rect = placement.rects[0];
        assert_eq!(rect.cols, MAX_COLUMNS);
        assert!(rect.rows < 60, "the page gets shorter instead");
    }

    #[test]
    fn very_tall_pages_are_limited_to_the_addressable_rows() {
        let area = Rect::new(0, 0, 100, 40);
        let placement = place(
            &[(0, (800, 20000))],
            Direction::Rtl,
            Fit::Width,
            area,
            CELL,
            0,
        );
        let rect = placement.rects[0];
        assert_eq!(rect.rows, MAX_ROWS);
        assert!(rect.cols < 100, "the page gets narrower instead");
    }
}
