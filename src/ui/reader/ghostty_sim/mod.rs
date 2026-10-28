//! End-to-end checks of the reader's kitty path against a model of Ghostty 1.3.1.
//!
//! [`model`] interprets the escape sequences the reader writes the way Ghostty's kitty
//! graphics code does, and resolves every placeholder cell of the drawn buffer the way
//! Ghostty's renderer does. The [`Harness`] drives a [`ReaderScreen`] like `app.rs`
//! (prepare_frame, write output, draw, start jobs, deliver results) over chapters of real
//! images whose pixels encode the page (red) and the position within it (green = x,
//! blue = y), and after every frame checks what each cell would show.

mod model;

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use image::{Rgb, RgbImage};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;

use self::model::{Ghostty, Location, PLACEHOLDER, Screen};
use super::layout::{PageRect, place};
use super::{Fit, Graphics, PageLayout, ReaderJob, ReaderScreen, split};
use crate::app::Action;
use crate::library::state::{Direction, ReadingState};
use crate::library::{ChapterKind, LibraryManga, LocalChapter};
use crate::ui::kitty::{MAX_COLUMNS, MAX_ROWS, Medium};

/// The most pages the reader keeps in the terminal: four views of up to two pages.
const MAX_IMAGES: usize = 8;
/// A generous bound on the tiles a strip keeps in the terminal.
const MAX_STRIP_IMAGES: usize = 40;

#[derive(Clone, Copy)]
struct PageSpec {
    size: (u32, u32),
    broken: bool,
}

fn page(w: u32, h: u32) -> PageSpec {
    PageSpec {
        size: (w, h),
        broken: false,
    }
}

fn broken() -> PageSpec {
    PageSpec {
        size: (1, 1),
        broken: true,
    }
}

/// Red channel identifying a page; far enough apart to survive resizing.
fn tag(chapter: usize, page: usize) -> u8 {
    (20 + 24 * page + 12 * chapter) as u8
}

struct Book {
    _dir: tempfile::TempDir,
    manga: LibraryManga,
    specs: Vec<Vec<PageSpec>>,
}

impl Book {
    fn new(chapters: &[Vec<PageSpec>]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut locals = Vec::new();
        for (c, pages) in chapters.iter().enumerate() {
            let name = format!("Chapter {}", c + 1);
            let path = dir.path().join(&name);
            std::fs::create_dir_all(&path).unwrap();
            for (p, spec) in pages.iter().enumerate() {
                let file = path.join(format!("{:03}.png", p + 1));
                if spec.broken {
                    std::fs::write(&file, b"not a png").unwrap();
                    continue;
                }
                let (w, h) = spec.size;
                let t = tag(c, p);
                RgbImage::from_fn(w, h, |x, y| {
                    Rgb([
                        t,
                        (u64::from(x) * 255 / u64::from(w - 1)) as u8,
                        (u64::from(y) * 255 / u64::from(h - 1)) as u8,
                    ])
                })
                .save(&file)
                .unwrap();
            }
            locals.push(LocalChapter {
                name: name.clone(),
                file_name: name,
                path,
                kind: ChapterKind::Folder,
            });
        }
        Book {
            _dir: dir,
            manga: LibraryManga {
                key: "Sim".into(),
                title: "Sim".into(),
                chapters: locals,
                dir: None,
                link: None,
            },
            specs: chapters.to_vec(),
        }
    }

    /// The (chapter, page) an image shows, from its red channel.
    fn identify(&self, image: &model::Image) -> Option<(usize, usize)> {
        let [r, _, _] = image.pixel(f64::from(image.width) / 2.0, f64::from(image.height) / 2.0);
        for (c, pages) in self.specs.iter().enumerate() {
            for p in 0..pages.len() {
                if (i32::from(r) - i32::from(tag(c, p))).abs() <= 4 {
                    return Some((c, p));
                }
            }
        }
        None
    }
}

/// The cell size `KittyView` works with: the terminal's if it reports one, else the picker's.
fn app_cell() -> (u16, u16) {
    match crossterm::terminal::window_size() {
        Ok(s) if s.columns > 0 && s.rows > 0 && s.width > 0 && s.height > 0 => {
            (s.width / s.columns, s.height / s.rows)
        }
        _ => {
            let font = Picker::halfblocks().font_size();
            (font.width, font.height)
        }
    }
}

/// The pages of the views the reader should keep: current, next, previous, the one after.
fn window_pages(views: &[Vec<usize>], view: usize) -> Vec<usize> {
    let mut wanted = vec![view, view + 1, view + 2];
    if view > 0 {
        wanted.push(view - 1);
    }
    let mut pages: Vec<usize> = wanted
        .into_iter()
        .filter(|&v| v < views.len())
        .flat_map(|v| views[v].iter().copied())
        .collect();
    pages.sort();
    pages
}

struct Harness {
    book: Book,
    shm: Option<tempfile::TempDir>,
    state: ReadingState,
    _state_dir: tempfile::TempDir,
    graphics: Graphics,
    reader: Option<ReaderScreen>,
    terminal: Terminal<TestBackend>,
    ghostty: Ghostty,
    /// Written before the next frame, like `App::terminal_output`.
    pending_output: Vec<u8>,
    /// Jobs handed out but not finished yet, oldest first.
    jobs: Vec<ReaderJob>,
    cell: (u16, u16),
    frames: usize,
    cells_checked: std::cell::Cell<usize>,
    most_images: usize,
    /// What the last frame showed, per page: rects and the screen.
    last_rects: Vec<PageRect>,
    last_screen: Option<Screen>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Direct,
    TempFile,
}

impl Harness {
    fn new(chapters: &[Vec<PageSpec>], via: Via, size: (u16, u16)) -> Self {
        Self::with(chapters, via, size, PageLayout::Single, Fit::Page)
    }

    fn with(
        chapters: &[Vec<PageSpec>],
        via: Via,
        size: (u16, u16),
        layout: PageLayout,
        fit: Fit,
    ) -> Self {
        let book = Book::new(chapters);
        let state_dir = tempfile::tempdir().unwrap();
        let state = ReadingState::load(state_dir.path().join("library.json")).0;
        let shm = (via == Via::TempFile).then(|| tempfile::tempdir().unwrap());
        let medium = match &shm {
            Some(dir) => Medium::TempFile(dir.path().to_owned()),
            None => Medium::Direct,
        };
        let graphics = Graphics {
            picker: Picker::halfblocks(),
            kitty: Some(medium),
        };
        let cell = app_cell();
        let (reader, _) = ReaderScreen::new(book.manga.clone(), 0, &state, &graphics, layout, fit);
        let mut harness = Harness {
            book,
            shm,
            state,
            _state_dir: state_dir,
            graphics,
            reader: Some(reader),
            terminal: Terminal::new(TestBackend::new(size.0, size.1)).unwrap(),
            ghostty: Ghostty::new((u32::from(cell.0), u32::from(cell.1))),
            pending_output: Vec::new(),
            jobs: Vec::new(),
            cell,
            frames: 0,
            cells_checked: std::cell::Cell::new(0),
            most_images: 0,
            last_rects: Vec::new(),
            last_screen: None,
        };
        harness.frame();
        harness
    }

    fn reader(&self) -> &ReaderScreen {
        self.reader.as_ref().expect("reader open")
    }

    /// One turn of the app's loop: reader commands, then the frame, then new jobs.
    fn frame(&mut self) {
        let size = self.terminal.size().unwrap();
        let area = Rect::new(0, 0, size.width, size.height);
        let mut out = std::mem::take(&mut self.pending_output);
        if let Some(reader) = &mut self.reader {
            reader.prepare_frame(area);
            out.extend(reader.take_output());
        }
        self.ghostty.feed(&out);
        let reader = &mut self.reader;
        self.terminal
            .draw(|frame| {
                if let Some(reader) = reader {
                    reader.render(frame, frame.area());
                }
            })
            .unwrap();
        self.frames += 1;
        self.verify();
        if let Some(reader) = &mut self.reader {
            self.jobs.extend(reader.take_requests());
        }
    }

    /// Finishes the oldest job and lets the app handle its result.
    fn finish_job(&mut self) {
        let job = self.jobs.remove(0);
        let result = job.run();
        if let Some(reader) = &mut self.reader {
            reader.on_job_done(result);
        }
        self.frame();
    }

    /// Runs frames and jobs until nothing's left to do, then checks the terminal holds exactly
    /// the pages near the current view.
    fn settle(&mut self) {
        for _ in 0..500 {
            if self.jobs.is_empty() {
                self.frame();
                if self.jobs.is_empty() {
                    self.check_settled();
                    return;
                }
            } else {
                self.finish_job();
            }
        }
        panic!("the reader never settled");
    }

    fn key(&mut self, code: KeyCode) -> Option<Action> {
        let reader = self.reader.as_mut().expect("reader open");
        let action = reader.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        self.apply(&action);
        self.frame();
        action
    }

    /// One step of the mouse wheel.
    fn wheel(&mut self, down: bool) -> Option<Action> {
        let reader = self.reader.as_mut().expect("reader open");
        let kind = if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        };
        let action = reader.handle_mouse(MouseEvent {
            kind,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        self.apply(&action);
        self.frame();
        action
    }

    /// Does what the app does with the reader's actions.
    fn apply(&mut self, action: &Option<Action>) {
        match action {
            Some(Action::CloseReader) => {
                let mut reader = self.reader.take().unwrap();
                self.pending_output.extend(reader.close());
            }
            Some(Action::OpenReader { manga, chapter }) => {
                let (layout, fit) = self.reader().preferences();
                let (screen, _) = ReaderScreen::new(
                    manga.clone(),
                    *chapter,
                    &self.state,
                    &self.graphics,
                    layout,
                    fit,
                );
                if let Some(mut old) = self.reader.replace(screen) {
                    self.pending_output.extend(old.close());
                }
            }
            Some(Action::SaveProgress {
                manga,
                chapter,
                page,
                finished,
            }) => self.state.record(manga, chapter, *page, *finished),
            Some(Action::SetStrip { manga, strip }) => self.state.set_strip(manga, *strip),
            _ => {}
        }
    }

    fn press(&mut self, code: KeyCode) {
        self.key(code);
        self.settle();
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().resize(width, height);
        self.frame();
        self.settle();
    }

    fn view_pages(&self) -> Vec<usize> {
        let r = &self.reader().reading;
        r.views[r.view].clone()
    }

    fn rect(&self, page: usize) -> PageRect {
        *self
            .last_rects
            .iter()
            .find(|r| r.page == page)
            .unwrap_or_else(|| panic!("page {page} isn't on screen"))
    }

    /// The position (0..1, 0..1) within its page that the centre of cell (x, y) shows.
    fn shown_at(&self, x: u16, y: u16) -> (usize, f64, f64) {
        let screen = self.last_screen.as_ref().unwrap();
        let cell = screen
            .at(x, y)
            .unwrap_or_else(|| panic!("({x},{y}) shows nothing"));
        let image = &self.ghostty.images[&cell.image_id];
        let (_, page) = self.book.identify(image).unwrap();
        let (sx, sy) = cell.centre();
        (
            page,
            sx / f64::from(image.width),
            sy / f64::from(image.height),
        )
    }

    fn files_waiting(&self) -> usize {
        self.shm
            .as_ref()
            .map_or(0, |dir| std::fs::read_dir(dir.path()).unwrap().count())
    }

    fn check_settled(&self) {
        let Some(reader) = &self.reader else {
            return;
        };
        let r = &reader.reading;
        if r.error.is_some() {
            return;
        }
        if r.strip.is_some() {
            return self.check_strip_settled(r);
        }
        let chapter = r.chapter;
        let mut held: Vec<usize> = self
            .ghostty
            .images
            .values()
            .filter_map(|image| self.book.identify(image).map(|(_, p)| p))
            .collect();
        held.sort();
        let wanted: Vec<usize> = window_pages(&r.views, r.view)
            .into_iter()
            .filter(|&p| !self.book.specs[chapter][p].broken)
            .collect();
        assert_eq!(
            held, wanted,
            "after settling the terminal should hold exactly the pages near view {} (views {:?})",
            r.view, r.views
        );
        assert_eq!(self.files_waiting(), 0, "page files left behind");
    }

    // -----------------------------------------------------------------------------------------
    // Checks after every frame
    // -----------------------------------------------------------------------------------------

    fn verify(&mut self) {
        let buf = self.terminal.backend().buffer().clone();
        let screen = self.ghostty.render(&buf);
        let mut problems: Vec<String> = Vec::new();
        problems.append(&mut self.ghostty.errors);
        problems.extend(screen.errors.iter().cloned());
        if !self.ghostty.responses.is_empty() {
            problems.push(format!(
                "Ghostty would reply on stdin: {:?}",
                self.ghostty.responses
            ));
        }
        if !self.ghostty.stray.is_empty() {
            problems.push(format!(
                "non-graphics bytes in the reader output: {:?}",
                String::from_utf8_lossy(&self.ghostty.stray)
            ));
            self.ghostty.stray.clear();
        }
        if !self.ghostty.evicted.is_empty() {
            problems.push(format!("Ghostty evicted images {:?}", self.ghostty.evicted));
        }
        if self.ghostty.loading_in_progress() {
            problems.push("a chunked transmission is still open after the frame".into());
        }
        self.most_images = self.most_images.max(self.ghostty.images.len());
        let strip = self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.reading.strip.is_some());
        let limit = if strip { MAX_STRIP_IMAGES } else { MAX_IMAGES };
        if self.ghostty.images.len() > limit {
            problems.push(format!(
                "Ghostty holds {} images",
                self.ghostty.images.len()
            ));
        }
        let mut rects = Vec::new();
        match &self.reader {
            None => {
                if !self.ghostty.images.is_empty() || !self.ghostty.placements.is_empty() {
                    problems.push(format!(
                        "reader closed but Ghostty still holds images {:?} and placements {:?}",
                        self.ghostty.images.keys().collect::<Vec<_>>(),
                        self.ghostty.placements
                    ));
                }
            }
            Some(reader) => rects = self.verify_reader(reader, &buf, &screen, &mut problems),
        }
        if !problems.is_empty() {
            problems.truncate(25);
            panic!(
                "frame {}: {}\n{}\n{}",
                self.frames,
                self.describe(),
                problems.join("\n"),
                self.map(&buf, &screen)
            );
        }
        self.last_rects = rects;
        self.last_screen = Some(screen);
    }

    fn describe(&self) -> String {
        match &self.reader {
            None => "reader closed".into(),
            Some(reader) if reader.reading.strip.is_some() => {
                let r = &reader.reading;
                let strip = r.strip.as_ref().unwrap();
                format!(
                    "chapter {} strip at page {} +{:.3}, scroll {} of {:?} rows, {:?}, popup {}, {} images {:?}",
                    r.chapter,
                    strip.page,
                    strip.offset,
                    strip.scroll,
                    strip.layout.as_ref().map(|l| l.total_rows()),
                    r.fit,
                    reader.end_prompt,
                    self.ghostty.images.len(),
                    self.ghostty
                        .images
                        .values()
                        .map(|i| (i.id, self.book.identify(i), i.width, i.height))
                        .collect::<Vec<_>>()
                )
            }
            Some(reader) => {
                let r = &reader.reading;
                format!(
                    "chapter {} view {} {:?} of {:?}, {:?} {:?} {:?}, scroll {}/{}, popup {}, {} images {:?}",
                    r.chapter,
                    r.view,
                    r.views.get(r.view),
                    r.views,
                    r.layout,
                    r.fit,
                    r.direction,
                    r.scroll,
                    r.max_scroll,
                    reader.end_prompt,
                    self.ghostty.images.len(),
                    self.ghostty
                        .images
                        .values()
                        .map(|i| (i.id, self.book.identify(i), i.width, i.height))
                        .collect::<Vec<_>>()
                )
            }
        }
    }

    /// A picture of the screen: page letters where images show, '#' for text, '?' for
    /// placeholders showing nothing.
    fn map(&self, buf: &Buffer, screen: &Screen) -> String {
        let mut out = String::new();
        if buf.area.width > 200 {
            return out;
        }
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let symbol = buf[(x, y)].symbol();
                let c = match screen.at(x, y) {
                    Some(cell) => match self
                        .ghostty
                        .images
                        .get(&cell.image_id)
                        .and_then(|i| self.book.identify(i))
                    {
                        Some((_, p)) => (b'a' + p as u8) as char,
                        None => '!',
                    },
                    None if symbol.starts_with(PLACEHOLDER) => '?',
                    None if symbol != " " => '#',
                    None => '.',
                };
                out.push(c);
            }
            out.push('\n');
        }
        out
    }

    fn verify_reader(
        &self,
        reader: &ReaderScreen,
        buf: &Buffer,
        screen: &Screen,
        problems: &mut Vec<String>,
    ) -> Vec<PageRect> {
        let r = &reader.reading;
        if r.strip.is_some() {
            self.verify_strip(reader, buf, screen, problems);
            return Vec::new();
        }
        let chapter = r.chapter;
        let (image_area, status) = split(buf.area);

        // What the terminal holds.
        let mut page_ids: HashMap<usize, u32> = HashMap::new();
        let window = if r.can_show() {
            window_pages(&r.views, r.view)
        } else {
            Vec::new()
        };
        for image in self.ghostty.images.values() {
            match self.book.identify(image) {
                Some((c, p)) if c == chapter => {
                    if !window.contains(&p) {
                        problems.push(format!(
                            "image {} (page {p}) is kept although it's outside the window {window:?}",
                            image.id
                        ));
                    }
                    if let Some(other) = page_ids.insert(p, image.id) {
                        problems.push(format!(
                            "page {p} is held twice, as images {other} and {}",
                            image.id
                        ));
                    }
                }
                other => problems.push(format!(
                    "image {} shows {other:?}, not a page of chapter {chapter}",
                    image.id
                )),
            }
            let placements: Vec<_> = self
                .ghostty
                .placements
                .iter()
                .filter(|(k, _)| k.image_id == image.id)
                .collect();
            let good = placements.len() == 1
                && placements[0].1.location == Location::Virtual
                && placements[0].0.external
                && placements[0].0.id == 1;
            if !good {
                problems.push(format!(
                    "image {} should have one virtual placement p=1, has {placements:?}",
                    image.id
                ));
            }
        }

        // Nothing image-like in the status line.
        for x in 0..status.width {
            if buf[(status.x + x, status.y)]
                .symbol()
                .starts_with(PLACEHOLDER)
                || screen.at(status.x + x, status.y).is_some()
            {
                problems.push(format!("status line cell {x} shows an image"));
            }
        }

        let text = crate::ui::buffer_text(&self.terminal);
        if reader.end_prompt && !text.contains("End of chapter") {
            problems.push("the end-of-chapter popup isn't drawn".into());
        }
        if !r.can_show() {
            if !screen.placeholder_cells.is_empty() {
                problems.push("placeholders drawn for a chapter that can't be shown".into());
            }
            return Vec::new();
        }

        let current = r.views[r.view].clone();
        let showing = current.iter().all(|p| page_ids.contains_key(p));
        if !showing {
            if !screen.placeholder_cells.is_empty() {
                problems.push(format!(
                    "placeholders drawn while pages {current:?} aren't all in the terminal"
                ));
            }
            if !text.contains("Loading page…") && !text.contains("Couldn't show this page") {
                problems.push("neither a page nor a loading or error message".into());
            }
            return Vec::new();
        }

        if image_area.is_empty() {
            if !screen.placeholder_cells.is_empty() {
                problems.push("placeholders drawn in an empty page area".into());
            }
            return Vec::new();
        }
        // Where the pages should be.
        let natives: Vec<(usize, (u32, u32))> = current
            .iter()
            .map(|&p| (p, self.book.specs[chapter][p].size))
            .collect();
        let placement = place(
            &natives,
            r.direction,
            r.fit,
            image_area,
            self.cell,
            r.scroll,
        );
        self.check_layout(r, image_area, &natives, &placement, problems);

        // The popup: text and cleared cells in the page area.
        let mut popup: Option<Rect> = None;
        for y in image_area.top()..image_area.bottom() {
            for x in image_area.left()..image_area.right() {
                let symbol = buf[(x, y)].symbol();
                if symbol != " " && !symbol.starts_with(PLACEHOLDER) {
                    popup = Some(match popup {
                        None => Rect::new(x, y, 1, 1),
                        Some(p) => p.union(Rect::new(x, y, 1, 1)),
                    });
                }
            }
        }
        if reader.end_prompt != popup.is_some() {
            problems.push(format!(
                "popup shown: {}, text in the page area: {popup:?}",
                reader.end_prompt
            ));
        }
        let in_popup = |x: u16, y: u16| popup.is_some_and(|p| p.contains((x, y).into()));

        let mut reported = 0;
        let mut report = |problems: &mut Vec<String>, message: String| {
            reported += 1;
            if reported <= 12 {
                problems.push(message);
            }
        };
        for y in image_area.top()..image_area.bottom() {
            for x in image_area.left()..image_area.right() {
                let rect = placement
                    .rects
                    .iter()
                    .find(|rect| rect.area.contains((x, y).into()));
                let shown = screen.at(x, y);
                if in_popup(x, y) {
                    if shown.is_some() || buf[(x, y)].symbol().starts_with(PLACEHOLDER) {
                        report(problems, format!("popup cell ({x},{y}) shows an image"));
                    }
                    continue;
                }
                let Some(rect) = rect else {
                    if shown.is_some() || buf[(x, y)].symbol().starts_with(PLACEHOLDER) {
                        report(
                            problems,
                            format!("({x},{y}) is outside every page but shows an image"),
                        );
                    }
                    continue;
                };
                self.cells_checked.set(self.cells_checked.get() + 1);
                let Some(cell) = shown else {
                    // Only the letterbox around a page that doesn't fill its cells exactly may
                    // stay empty.
                    let letterbox = page_ids
                        .get(&rect.page)
                        .and_then(|id| self.ghostty.images.get(id))
                        .is_some_and(|image| {
                            let fit = Fitted::new(rect, (image.width, image.height), self.cell);
                            !fit.touches(x - rect.area.x, y - rect.area.y + rect.row_offset)
                        });
                    if !letterbox {
                        report(
                            problems,
                            format!("({x},{y}) of page {} shows nothing", rect.page),
                        );
                    }
                    continue;
                };
                let Some(image) = self.ghostty.images.get(&cell.image_id) else {
                    report(
                        problems,
                        format!("({x},{y}) shows unknown image {}", cell.image_id),
                    );
                    continue;
                };
                let identity = self.book.identify(image);
                if identity != Some((chapter, rect.page)) {
                    report(
                        problems,
                        format!(
                            "({x},{y}) should show page {} but shows {identity:?}",
                            rect.page
                        ),
                    );
                    continue;
                }
                let (sx, sy) = cell.centre();
                let u = sx / f64::from(image.width);
                let v = sy / f64::from(image.height);
                // Where this cell should be within the page: the page is fitted into the
                // placement's grid keeping its aspect ratio, and centred (as Ghostty does).
                let fit = Fitted::new(rect, (image.width, image.height), self.cell);
                let (eu, ev) = fit.expected(x - rect.area.x, y - rect.area.y + rect.row_offset);
                if (u - eu).abs() > fit.tol_u || (v - ev).abs() > fit.tol_v {
                    report(
                        problems,
                        format!(
                            "({x},{y}) of page {} shows ({u:.4},{v:.4}) of the page instead of ({eu:.4},{ev:.4})",
                            rect.page
                        ),
                    );
                    continue;
                }
                let interior = fit.interior(x - rect.area.x, y - rect.area.y + rect.row_offset);
                if cell.coverage < if interior { 0.999 } else { 0.0 } {
                    report(
                        problems,
                        format!(
                            "({x},{y}) of page {} is only {:.0}% covered",
                            rect.page,
                            cell.coverage * 100.0
                        ),
                    );
                }
                let [pr, pg, pb] = image.pixel(sx, sy);
                if (i32::from(pr) - i32::from(tag(chapter, rect.page))).abs() > 4
                    || (f64::from(pg) / 255.0 - u).abs() > 0.03 + 1.0 / f64::from(image.width)
                    || (f64::from(pb) / 255.0 - v).abs() > 0.03 + 1.0 / f64::from(image.height)
                {
                    report(
                        problems,
                        format!(
                            "({x},{y}): pixel {:?} doesn't match page {} at ({u:.3},{v:.3})",
                            [pr, pg, pb],
                            rect.page
                        ),
                    );
                }
            }
        }
        if reported > 12 {
            problems.push(format!("... {} cell problems in all", reported));
        }
        placement.rects
    }

    /// Checks `layout::place` against what the reader is meant to do, independently of how it
    /// computes it.
    fn check_layout(
        &self,
        r: &super::Reading,
        area: Rect,
        natives: &[(usize, (u32, u32))],
        placement: &super::layout::Placement,
        problems: &mut Vec<String>,
    ) {
        let rects = &placement.rects;
        let (cw, ch) = (f64::from(self.cell.0), f64::from(self.cell.1));
        let mut pages: Vec<usize> = rects.iter().map(|r| r.page).collect();
        pages.sort();
        let mut want: Vec<usize> = natives.iter().map(|n| n.0).collect();
        want.sort();
        if pages != want {
            problems.push(format!("layout shows pages {pages:?} instead of {want:?}"));
            return;
        }
        for rect in rects {
            let native = natives.iter().find(|n| n.0 == rect.page).unwrap().1;
            let aspect = f64::from(native.0) / f64::from(native.1);
            let shown = f64::from(rect.cols) * cw / (f64::from(rect.rows) * ch);
            if (shown / aspect - 1.0).abs()
                > 1.0 / f64::from(rect.cols) + 1.0 / f64::from(rect.rows)
            {
                problems.push(format!(
                    "page {} is placed {}x{} cells, aspect {shown:.3} instead of {aspect:.3}",
                    rect.page, rect.cols, rect.rows
                ));
            }
            if !area.contains(rect.area.as_position())
                || rect.area.right() > area.right()
                || rect.area.bottom() > area.bottom()
            {
                problems.push(format!(
                    "page {} at {:?} leaves the area {area:?}",
                    rect.page, rect.area
                ));
            }
            if rect.area.height != rect.rows.min(area.height) || rect.rows > MAX_ROWS {
                problems.push(format!(
                    "page {} shows {} of {} rows",
                    rect.page, rect.area.height, rect.rows
                ));
            }
            if rect.area.width != rect.cols {
                problems.push(format!(
                    "page {} is cut to {} of {} columns",
                    rect.page, rect.area.width, rect.cols
                ));
            }
            let max_scroll = rect.rows - rect.area.height;
            if rect.row_offset != r.scroll.min(max_scroll) || r.scroll > max_scroll {
                problems.push(format!(
                    "page {} starts at row {} with scroll {} (max {max_scroll})",
                    rect.page, rect.row_offset, r.scroll
                ));
            }
        }
        if rects.len() == 2 {
            let (left, right) = (rects[0], rects[1]);
            let rtl_ok = left.page > right.page;
            if (r.direction == Direction::Rtl) != rtl_ok {
                problems.push(format!(
                    "{:?} spread shows page {} on the left of page {}",
                    r.direction, left.page, right.page
                ));
            }
            if left.area.right() != right.area.x
                || left.area.y != right.area.y
                || left.rows != right.rows
            {
                problems.push("spread pages don't line up".into());
            }
        }
        let first = rects[0].area;
        let last = rects[rects.len() - 1].area;
        let left = first.x - area.x;
        let right = area.right() - last.right();
        let top = first.y - area.y;
        let bottom = area.bottom() - first.bottom();
        if left.abs_diff(right) > 1 || top.abs_diff(bottom) > 1 {
            problems.push(format!(
                "pages aren't centred: {left}/{right} {top}/{bottom}"
            ));
        }
        let rows = rects[0].rows;
        let width: u16 = rects.iter().map(|r| r.cols).sum();
        let slack = rects.len() as u16;
        let fills_width = width + slack >= area.width;
        let fills_height = rows + 1 >= area.height;
        let maximal = match r.fit {
            Fit::Page => rows <= area.height && (fills_width || fills_height),
            Fit::Width => fills_width,
        };
        // Pages also stop growing at the addressable row or column limit.
        let at_limit = rows == MAX_ROWS || rects.iter().any(|r| r.cols == MAX_COLUMNS);
        if !maximal && !at_limit {
            problems.push(format!(
                "{:?}: pages are {width}x{rows} cells in a {}x{} area",
                r.fit, area.width, area.height
            ));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Long strips
// ---------------------------------------------------------------------------------------------

/// The strip as the reader is meant to lay it out, worked out from the page files' sizes:
/// pages at their usual width (or the area's), stacked, each cut into images of at most 297
/// rows and 10000 px.
struct ExpectedStrip {
    x: u16,
    cols: u16,
    starts: Vec<u32>,
    per_tile: u32,
    broken: Vec<bool>,
}

impl ExpectedStrip {
    fn new(specs: &[PageSpec], fit: Fit, area: Rect, cell: (u16, u16)) -> Self {
        let (cw, ch) = (u32::from(cell.0), u32::from(cell.1));
        let mut widths: Vec<u32> = specs
            .iter()
            .filter(|s| !s.broken)
            .map(|s| s.size.0)
            .collect();
        widths.sort();
        let width = widths.get(widths.len() / 2).copied().unwrap_or(1);
        let full = u32::from(area.width)
            .min(u32::from(MAX_COLUMNS))
            .min(10_000 / cw)
            .max(1);
        let cols = match fit {
            Fit::Width => full,
            Fit::Page => (width / cw).clamp(1, full),
        };
        let mut starts = vec![0];
        for spec in specs {
            let rows = if spec.broken {
                3
            } else {
                let height = f64::from(spec.size.1) * f64::from(cols * cw) / f64::from(spec.size.0);
                ((height / f64::from(ch)).round() as u32).max(1)
            };
            starts.push(starts.last().unwrap() + rows);
        }
        ExpectedStrip {
            x: area.x + (area.width - cols as u16) / 2,
            cols: cols as u16,
            starts,
            per_tile: u32::from(MAX_ROWS).min(10_000 / ch),
            broken: specs.iter().map(|s| s.broken).collect(),
        }
    }

    fn total(&self) -> u32 {
        *self.starts.last().unwrap()
    }

    fn rows(&self, page: usize) -> u32 {
        self.starts[page + 1] - self.starts[page]
    }

    /// The page at strip row `row`, and the row within it.
    fn page_at(&self, row: u32) -> Option<(usize, u32)> {
        (0..self.starts.len() - 1)
            .find(|&p| row < self.starts[p + 1])
            .map(|p| (p, row - self.starts[p]))
    }

    /// The pages the reader should keep in the terminal: from a screen above to two below.
    fn near(&self, scroll: u32, height: u16) -> Vec<usize> {
        let height = u32::from(height.max(1));
        let (from, to) = (scroll.saturating_sub(height), scroll + 3 * height);
        (0..self.starts.len() - 1)
            .filter(|&p| self.starts[p] < to && self.starts[p + 1] > from && !self.broken[p])
            .collect()
    }
}

/// The end-of-chapter popup: the box between its top-left and bottom-right corners.
fn popup_box(buf: &Buffer, area: Rect) -> Option<Rect> {
    let mut top_left = None;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            match buf[(x, y)].symbol() {
                "┌" if top_left.is_none() => top_left = Some((x, y)),
                "┘" => {
                    let (left, top) = top_left?;
                    return Some(Rect::new(left, top, x + 1 - left, y + 1 - top));
                }
                _ => {}
            }
        }
    }
    None
}

impl Harness {
    fn strip_scroll(&self) -> u32 {
        self.reader().reading.strip.as_ref().unwrap().scroll
    }

    fn strip_max_scroll(&self) -> u32 {
        let strip = self.reader().reading.strip.as_ref().unwrap();
        strip.layout.as_ref().unwrap().max_scroll()
    }

    fn top_page(&self) -> usize {
        self.reader().reading.first_page()
    }

    fn check_strip_settled(&self, r: &super::Reading) {
        let strip = r.strip.as_ref().unwrap();
        assert!(
            strip.layout.is_some(),
            "settled without measuring the pages"
        );
        let size = self.terminal.size().unwrap();
        let (area, _) = split(Rect::new(0, 0, size.width, size.height));
        let expected = ExpectedStrip::new(&self.book.specs[r.chapter], r.fit, area, self.cell);
        let mut tiles: HashMap<usize, u32> = HashMap::new();
        for image in self.ghostty.images.values() {
            if let Some((_, page)) = self.book.identify(image) {
                *tiles.entry(page).or_default() += 1;
            }
        }
        let mut held: Vec<usize> = tiles.keys().copied().collect();
        held.sort();
        let near = expected.near(strip.scroll, area.height);
        assert_eq!(
            held, near,
            "after settling the terminal should hold exactly the pages near scroll {}",
            strip.scroll
        );
        for page in near {
            let want = expected.rows(page).div_ceil(expected.per_tile);
            assert_eq!(tiles[&page], want, "page {page} should be in {want} images");
        }
        assert_eq!(self.files_waiting(), 0, "page files left behind");
        let text = crate::ui::buffer_text(&self.terminal);
        assert!(
            !text.contains("Loading page…"),
            "still loading after settling"
        );
    }

    fn verify_strip(
        &self,
        reader: &ReaderScreen,
        buf: &Buffer,
        screen: &Screen,
        problems: &mut Vec<String>,
    ) {
        let r = &reader.reading;
        let strip = r.strip.as_ref().unwrap();
        let chapter = r.chapter;
        let (area, status) = split(buf.area);
        let text = crate::ui::buffer_text(&self.terminal);
        let lines: Vec<&str> = text.lines().collect();
        for x in 0..status.width {
            if buf[(status.x + x, status.y)]
                .symbol()
                .starts_with(PLACEHOLDER)
                || screen.at(status.x + x, status.y).is_some()
            {
                problems.push(format!("status line cell {x} shows an image"));
            }
        }

        // Every image is a tile of a page of this chapter, with one virtual placement.
        let mut tile_rows: HashMap<u32, u32> = HashMap::new();
        for image in self.ghostty.images.values() {
            match self.book.identify(image) {
                Some((c, _)) if c == chapter => {}
                other => problems.push(format!(
                    "image {} shows {other:?}, not a page of chapter {chapter}",
                    image.id
                )),
            }
            let placements: Vec<_> = self
                .ghostty
                .placements
                .iter()
                .filter(|(k, _)| k.image_id == image.id)
                .collect();
            if placements.len() == 1
                && placements[0].1.location == Location::Virtual
                && placements[0].0.external
                && placements[0].0.id == 1
            {
                tile_rows.insert(image.id, placements[0].1.rows);
            } else {
                problems.push(format!(
                    "image {} should have one virtual placement p=1, has {placements:?}",
                    image.id
                ));
            }
        }
        if area.is_empty() {
            return;
        }
        let Some(layout) = &strip.layout else {
            if !screen.placeholder_cells.is_empty() {
                problems.push("placeholders drawn before the pages are measured".into());
            }
            if !text.contains("Loading page…") {
                problems.push("nothing says the strip is loading".into());
            }
            return;
        };
        let expected = ExpectedStrip::new(&self.book.specs[chapter], r.fit, area, self.cell);
        if (layout.x, layout.cols, layout.total_rows())
            != (expected.x, expected.cols, expected.total())
        {
            problems.push(format!(
                "strip at x {} with {} columns and {} rows instead of x {} with {} and {}",
                layout.x,
                layout.cols,
                layout.total_rows(),
                expected.x,
                expected.cols,
                expected.total()
            ));
            return;
        }
        let scroll = strip.scroll;
        let max_scroll = expected.total().saturating_sub(u32::from(area.height));
        if scroll > max_scroll {
            problems.push(format!(
                "scrolled to {scroll}, past the end at {max_scroll}"
            ));
        }
        let popup = popup_box(buf, area);
        if reader.end_prompt != popup.is_some() {
            problems.push(format!(
                "popup shown: {}, popup box on screen: {popup:?}",
                reader.end_prompt
            ));
        }
        let in_popup = |x: u16, y: u16| popup.is_some_and(|p| p.contains((x, y).into()));
        let row_of = |y: u16| scroll + u32::from(y - area.y);

        // Pages that show anything; every other page on screen says why it doesn't.
        let mut shown_pages = std::collections::HashSet::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = screen.at(x, y)
                    && let Some((_, page)) = self
                        .ghostty
                        .images
                        .get(&cell.image_id)
                        .and_then(|i| self.book.identify(i))
                {
                    shown_pages.insert(page);
                }
            }
        }
        let bottom = (scroll + u32::from(area.height)).min(expected.total());
        let mut row = scroll;
        while row < bottom {
            let (page, within) = expected.page_at(row).unwrap();
            let rows = (expected.rows(page) - within).min(bottom - row);
            let y = area.y + (row - scroll) as u16 + (rows / 2) as u16;
            let line = lines[usize::from(y)];
            let hidden = popup.is_some_and(|p| y >= p.top() && y < p.bottom());
            // The message needs room to be read.
            let room = expected.cols >= 14;
            if !shown_pages.contains(&page)
                && !hidden
                && room
                && !line.contains("Loading page…")
                && !line.contains(&format!("Couldn't show page {}", page + 1))
            {
                problems.push(format!(
                    "page {page} is on screen but shows neither the page nor a message"
                ));
            }
            row += rows;
        }

        let mut reported = 0;
        let mut report = |problems: &mut Vec<String>, message: String| {
            reported += 1;
            if reported <= 12 {
                problems.push(message);
            }
        };
        let cols = f64::from(expected.cols);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let shown = screen.at(x, y);
                let placeholder = buf[(x, y)].symbol().starts_with(PLACEHOLDER);
                if in_popup(x, y) {
                    if shown.is_some() || placeholder {
                        report(problems, format!("popup cell ({x},{y}) shows an image"));
                    }
                    continue;
                }
                let col = x.checked_sub(expected.x).filter(|&col| col < expected.cols);
                let (Some(col), Some((page, within))) = (col, expected.page_at(row_of(y))) else {
                    if shown.is_some() || placeholder {
                        report(
                            problems,
                            format!("({x},{y}) is outside the strip but shows an image"),
                        );
                    }
                    continue;
                };
                self.cells_checked.set(self.cells_checked.get() + 1);
                let Some(cell) = shown else {
                    if shown_pages.contains(&page) {
                        report(problems, format!("({x},{y}) of page {page} shows nothing"));
                    }
                    continue;
                };
                let image = &self.ghostty.images[&cell.image_id];
                if self.book.identify(image) != Some((chapter, page)) {
                    report(
                        problems,
                        format!(
                            "({x},{y}) should show page {page} but shows {:?}",
                            self.book.identify(image)
                        ),
                    );
                    continue;
                }
                // Which row of the page this is: the row within the tile, from where the cell
                // samples the image, plus where the tile starts, told apart by colour.
                let page_rows = f64::from(expected.rows(page));
                let (cx, cy) = cell.centre();
                let rows_in_tile = f64::from(tile_rows.get(&cell.image_id).copied().unwrap_or(1));
                let tile_row = cy / f64::from(image.height) * rows_in_tile;
                let [_, pg, pb] = image.pixel(cx, cy);
                let coarse = f64::from(pb) / 255.0 * page_rows;
                let per_tile = f64::from(expected.per_tile);
                let tile = ((coarse - tile_row) / per_tile).round().max(0.0);
                let page_row = tile * per_tile + tile_row;
                let want_row = f64::from(within) + 0.5;
                let want_u = (f64::from(col) + 0.5) / cols;
                let u = cx / f64::from(image.width);
                if (page_row - want_row).abs() > 0.75 || (u - want_u).abs() > 0.75 / cols {
                    report(
                        problems,
                        format!(
                            "({x},{y}) shows row {page_row:.2}, x {u:.3} of page {page} instead of row {want_row}, x {want_u:.3}"
                        ),
                    );
                    continue;
                }
                let v = want_row / page_rows;
                if (f64::from(pg) / 255.0 - want_u).abs() > 0.03 + 1.0 / f64::from(image.width)
                    || (f64::from(pb) / 255.0 - v).abs() > 0.03 + 1.0 / page_rows
                {
                    report(
                        problems,
                        format!(
                            "({x},{y}): pixel {:?} isn't page {page} at ({want_u:.3},{v:.3})",
                            [pg, pb]
                        ),
                    );
                }
                // Pages and tiles meet without a gap; a page that's scaled up may leave a
                // sliver at the strip's sides.
                let edge = col == 0 || col + 1 == expected.cols;
                let needed = if edge { 0.8 } else { 0.999 };
                if cell.coverage < needed {
                    report(
                        problems,
                        format!(
                            "({x},{y}) of page {page} is only {:.1}% covered",
                            cell.coverage * 100.0
                        ),
                    );
                }
            }
        }
        if reported > 12 {
            problems.push(format!("... {reported} cell problems in all"));
        }
    }
}

/// How a page image is fitted into its placement: Ghostty keeps the image's aspect ratio
/// inside the grid of `cols` x `rows` cells and centres it.
struct Fitted {
    cell: (f64, f64),
    ox: f64,
    oy: f64,
    fw: f64,
    fh: f64,
    tol_u: f64,
    tol_v: f64,
}

impl Fitted {
    fn new(rect: &PageRect, image: (u32, u32), cell: (u16, u16)) -> Self {
        let (cw, ch) = (f64::from(cell.0), f64::from(cell.1));
        let gw = f64::from(rect.cols) * cw;
        let gh = f64::from(rect.rows) * ch;
        let aspect = f64::from(image.0) / f64::from(image.1);
        let (fw, fh) = if aspect * gh > gw {
            (gw, gw / aspect)
        } else {
            (gh * aspect, gh)
        };
        // Ghostty rounds source and destination rectangles to whole pixels, per run.
        let (sx, sy) = (fw / f64::from(image.0), fh / f64::from(image.1));
        Fitted {
            cell: (cw, ch),
            ox: (gw - fw) / 2.0,
            oy: (gh - fh) / 2.0,
            fw,
            fh,
            tol_u: (1.5 + sx) / fw,
            tol_v: (1.5 + sy) / fh,
        }
    }

    /// The page position (0..1) the centre of the visible part of cell (col, row) of the
    /// grid should show.
    fn expected(&self, col: u16, row: u16) -> (f64, f64) {
        let clamp = |lo: f64, hi: f64, a: f64, b: f64| (lo.max(a) + hi.min(b)) / 2.0;
        let x0 = f64::from(col) * self.cell.0;
        let y0 = f64::from(row) * self.cell.1;
        let cx = clamp(x0, x0 + self.cell.0, self.ox, self.ox + self.fw);
        let cy = clamp(y0, y0 + self.cell.1, self.oy, self.oy + self.fh);
        ((cx - self.ox) / self.fw, (cy - self.oy) / self.fh)
    }

    /// Whether the image covers the whole cell.
    fn interior(&self, col: u16, row: u16) -> bool {
        let x0 = f64::from(col) * self.cell.0;
        let y0 = f64::from(row) * self.cell.1;
        x0 >= self.ox + 1.0
            && x0 + self.cell.0 <= self.ox + self.fw - 1.0
            && y0 >= self.oy + 1.0
            && y0 + self.cell.1 <= self.oy + self.fh - 1.0
    }

    /// Whether the image reaches more than a pixel or so into the cell.
    fn touches(&self, col: u16, row: u16) -> bool {
        let x0 = f64::from(col) * self.cell.0;
        let y0 = f64::from(row) * self.cell.1;
        x0 + self.cell.0 > self.ox + 1.5
            && x0 < self.ox + self.fw - 1.5
            && y0 + self.cell.1 > self.oy + 1.5
            && y0 < self.oy + self.fh - 1.5
    }
}

fn portrait_chapter() -> Vec<PageSpec> {
    vec![
        page(800, 1200),
        page(760, 1140),
        page(800, 1200),
        page(1600, 1150),
        page(800, 1200),
        page(700, 1100),
        page(820, 1180),
        page(800, 1200),
    ]
}

fn short_chapter() -> Vec<PageSpec> {
    vec![page(800, 1200), page(800, 1200), page(800, 1200)]
}

// ---------------------------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------------------------

#[test]
fn opens_and_turns_pages_forward_and_back() {
    let mut h = Harness::new(&[portrait_chapter()], Via::Direct, (120, 40));
    h.settle();
    assert_eq!(h.view_pages(), [0]);
    // The page's top-left and bottom-right corners are on screen.
    let rect = h.rect(0);
    let (p, u, v) = h.shown_at(rect.area.x, rect.area.y);
    assert!(p == 0 && u < 0.05 && v < 0.05);
    let (_, u, v) = h.shown_at(rect.area.right() - 1, rect.area.bottom() - 1);
    assert!(u > 0.95 && v > 0.95);
    for expected in 1..8 {
        h.press(KeyCode::Right);
        assert_eq!(h.view_pages(), [expected]);
    }
    for expected in (0..7).rev() {
        h.press(KeyCode::Left);
        assert_eq!(h.view_pages(), [expected]);
    }
    h.press(KeyCode::End);
    assert_eq!(h.view_pages(), [7]);
    h.press(KeyCode::Home);
    assert_eq!(h.view_pages(), [0]);
    assert!(
        h.most_images <= 4,
        "single pages keep at most four: {}",
        h.most_images
    );
    h.press(KeyCode::Esc);
    assert!(h.ghostty.images.is_empty() && h.ghostty.placements.is_empty());
    eprintln!(
        "frames {} cells checked {}",
        h.frames,
        h.cells_checked.get()
    );
}

#[test]
fn spreads_right_to_left_and_left_to_right() {
    let mut h = Harness::new(&[portrait_chapter()], Via::Direct, (160, 45));
    h.settle();
    for _ in 0..3 {
        h.press(KeyCode::Right);
    }
    h.press(KeyCode::Char('s'));
    assert_eq!(h.view_pages(), [3, 4]);
    // Right to left: the later page on the left.
    let (left, right) = (h.last_rects[0], h.last_rects[1]);
    assert_eq!((left.page, right.page), (4, 3));
    let (p, u, _) = h.shown_at(left.area.x, left.area.y + left.area.height / 2);
    assert!(p == 4 && u < 0.05);
    let (p, u, _) = h.shown_at(right.area.x, right.area.y + right.area.height / 2);
    assert!(p == 3 && u < 0.05);

    h.press(KeyCode::Char('r'));
    assert_eq!(h.last_rects[0].page, 3, "left to right");
    h.press(KeyCode::Right);
    assert_eq!(h.view_pages(), [5, 6]);
    assert_eq!(h.last_rects[0].page, 5);
    h.press(KeyCode::Right);
    assert_eq!(h.view_pages(), [7]);
    h.press(KeyCode::Home);
    assert_eq!(h.view_pages(), [0]);
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('r'));
    assert_eq!(h.last_rects[0].page, 2, "right to left again");
    h.press(KeyCode::Char('s'));
    assert_eq!(h.view_pages(), [1]);
    assert!(h.most_images <= MAX_IMAGES);
}

#[test]
fn fit_width_scrolls_through_the_page() {
    let mut h = Harness::new(&[portrait_chapter()], Via::Direct, (100, 30));
    h.settle();
    h.press(KeyCode::Char('f'));
    let max = h.reader().reading.max_scroll;
    assert!(max > 20, "a portrait page is taller than the screen: {max}");
    let rect = h.rect(0);
    let (_, _, v_top) = h.shown_at(rect.area.x + 3, rect.area.y);
    assert!(v_top < 0.02);
    let mut last = 0;
    loop {
        h.press(KeyCode::Char('j'));
        let scroll = h.reader().reading.scroll;
        let rect = h.rect(0);
        assert_eq!(rect.row_offset, scroll);
        let (_, _, v) = h.shown_at(rect.area.x + 3, rect.area.y);
        let expected = (f64::from(scroll) + 0.5) / f64::from(rect.rows);
        assert!(
            (v - expected).abs() < 1.5 / f64::from(rect.rows),
            "{v} vs {expected}"
        );
        if scroll == last {
            break;
        }
        last = scroll;
    }
    assert_eq!(last, max);
    let rect = h.rect(0);
    let (_, _, v_bottom) = h.shown_at(rect.area.x + 3, rect.area.bottom() - 1);
    assert!(
        v_bottom > 0.98,
        "the bottom of the page is reachable: {v_bottom}"
    );
    h.press(KeyCode::Char('k'));
    assert_eq!(h.reader().reading.scroll, max - 3);
    // Turning resets the scroll.
    h.press(KeyCode::Right);
    assert_eq!(h.rect(1).row_offset, 0);
    // A wide page in fit width.
    h.press(KeyCode::Right);
    h.press(KeyCode::Right);
    assert_eq!(h.view_pages(), [3]);
    // Spreads in fit width scroll together.
    h.press(KeyCode::Char('s'));
    h.press(KeyCode::Right);
    assert_eq!(h.view_pages(), [5, 6]);
    for _ in 0..5 {
        h.press(KeyCode::Char('j'));
    }
    let max = h.reader().reading.max_scroll;
    assert!(max > 0);
    assert!(h.last_rects.iter().all(|r| r.row_offset == 15.min(max)));
    h.press(KeyCode::Char('f'));
    assert_eq!(h.reader().reading.scroll, 0);
}

#[test]
fn resizing_refits_and_resends() {
    let mut h = Harness::new(&[portrait_chapter()], Via::Direct, (120, 40));
    h.settle();
    h.press(KeyCode::Right);
    for (w, height) in [
        (200, 60),
        (80, 24),
        (30, 10),
        (300, 90),
        (12, 4),
        (40, 1),
        (1, 6),
        (2, 2),
        (120, 40),
    ] {
        h.resize(w, height);
    }
    h.press(KeyCode::Char('f'));
    for _ in 0..6 {
        h.press(KeyCode::Char('j'));
    }
    h.resize(160, 50);
    h.resize(90, 30);
    h.press(KeyCode::Char('s'));
    h.resize(250, 70);
    h.press(KeyCode::Char('f'));
    h.resize(100, 70);
    assert!(h.most_images <= MAX_IMAGES);
}

#[test]
fn end_of_chapter_popup_next_chapter_and_close() {
    let mut h = Harness::new(
        &[portrait_chapter(), short_chapter()],
        Via::Direct,
        (120, 40),
    );
    h.settle();
    h.press(KeyCode::End);
    assert!(h.key(KeyCode::Right).is_none());
    assert!(h.reader().end_prompt);
    h.settle();
    // Cells around the popup still show the right part of the page.
    let rect = h.rect(7);
    let mid = rect.area.y + rect.area.height / 2;
    let (p, u, _) = h.shown_at(rect.area.right() - 1, mid);
    assert!(p == 7 && u > 0.95);
    h.press(KeyCode::Esc);
    assert!(!h.reader().end_prompt);
    // The popup over a scrolled spread.
    h.press(KeyCode::Char('s'));
    h.press(KeyCode::Left);
    h.press(KeyCode::Char('f'));
    for _ in 0..4 {
        h.press(KeyCode::Char('j'));
    }
    h.press(KeyCode::Right);
    h.press(KeyCode::Right);
    assert!(h.reader().end_prompt);
    h.press(KeyCode::Esc);
    h.press(KeyCode::Char('f'));
    h.press(KeyCode::Char('s'));
    h.press(KeyCode::Right);
    assert!(h.reader().end_prompt);
    let old_ids: Vec<u32> = h.ghostty.images.keys().copied().collect();
    match h.key(KeyCode::Enter) {
        Some(Action::OpenReader { chapter: 1, .. }) => {}
        other => panic!("expected the next chapter, got {other:?}"),
    }
    h.settle();
    assert_eq!(h.reader().reading.chapter, 1);
    assert!(old_ids.iter().all(|id| !h.ghostty.images.contains_key(id)));
    h.press(KeyCode::Right);
    h.press(KeyCode::Right);
    h.press(KeyCode::Right);
    assert!(h.reader().end_prompt, "last downloaded chapter");
    h.press(KeyCode::Esc);
    assert!(matches!(h.key(KeyCode::Esc), Some(Action::CloseReader)));
    assert!(h.ghostty.images.is_empty() && h.ghostty.placements.is_empty());
}

#[test]
fn temporary_files_are_read_and_removed() {
    let mut h = Harness::new(&[portrait_chapter()], Via::TempFile, (120, 40));
    h.settle();
    for _ in 0..4 {
        h.press(KeyCode::Right);
    }
    h.press(KeyCode::Char('s'));
    h.resize(200, 60);
    h.press(KeyCode::Char('f'));
    h.press(KeyCode::Char('j'));
    h.press(KeyCode::End);
    h.press(KeyCode::Esc);
    assert_eq!(h.files_waiting(), 0);
}

#[test]
fn results_arriving_after_the_reader_moved_on() {
    for via in [Via::Direct, Via::TempFile] {
        let mut h = Harness::new(&[portrait_chapter(), portrait_chapter()], via, (120, 40));
        // Turn quickly while the first pages are still being prepared.
        for _ in 0..5 {
            h.key(KeyCode::Right);
        }
        h.finish_job();
        h.key(KeyCode::Char('s'));
        h.finish_job();
        h.key(KeyCode::Home);
        h.resize(150, 50);
        h.key(KeyCode::End);
        h.settle();
        // Next chapter while the old reader still has work out.
        h.key(KeyCode::Left);
        h.key(KeyCode::End);
        h.key(KeyCode::Right);
        assert!(h.reader().end_prompt);
        h.key(KeyCode::Enter);
        while !h.jobs.is_empty() {
            h.finish_job();
        }
        h.settle();
        assert_eq!(h.reader().reading.chapter, 1);
        // Close with jobs out; their results go nowhere and their files are removed.
        h.key(KeyCode::End);
        assert!(!h.jobs.is_empty());
        h.key(KeyCode::Esc);
        while !h.jobs.is_empty() {
            h.finish_job();
        }
        assert!(h.ghostty.images.is_empty());
        assert_eq!(h.files_waiting(), 0);
    }
}

#[test]
fn a_broken_page_shows_an_error_and_the_rest_still_work() {
    let chapter = vec![page(800, 1200), broken(), page(800, 1200), page(800, 1200)];
    let mut h = Harness::new(&[chapter], Via::Direct, (100, 30));
    h.settle();
    h.press(KeyCode::Right);
    assert!(crate::ui::buffer_text(&h.terminal).contains("Couldn't show this page"));
    h.press(KeyCode::Right);
    assert_eq!(h.view_pages(), [2]);
    h.press(KeyCode::Char('s'));
    assert_eq!(h.view_pages(), [1, 2]);
    assert!(crate::ui::buffer_text(&h.terminal).contains("Couldn't show this page"));
}

#[test]
fn tall_pages_are_limited_to_addressable_rows() {
    let chapter = vec![page(800, 1200), page(600, 9000), page(800, 1200)];
    let mut h = Harness::new(&[chapter], Via::Direct, (150, 40));
    h.settle();
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('f'));
    let rect = h.rect(1);
    assert_eq!(rect.rows, MAX_ROWS);
    for _ in 0..200 {
        h.key(KeyCode::Char('j'));
    }
    h.settle();
    let rect = h.rect(1);
    let (_, _, v) = h.shown_at(rect.area.x + 1, rect.area.bottom() - 1);
    assert!(v > 0.99);
}

/// A page that would be wider than the 297 columns the diacritics can address, with the
/// end-of-chapter popup over it. Cells past column 296 couldn't say where they are, so a run
/// Ghostty restarts right of the popup would show the page's top-left corner; pages are
/// therefore limited to 297 columns, and every cell after the popup is anchored.
#[test]
fn pages_wider_than_297_columns_with_the_popup() {
    let chapter = vec![page(800, 1200), page(6000, 1000)];
    let mut h = Harness::new(&[chapter], Via::Direct, (640, 60));
    h.settle();
    h.press(KeyCode::Right);
    let rect = h.rect(1);
    assert_eq!(rect.cols, MAX_COLUMNS);
    h.key(KeyCode::Right);
    h.settle();
}

/// Very wide pages in single, fit-width and spread layouts stay within 297 columns per page.
#[test]
fn pages_wider_than_297_columns() {
    let chapter = vec![page(800, 1200), page(6000, 1000), page(9000, 1000)];
    let mut h = Harness::new(&[chapter], Via::Direct, (640, 60));
    h.settle();
    h.press(KeyCode::Right);
    assert_eq!(h.rect(1).cols, MAX_COLUMNS);
    h.press(KeyCode::Char('f'));
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('s'));
}

#[test]
fn popup_over_a_spread_in_both_directions() {
    // Seven pages: the last view is the spread 6-7 (pages 5 and 6).
    let mut chapter = portrait_chapter();
    chapter.truncate(7);
    let mut h = Harness::with(
        &[chapter],
        Via::Direct,
        (150, 40),
        PageLayout::Spread,
        Fit::Page,
    );
    h.settle();
    h.press(KeyCode::End);
    assert_eq!(h.view_pages(), [5, 6]);
    h.press(KeyCode::Right);
    assert!(h.reader().end_prompt);
    // The popup sits across the seam; both pages around it are intact.
    let (left, right) = (h.last_rects[0], h.last_rects[1]);
    assert_eq!((left.page, right.page), (6, 5));
    let mid = left.area.y + left.area.height / 2;
    let (p, u, _) = h.shown_at(right.area.right() - 1, mid);
    assert!(p == 5 && u > 0.95);
    let (p, u, _) = h.shown_at(left.area.x, mid);
    assert!(p == 6 && u < 0.05);
    h.press(KeyCode::Char('r'));
    assert!(!h.reader().end_prompt, "any other key closes the popup");
    h.press(KeyCode::Right);
    assert!(h.reader().end_prompt);
    assert_eq!(h.last_rects[0].page, 5, "left to right");
    h.press(KeyCode::Char('f'));
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('j'));
    h.press(KeyCode::Right);
    h.resize(90, 25);
    h.resize(200, 60);
}

#[test]
fn huge_pages_are_capped_at_ghosttys_limit() {
    let chapter = vec![page(12000, 800), page(500, 15000), page(800, 1200)];
    let mut h = Harness::new(&[chapter], Via::Direct, (300, 80));
    h.settle();
    assert!(
        h.ghostty
            .images
            .values()
            .all(|i| i.width <= 10000 && i.height <= 10000)
    );
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('f'));
    for _ in 0..120 {
        h.key(KeyCode::Char('j'));
    }
    h.settle();
}

#[test]
fn reading_back_and_forth_stays_bounded() {
    let mut h = Harness::new(&[portrait_chapter()], Via::TempFile, (120, 40));
    h.settle();
    let sizes = [(120, 40), (180, 50), (90, 30)];
    for round in 0..6 {
        h.resize(sizes[round % 3].0, sizes[round % 3].1);
        for _ in 0..7 {
            h.key(KeyCode::Right);
            if round % 2 == 0 {
                h.settle();
            }
        }
        h.key(KeyCode::Char('s'));
        for _ in 0..4 {
            h.key(KeyCode::Left);
        }
        h.settle();
        h.key(KeyCode::Char('s'));
        if round == 3 {
            h.key(KeyCode::Char('f'));
        }
    }
    h.settle();
    assert!(h.most_images <= MAX_IMAGES, "{}", h.most_images);
    assert!(
        h.ghostty.total_bytes < 50_000_000,
        "{}",
        h.ghostty.total_bytes
    );
    h.press(KeyCode::Esc);
    assert_eq!(h.ghostty.total_bytes, 0);
    assert_eq!(h.files_waiting(), 0);
}

/// Webtoon pages: much taller than wide, two a little narrower than the rest and of awkward
/// sizes, so scaling them to the strip doesn't come out even.
fn strip_chapter() -> Vec<PageSpec> {
    vec![
        page(400, 1600),
        page(400, 1200),
        page(400, 2400),
        page(400, 900),
        page(383, 1517),
        page(397, 1601),
        page(400, 2000),
    ]
}

#[test]
fn long_strips_scroll_continuously_through_the_chapter() {
    let mut h = Harness::new(&[strip_chapter(), strip_chapter()], Via::Direct, (100, 30));
    assert!(
        h.reader().reading.strip.is_some(),
        "detected as a long strip"
    );
    h.settle();
    // A few rows at a time, across every page boundary, to the bottom.
    let mut last = 0;
    loop {
        h.press(KeyCode::Char('j'));
        let scroll = h.strip_scroll();
        if scroll == last {
            break;
        }
        assert_eq!(scroll, (last + 3).min(h.strip_max_scroll()));
        last = scroll;
    }
    assert_eq!(last, h.strip_max_scroll());
    assert!(
        h.reader().end_prompt,
        "going on past the end offers the next chapter"
    );
    assert_eq!(
        h.state
            .progress("Sim", "Chapter 1")
            .map(|p| (p.page, p.read)),
        Some((6, true))
    );
    h.press(KeyCode::Esc);
    // Back up a screen at a time, then jump around.
    h.press(KeyCode::PageUp);
    assert_eq!(
        h.strip_scroll(),
        last - 27,
        "a screen less two rows of overlap"
    );
    h.press(KeyCode::Home);
    assert_eq!((h.strip_scroll(), h.top_page()), (0, 0));
    h.press(KeyCode::Char(' '));
    h.press(KeyCode::Right);
    assert_eq!(h.strip_scroll(), 54);
    h.press(KeyCode::End);
    assert_eq!(h.strip_scroll(), h.strip_max_scroll());
    h.press(KeyCode::Char('s'));
    assert!(h.reader().notice.is_some(), "no spreads in a strip");
    h.press(KeyCode::Down);
    assert!(h.reader().end_prompt);
    match h.key(KeyCode::Enter) {
        Some(Action::OpenReader { chapter: 1, .. }) => {}
        other => panic!("expected the next chapter, got {other:?}"),
    }
    h.settle();
    assert!(h.reader().reading.strip.is_some());
    assert_eq!(h.strip_scroll(), 0);
    assert!(matches!(h.key(KeyCode::Esc), Some(Action::CloseReader)));
    assert!(h.ghostty.images.is_empty() && h.ghostty.placements.is_empty());
    assert!(h.cells_checked.get() > 100_000, "{}", h.cells_checked.get());
}

#[test]
fn tall_strip_pages_are_cut_into_tiles() {
    let chapter = vec![page(403, 16001), page(400, 1600), page(400, 1600)];
    let mut h = Harness::new(&[chapter], Via::TempFile, (100, 30));
    h.settle();
    let per_tile = u32::from(MAX_ROWS).min(10_000 / u32::from(h.cell.1));
    let rows = h
        .reader()
        .reading
        .strip
        .as_ref()
        .unwrap()
        .layout
        .as_ref()
        .unwrap()
        .rows(0);
    assert!(
        rows > per_tile,
        "the first page needs more than one image: {rows} rows"
    );
    // Every tile boundary passes the screen, a screen and then a row at a time, at the
    // pages' width and scaled up to the full width.
    for _ in 0..2 {
        while h.strip_scroll() < h.strip_max_scroll() {
            h.press(KeyCode::Char(' '));
            h.press(KeyCode::Char('j'));
        }
        h.press(KeyCode::Home);
        h.press(KeyCode::Char('f'));
    }
    h.press(KeyCode::Esc);
    assert_eq!(h.files_waiting(), 0);
}

#[test]
fn strips_keep_their_place_when_the_width_changes() {
    let mut h = Harness::new(&[strip_chapter()], Via::TempFile, (120, 40));
    h.settle();
    for _ in 0..5 {
        h.press(KeyCode::Char(' '));
    }
    let page = h.top_page();
    let offset = h.reader().reading.strip.as_ref().unwrap().offset;
    // Full width scales the pages up on the GPU; seams may only show at the sides.
    h.press(KeyCode::Char('f'));
    assert_eq!(
        h.reader()
            .reading
            .strip
            .as_ref()
            .unwrap()
            .layout
            .as_ref()
            .unwrap()
            .cols,
        120
    );
    assert_eq!(h.top_page(), page);
    for (width, height) in [(80, 30), (200, 50), (30, 12), (1, 5), (160, 45)] {
        h.resize(width, height);
        let strip = h.reader().reading.strip.as_ref().unwrap();
        if h.strip_scroll() < h.strip_max_scroll() {
            assert_eq!(strip.page, page, "same page at {width}x{height}");
            assert!((strip.offset - offset).abs() < 0.05);
        }
    }
    h.press(KeyCode::Char('j'));
    h.press(KeyCode::Char('f'));
    h.resize(100, 30);
    h.press(KeyCode::Esc);
    assert_eq!(h.files_waiting(), 0);
}

#[test]
fn switching_between_strip_and_pages() {
    let mut h = Harness::new(&[strip_chapter()], Via::Direct, (120, 40));
    h.settle();
    while h.top_page() < 2 {
        h.press(KeyCode::Char(' '));
    }
    let page = h.top_page();
    assert!(matches!(
        h.key(KeyCode::Char('w')),
        Some(Action::SetStrip { strip: false, .. })
    ));
    h.settle();
    assert_eq!(h.view_pages(), [page], "the same page, on its own");
    assert_eq!(h.state.strip("Sim"), Some(false));
    h.press(KeyCode::Right);
    h.press(KeyCode::Char('s'));
    let spread = h.view_pages();
    h.key(KeyCode::Char('w'));
    h.settle();
    assert_eq!(h.top_page(), spread[0]);
    // The choice sticks for the manga, over what the pages look like.
    h.key(KeyCode::Char('w'));
    h.press(KeyCode::Esc);
    let (reader, _) = ReaderScreen::new(
        h.book.manga.clone(),
        0,
        &h.state,
        &h.graphics,
        PageLayout::Single,
        Fit::Page,
    );
    assert!(reader.reading.strip.is_none());
}

#[test]
fn the_wheel_scrolls_strips_and_turns_pages() {
    let mut h = Harness::new(&[strip_chapter()], Via::Direct, (100, 30));
    h.settle();
    for _ in 0..5 {
        h.wheel(true);
    }
    h.settle();
    assert_eq!(h.strip_scroll(), 15);
    h.wheel(false);
    h.settle();
    assert_eq!(h.strip_scroll(), 12);
    h.key(KeyCode::Char('w'));
    h.settle();
    let page = h.view_pages()[0];
    h.wheel(true);
    h.wheel(true);
    h.settle();
    assert_eq!(h.view_pages(), [page + 1], "one flick turns one page");
}

#[test]
fn a_broken_strip_page_says_so_and_the_rest_still_show() {
    let chapter = vec![
        page(400, 1600),
        broken(),
        page(400, 1600),
        page(400, 1600),
        page(400, 1600),
    ];
    let mut h = Harness::new(&[chapter], Via::Direct, (100, 30));
    assert!(h.reader().reading.strip.is_some());
    h.settle();
    // The broken page takes a few rows after the first page's 80.
    for _ in 0..20 {
        h.press(KeyCode::Char('j'));
    }
    assert!(crate::ui::buffer_text(&h.terminal).contains("Couldn't show page 2"));
    h.press(KeyCode::End);
}

#[test]
fn strip_results_arriving_after_the_reader_moved_on() {
    for via in [Via::Direct, Via::TempFile] {
        let mut h = Harness::new(&[strip_chapter(), strip_chapter()], via, (100, 30));
        // Measuring and the first pages are still out when the reader starts moving.
        h.finish_job();
        for _ in 0..6 {
            h.key(KeyCode::Char(' '));
        }
        h.finish_job();
        h.key(KeyCode::Char('f'));
        h.finish_job();
        h.key(KeyCode::Char('w'));
        h.finish_job();
        h.key(KeyCode::Char('w'));
        h.resize(140, 40);
        h.key(KeyCode::End);
        h.key(KeyCode::Down);
        h.key(KeyCode::Enter);
        h.finish_job();
        h.key(KeyCode::End);
        assert!(!h.jobs.is_empty());
        h.key(KeyCode::Esc);
        while !h.jobs.is_empty() {
            h.finish_job();
        }
        assert!(h.ghostty.images.is_empty());
        assert_eq!(h.files_waiting(), 0);
    }
}

#[test]
fn full_width_strips_meet_without_gaps() {
    let mut h = Harness::new(&[strip_chapter()], Via::Direct, (100, 30));
    h.settle();
    h.press(KeyCode::Char('f'));
    // Every page boundary passes the screen with the pages scaled up on the GPU.
    while h.strip_scroll() < h.strip_max_scroll() {
        h.press(KeyCode::Char(' '));
        h.press(KeyCode::Char('j'));
    }
}
