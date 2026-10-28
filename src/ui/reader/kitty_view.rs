//! Showing pages through the kitty protocol: every page near the current one is decoded,
//! shrunk if needed and sent to the terminal ahead of time, so turning a page or scrolling only
//! redraws placeholder cells.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::widgets::{Paragraph, Wrap};

use super::ViewState;
use super::layout::{Placement, place};
use crate::library::LocalChapter;
use crate::library::prepare::{Bounds, fitted_size, prepare};
use crate::ui::kitty::{self, Medium, Payload, Placeholders};

/// Pages prepared at once. Each takes 10–20 ms; more would only compete with the current page.
const MAX_IN_FLIGHT: usize = 3;

/// Decodes a page, shrinks it to `bounds` and, for [`Medium::TempFile`], writes the pixels to
/// the file the terminal will read. Blocking; run it on a worker thread.
pub struct PrepareJob {
    reader_id: u64,
    page: usize,
    name: String,
    chapter: LocalChapter,
    bounds: Bounds,
    file: Option<PathBuf>,
}

pub struct Prepared {
    pub(super) reader_id: u64,
    page: usize,
    result: Result<PreparedPage, String>,
}

struct PreparedPage {
    native: (u32, u32),
    size: (u32, u32),
    payload: Payload,
}

impl PrepareJob {
    pub fn run(self) -> Prepared {
        let result = prepare(&self.chapter, &self.name, self.bounds).map(|page| PreparedPage {
            native: page.native,
            size: (page.width, page.height),
            payload: Payload::new(self.file, page.rgb),
        });
        Prepared {
            reader_id: self.reader_id,
            page: self.page,
            result,
        }
    }
}

/// A page held by the terminal.
struct Sent {
    id: u32,
    /// Pixel size sent, after shrinking.
    size: (u32, u32),
    /// Cells of the current placement.
    placed: (u16, u16),
}

pub struct KittyView {
    medium: Medium,
    /// Pixels per cell, needed to fit pages and decide how far to shrink them.
    cell: (u16, u16),
    area: Rect,
    sent: HashMap<usize, Sent>,
    /// Original pixel sizes of pages decoded so far.
    native: HashMap<usize, (u32, u32)>,
    in_flight: HashSet<usize>,
    failed: HashMap<usize, String>,
    /// Pages to prepare, most urgent first. Rebuilt every frame.
    queue: Vec<(usize, Bounds)>,
    /// Escape sequences for the terminal, written before the next frame is drawn.
    output: Vec<u8>,
    /// Where the current view's pages are, once all of them are in the terminal.
    current: Option<Placement>,
}

impl KittyView {
    pub fn new(medium: Medium, cell: (u16, u16)) -> Self {
        KittyView {
            medium,
            cell,
            area: Rect::default(),
            sent: HashMap::new(),
            native: HashMap::new(),
            in_flight: HashSet::new(),
            failed: HashMap::new(),
            queue: Vec::new(),
            output: Vec::new(),
            current: None,
        }
    }

    /// Works out what the terminal needs before drawing `area`: drops pages that are no longer
    /// near, queues pages to prepare (current view first, then the next, the previous and the
    /// one after), and places the current view's pages. Returns how far the view can scroll.
    pub fn prepare_frame(&mut self, state: &ViewState, area: Rect) -> u16 {
        if area != self.area {
            self.area = area;
            self.refresh_cell_size();
        }
        let window = window(state);
        let wanted: Vec<usize> = window
            .iter()
            .flat_map(|&view| state.views[view].iter().copied())
            .collect();

        let gone: Vec<usize> = self
            .sent
            .keys()
            .filter(|page| !wanted.contains(page))
            .copied()
            .collect();
        for page in gone {
            let sent = self.sent.remove(&page).expect("listed above");
            kitty::delete(&mut self.output, sent.id);
        }

        self.queue.clear();
        for &view in &window {
            for &page in &state.views[view] {
                if self.in_flight.contains(&page) || self.failed.contains_key(&page) {
                    continue;
                }
                let bounds = self.bounds(state, view, page);
                let up_to_date = match (self.sent.get(&page), self.native.get(&page)) {
                    (Some(sent), Some(&native)) => {
                        good_enough(sent.size, fitted_size(native, bounds))
                    }
                    _ => false,
                };
                if !up_to_date {
                    self.queue.push((page, bounds));
                }
            }
        }

        let current = &state.views[state.view];
        self.current = if current.iter().all(|page| self.sent.contains_key(page)) {
            let placement = self.placement(state, state.view);
            for rect in &placement.rects {
                let sent = self.sent.get_mut(&rect.page).expect("checked above");
                if sent.placed != (rect.cols, rect.rows) {
                    sent.placed = (rect.cols, rect.rows);
                    kitty::place(&mut self.output, sent.id, sent.placed);
                }
            }
            Some(placement)
        } else {
            None
        };
        self.current
            .as_ref()
            .map_or(0, |placement| placement.max_scroll)
    }

    /// Hands out queued pages to prepare, up to the in-flight limit.
    pub fn take_requests(&mut self, state: &ViewState) -> Vec<PrepareJob> {
        let free = MAX_IN_FLIGHT.saturating_sub(self.in_flight.len());
        let queue = std::mem::take(&mut self.queue);
        queue
            .into_iter()
            .take(free)
            .map(|(page, bounds)| {
                self.in_flight.insert(page);
                PrepareJob {
                    reader_id: state.reader_id,
                    page,
                    name: state.pages[page].clone(),
                    chapter: state.chapter.clone(),
                    bounds,
                    file: self.medium.temp_file(),
                }
            })
            .collect()
    }

    /// Sends a prepared page to the terminal, unless it's no longer needed.
    pub fn on_prepared(&mut self, state: &ViewState, prepared: Prepared) {
        self.in_flight.remove(&prepared.page);
        let page = prepared.page;
        let near = window(state)
            .iter()
            .any(|&view| state.views[view].contains(&page));
        if !near {
            // Dropping it removes its file.
            return;
        }
        match prepared.result {
            Err(err) => {
                self.failed.insert(page, err);
            }
            Ok(prepared) => {
                self.native.insert(page, prepared.native);
                let id = self
                    .sent
                    .get(&page)
                    .map_or_else(kitty::next_id, |sent| sent.id);
                let placed = self.cells_for(state, page);
                prepared
                    .payload
                    .transmit(&mut self.output, id, prepared.size, placed);
                self.sent.insert(
                    page,
                    Sent {
                        id,
                        size: prepared.size,
                        placed,
                    },
                );
            }
        }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Deletes every page from the terminal; for when the reader closes.
    pub fn close(&mut self) -> Vec<u8> {
        for (_, sent) in self.sent.drain() {
            kitty::delete(&mut self.output, sent.id);
        }
        self.take_output()
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, state: &ViewState) {
        if let Some(placement) = &self.current {
            for rect in &placement.rects {
                let id = self.sent[&rect.page].id;
                let placeholders = Placeholders {
                    id,
                    row_offset: rect.row_offset,
                };
                frame.render_widget(placeholders, rect.area);
            }
            return;
        }
        let failure = state.views[state.view]
            .iter()
            .find_map(|page| self.failed.get(page));
        let message = match failure {
            Some(err) => Paragraph::new(format!("Couldn't show this page: {err}").red()),
            None => Paragraph::new("Loading page…".dark_gray()),
        };
        let line = Rect::new(
            area.x,
            area.y + area.height / 2,
            area.width,
            1.min(area.height),
        );
        frame.render_widget(message.centered().wrap(Wrap { trim: true }), line);
    }

    fn refresh_cell_size(&mut self) {
        if let Some(cell) = kitty::cell_size() {
            self.cell = cell;
        }
    }

    /// Where a view's pages go, using the sizes of pages decoded so far. A page not decoded
    /// yet is assumed to be shaped like its neighbour.
    fn placement(&self, state: &ViewState, view: usize) -> Placement {
        let pages = &state.views[view];
        let known = pages.iter().find_map(|page| self.native.get(page)).copied();
        let sizes: Vec<(usize, (u32, u32))> = pages
            .iter()
            .map(|&page| {
                let size = self.native.get(&page).copied().or(known).unwrap_or((1, 1));
                (page, size)
            })
            .collect();
        place(
            &sizes,
            state.direction,
            state.fit,
            self.area,
            self.cell,
            state.scroll,
        )
    }

    /// The largest size `page` is shown at in `view`, so it's never sent bigger than needed.
    fn bounds(&self, state: &ViewState, view: usize, page: usize) -> Bounds {
        let pages = &state.views[view];
        let (cell_width, cell_height) = (u32::from(self.cell.0), u32::from(self.cell.1));
        if pages.iter().all(|page| self.native.contains_key(page)) {
            let placement = self.placement(state, view);
            if let Some(rect) = placement.rects.iter().find(|rect| rect.page == page) {
                return Bounds {
                    width: u32::from(rect.cols) * cell_width,
                    height: Some(u32::from(rect.rows) * cell_height),
                };
            }
        }
        let area_width = u32::from(self.area.width) * cell_width;
        Bounds {
            width: area_width / pages.len().max(1) as u32,
            height: (state.fit == super::Fit::Page)
                .then(|| u32::from(self.area.height) * cell_height),
        }
    }

    /// The placement size for a page that's about to be sent.
    fn cells_for(&self, state: &ViewState, page: usize) -> (u16, u16) {
        let view = state
            .views
            .iter()
            .position(|view| view.contains(&page))
            .unwrap_or(state.view);
        self.placement(state, view)
            .rects
            .iter()
            .find(|rect| rect.page == page)
            .map_or((1, 1), |rect| (rect.cols, rect.rows))
    }
}

/// The views kept in the terminal, most urgent first: current, next, previous, the one after.
fn window(state: &ViewState) -> Vec<usize> {
    let view = state.view;
    let mut views = vec![view, view + 1];
    if let Some(previous) = view.checked_sub(1) {
        views.push(previous);
    }
    views.push(view + 2);
    views.retain(|&v| v < state.views.len());
    views
}

/// Whether a page sent at `sent` pixels can be shown at `wanted` without visibly losing
/// detail (it's at least ~95% of what's useful) or wasting effort (at most twice the size;
/// the terminal's scaling also looks worse when shrinking further).
fn good_enough(sent: (u32, u32), wanted: (u32, u32)) -> bool {
    let (sent, wanted) = (u64::from(sent.0), u64::from(wanted.0));
    sent * 100 >= wanted * 95 && sent <= wanted * 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ChapterKind;
    use crate::library::state::Direction;
    use crate::ui::reader::{Fit, PageLayout, layout::page_views};
    use image::DynamicImage;
    use std::fs;
    use std::path::Path;

    struct Fixture {
        _dir: tempfile::TempDir,
        shm: tempfile::TempDir,
        chapter: LocalChapter,
        pages: Vec<String>,
    }

    /// Five 400x600 pages, and a folder standing in for /dev/shm.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut pages = Vec::new();
        for page in 1..=5 {
            let name = format!("{page:03}.png");
            DynamicImage::new_rgb8(400, 600)
                .save(dir.path().join(&name))
                .unwrap();
            pages.push(name);
        }
        let chapter = LocalChapter {
            name: "A".to_owned(),
            file_name: "A".to_owned(),
            path: dir.path().to_owned(),
            kind: ChapterKind::Folder,
        };
        Fixture {
            _dir: dir,
            shm: tempfile::tempdir().unwrap(),
            chapter,
            pages,
        }
    }

    fn state<'a>(
        f: &'a Fixture,
        views: &'a [Vec<usize>],
        view: usize,
        scroll: u16,
        fit: Fit,
    ) -> ViewState<'a> {
        ViewState {
            reader_id: 1,
            views,
            view,
            fit,
            direction: Direction::Rtl,
            scroll,
            pages: &f.pages,
            chapter: &f.chapter,
        }
    }

    /// Runs every job the view asks for until nothing's left, like the app's workers.
    fn settle(view: &mut KittyView, state: &ViewState, area: Rect) -> String {
        let mut output = Vec::new();
        for _ in 0..10 {
            view.prepare_frame(state, area);
            let jobs = view.take_requests(state);
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                view.on_prepared(state, job.run());
            }
        }
        view.prepare_frame(state, area);
        output.extend(view.take_output());
        String::from_utf8(output).unwrap()
    }

    fn count(text: &str, needle: &str) -> usize {
        text.matches(needle).count()
    }

    fn files_in(dir: &Path) -> usize {
        fs::read_dir(dir).unwrap().count()
    }

    #[test]
    fn sends_the_current_page_first_then_its_neighbours() {
        let f = fixture();
        let views = page_views(5, PageLayout::Single);
        let mut view = KittyView::new(Medium::TempFile(f.shm.path().to_owned()), (10, 20));
        let area = Rect::new(0, 0, 100, 40);
        let s = state(&f, &views, 2, 0, Fit::Page);

        view.prepare_frame(&s, area);
        let jobs = view.take_requests(&s);
        let order: Vec<usize> = jobs.iter().map(|job| job.page).collect();
        assert_eq!(order, [2, 3, 1], "current, next, previous; capped at three");
        assert!(
            view.take_requests(&s).is_empty(),
            "nothing more while three are running"
        );

        for job in jobs {
            view.on_prepared(&s, job.run());
        }
        let output = String::from_utf8(view.take_output()).unwrap();
        assert_eq!(count(&output, "a=T,U=1"), 3);
        assert!(
            output.contains(",f=24,t=t,s=400,v=600;"),
            "sent at native size, never upscaled"
        );
        assert_eq!(
            files_in(f.shm.path()),
            3,
            "pixels wait in files for the terminal"
        );
    }

    #[test]
    fn turning_and_scrolling_only_redraw_cells() {
        let f = fixture();
        let views = page_views(5, PageLayout::Single);
        let mut view = KittyView::new(Medium::Direct, (10, 20));
        let area = Rect::new(0, 0, 100, 40);
        settle(&mut view, &state(&f, &views, 0, 0, Fit::Page), area);

        // The next page is already in the terminal: turning sends at most a placement.
        let output = settle(&mut view, &state(&f, &views, 1, 0, Fit::Page), area);
        assert_eq!(
            count(&output, "a=T"),
            1,
            "only the newly near page 3 is sent"
        );
        assert!(view.current.is_some());

        // Fit width at a new scroll offset reuses the same image with a taller placement.
        let before = view.sent[&1].id;
        let output = settle(&mut view, &state(&f, &views, 1, 7, Fit::Width), area);
        assert!(!output.contains(&format!("a=T,U=1,q=2,i={before},")));
        assert!(output.contains(&format!("a=p,U=1,q=2,i={before},p=1,c=100,r=75")));
        let rect = view.current.as_ref().unwrap().rects[0];
        assert_eq!(rect.row_offset, 7);
    }

    #[test]
    fn deletes_pages_that_are_no_longer_near() {
        let f = fixture();
        let views = page_views(5, PageLayout::Single);
        let mut view = KittyView::new(Medium::Direct, (10, 20));
        let area = Rect::new(0, 0, 100, 40);
        settle(&mut view, &state(&f, &views, 0, 0, Fit::Page), area);
        let first = view.sent[&0].id;

        let output = settle(&mut view, &state(&f, &views, 4, 0, Fit::Page), area);
        assert!(output.contains(&format!("a=d,d=I,q=2,i={first}")));
        let mut kept: Vec<usize> = view.sent.keys().copied().collect();
        kept.sort();
        assert_eq!(kept, [3, 4]);

        let closing = String::from_utf8(view.close()).unwrap();
        assert_eq!(count(&closing, "a=d,d=I"), 2);
    }

    #[test]
    fn late_pages_are_dropped_with_their_files() {
        let f = fixture();
        let views = page_views(5, PageLayout::Single);
        let mut view = KittyView::new(Medium::TempFile(f.shm.path().to_owned()), (10, 20));
        let area = Rect::new(0, 0, 100, 40);
        let s = state(&f, &views, 0, 0, Fit::Page);
        view.prepare_frame(&s, area);
        let jobs = view.take_requests(&s);

        // The reader jumped to the end before the jobs finished.
        let far = state(&f, &views, 4, 0, Fit::Page);
        view.prepare_frame(&far, area);
        for job in jobs {
            view.on_prepared(&far, job.run());
        }
        assert!(view.sent.is_empty());
        assert_eq!(files_in(f.shm.path()), 0);
    }

    #[test]
    fn shrinks_big_pages_to_the_displayed_size() {
        assert!(good_enough((697, 1000), (697, 1000)));
        assert!(
            good_enough((848, 1216), (697, 1000)),
            "a little bigger is fine"
        );
        assert!(
            !good_enough((300, 430), (697, 1000)),
            "too small after the window grew"
        );
        assert!(
            !good_enough((1900, 2700), (697, 1000)),
            "much bigger than shown"
        );
    }
}
