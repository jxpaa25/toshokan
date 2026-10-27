//! Where pages go: grouping pages into what's shown at once, and fitting them into the reader.

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

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (u16, u16) = (10, 20);

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
