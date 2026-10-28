//! Manga covers beside the text of the search, manga and library screens.
//!
//! Covers are fetched or read from disk and shrunk off the UI thread, then kept in memory for
//! the session. Before each frame the app tells [`Covers`] which cover the screen shows and
//! where; with the kitty protocol each cover is sent to the terminal once per size and drawn
//! with placeholder cells, and the terminal fits it into them, centred. Elsewhere
//! `ratatui-image` draws it (Sixel, iTerm2 or half-blocks).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use image::imageops::FilterType;
use image::{DynamicImage, RgbImage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect, Size};
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use super::kitty::{self, Medium, Payload, Placeholders};
use crate::api::{Client, Manga};
use crate::library::prepare::{Bounds, shrink_to};
use crate::library::{LibraryManga, LocalChapter, is_image, pages};

/// Columns per row of a cover: covers are about 0.7 times as wide as tall and cells about
/// twice as tall as wide. The terminal letterboxes whatever's left over.
const COLS_PER_ROW: f64 = 1.4;
/// The width of the MangaDex thumbnails shown for online manga.
const THUMBNAIL: u16 = 512;
/// The largest a cover is kept at; bigger cover files are shrunk to this once loaded.
const KEPT_SIZE: Bounds = Bounds {
    width: 512,
    height: Some(768),
};
/// Covers kept in memory, and images kept in the terminal.
const KEPT: usize = 24;
const SENT: usize = 6;

/// Where a cover comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CoverSource {
    /// A thumbnail of the manga's cover on MangaDex.
    Online { manga_id: String, file: String },
    /// A manga in the library: the cover image in its folder; failing that, for a folder linked
    /// to MangaDex, the cover from there, saved to the folder; else its first page.
    Local {
        dir: Option<PathBuf>,
        manga_id: Option<String>,
        first_chapter: Option<LocalChapter>,
    },
}

impl CoverSource {
    pub fn online(manga: &Manga) -> Option<CoverSource> {
        Some(CoverSource::Online {
            manga_id: manga.id.clone(),
            file: manga.cover_file.clone()?,
        })
    }

    pub fn local(manga: &LibraryManga) -> CoverSource {
        CoverSource::Local {
            dir: manga.dir.clone(),
            manga_id: manga.link.as_ref().map(|link| link.id.clone()),
            first_chapter: manga.chapters.first().cloned(),
        }
    }
}

/// The cells for a cover at most `max_rows` tall in `area`: at its top left, or centred
/// across it.
pub fn cover_rect(area: Rect, max_rows: u16, centred: bool) -> Rect {
    let mut rows = area.height.min(max_rows);
    let mut cols = (f64::from(rows) * COLS_PER_ROW).round() as u16;
    if cols > area.width {
        cols = area.width;
        rows = (f64::from(cols) / COLS_PER_ROW).round() as u16;
    }
    let x = if centred {
        area.x + (area.width - cols) / 2
    } else {
        area.x
    };
    Rect::new(x, area.y, cols, rows)
}

/// Gets a cover's pixels, shrunk to at most 512 x 768. Online covers are MangaDex's 512 px
/// thumbnails.
pub async fn load(client: &Client, source: CoverSource) -> Result<RgbImage, String> {
    match source {
        CoverSource::Online { manga_id, file } => {
            let bytes = client
                .fetch_cover_thumbnail(&manga_id, &file, THUMBNAIL)
                .await
                .map_err(|err| err.to_string())?;
            blocking(move || decode(&bytes)).await
        }
        CoverSource::Local {
            dir,
            manga_id,
            first_chapter,
        } => {
            let folder = dir.clone();
            if let Ok(image) = blocking(move || folder_cover(folder.as_deref())).await {
                return Ok(image);
            }
            if let (Some(dir), Some(id)) = (dir, manga_id)
                && let Ok(image) = fetch_and_save(client, &dir, &id).await
            {
                return Ok(image);
            }
            blocking(move || first_page(first_chapter.as_ref())).await
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| err.to_string())?
}

fn decode(bytes: &[u8]) -> Result<RgbImage, String> {
    let image = image::load_from_memory(bytes).map_err(|err| err.to_string())?;
    Ok(kept(image))
}

fn kept(image: DynamicImage) -> RgbImage {
    shrink_to(image, KEPT_SIZE).into_image().into_rgb8()
}

/// The `cover.*` image downloads save next to the chapters.
fn folder_cover(dir: Option<&Path>) -> Result<RgbImage, String> {
    let dir = dir.ok_or("not a folder")?;
    let path = fs::read_dir(dir)
        .map_err(|err| err.to_string())?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.file_stem().is_some_and(|stem| stem == "cover") && is_image(path))
        .ok_or("no cover in the folder")?;
    let image = image::open(&path).map_err(|err| err.to_string())?;
    Ok(kept(image))
}

/// Fetches the manga's cover from MangaDex and saves it as `cover.<ext>`, as downloads do.
async fn fetch_and_save(client: &Client, dir: &Path, manga_id: &str) -> Result<RgbImage, String> {
    let manga = client
        .manga(manga_id)
        .await
        .map_err(|err| err.to_string())?;
    let file = manga.cover_file.ok_or("no cover on MangaDex")?;
    let bytes = client
        .fetch_cover(manga_id, &file)
        .await
        .map_err(|err| err.to_string())?;
    let extension = Path::new(&file)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("jpg")
        .to_owned();
    let path = dir.join(format!("cover.{extension}"));
    blocking(move || {
        let image = decode(&bytes)?;
        // Best effort: the cover still shows when the folder can't be written to.
        if !path.exists() {
            let _ = fs::write(&path, &bytes);
        }
        Ok(image)
    })
    .await
}

fn first_page(chapter: Option<&LocalChapter>) -> Result<RgbImage, String> {
    let chapter = chapter.ok_or("no chapters")?;
    let first = pages::list(chapter)?.into_iter().next().ok_or("no pages")?;
    Ok(kept(pages::load(chapter, &first)?))
}

enum Loaded {
    Image(Arc<RgbImage>),
    Failed,
}

/// A cover held by the terminal at one size.
struct Sent {
    source: CoverSource,
    cells: (u16, u16),
    id: u32,
}

pub struct Covers {
    picker: Picker,
    kitty: Option<Medium>,
    /// Pixels per cell for kitty covers, checked again when the screen size changes.
    cell: (u16, u16),
    screen: Rect,
    /// Least recently shown first.
    loaded: Vec<(CoverSource, Loaded)>,
    loading: Option<CoverSource>,
    /// A cover to load, for the app to hand to a worker.
    job: Option<CoverSource>,
    /// What the frame being drawn shows, and where.
    shown: Option<(CoverSource, Rect)>,
    /// Least recently shown first.
    sent: Vec<Sent>,
    fallback: Option<(CoverSource, Rect, Protocol)>,
    output: Vec<u8>,
}

impl Covers {
    pub fn new(picker: Picker, kitty: Option<Medium>) -> Self {
        let font = picker.font_size();
        Covers {
            picker,
            kitty,
            cell: (font.width, font.height),
            screen: Rect::default(),
            loaded: Vec::new(),
            loading: None,
            job: None,
            shown: None,
            sent: Vec::new(),
            fallback: None,
            output: Vec::new(),
        }
    }

    /// Gets ready to show `wanted` in the next frame: starts loading it, or sends it to the
    /// terminal at its size.
    pub fn prepare(&mut self, screen: Rect, wanted: Option<(CoverSource, Rect)>) {
        if screen != self.screen {
            self.screen = screen;
            if self.kitty.is_some()
                && let Some(cell) = kitty::cell_size()
            {
                self.cell = cell;
            }
        }
        self.shown = wanted.clone();
        let Some((source, rect)) = wanted else {
            return;
        };
        let Some(index) = self.loaded.iter().position(|(known, _)| *known == source) else {
            if self.loading.is_none() {
                self.loading = Some(source.clone());
                self.job = Some(source);
            }
            return;
        };
        // Most recently shown last.
        let entry = self.loaded.remove(index);
        self.loaded.push(entry);
        let Some((_, Loaded::Image(image))) = self.loaded.last() else {
            return;
        };
        let image = image.clone();
        if self.kitty.is_some() {
            self.send(&source, rect, &image);
        } else {
            self.make_fallback(&source, rect, &image);
        }
    }

    /// Sends the cover at the size of `rect`, unless the terminal has it already.
    fn send(&mut self, source: &CoverSource, rect: Rect, image: &RgbImage) {
        let cells = (rect.width, rect.height);
        if let Some(index) = self
            .sent
            .iter()
            .position(|sent| sent.source == *source && sent.cells == cells)
        {
            let sent = self.sent.remove(index);
            self.sent.push(sent);
            return;
        }
        let bounds = Bounds {
            width: u32::from(cells.0) * u32::from(self.cell.0),
            height: Some(u32::from(cells.1) * u32::from(self.cell.1)),
        };
        let page = shrink_to(DynamicImage::ImageRgb8(image.clone()), bounds);
        let medium = self.kitty.as_ref().expect("only sent with kitty");
        let id = kitty::next_id();
        Payload::new(medium.temp_file(), page.rgb).transmit(
            &mut self.output,
            id,
            (page.width, page.height),
            cells,
        );
        self.sent.push(Sent {
            source: source.clone(),
            cells,
            id,
        });
        if self.sent.len() > SENT {
            let oldest = self.sent.remove(0);
            kitty::delete(&mut self.output, oldest.id);
        }
    }

    fn make_fallback(&mut self, source: &CoverSource, rect: Rect, image: &RgbImage) {
        let current = self
            .fallback
            .as_ref()
            .is_some_and(|(shown, area, _)| shown == source && area.as_size() == rect.as_size());
        if current {
            return;
        }
        let size = Size::new(rect.width, rect.height);
        let resize = Resize::Scale(Some(FilterType::Triangle));
        let image = DynamicImage::ImageRgb8(image.clone());
        self.fallback = self
            .picker
            .new_protocol(image, size, resize)
            .ok()
            .map(|protocol| (source.clone(), rect, protocol));
    }

    /// The cover to load next, for the app to pass to [`load`].
    pub fn take_job(&mut self) -> Option<CoverSource> {
        self.job.take()
    }

    pub fn on_loaded(&mut self, source: CoverSource, result: Result<RgbImage, String>) {
        if self.loading.as_ref() == Some(&source) {
            self.loading = None;
        }
        let loaded = match result {
            Ok(image) => Loaded::Image(Arc::new(image)),
            Err(_) => Loaded::Failed,
        };
        self.loaded.retain(|(known, _)| *known != source);
        self.loaded.push((source, loaded));
        if self.loaded.len() > KEPT {
            self.loaded.remove(0);
        }
    }

    /// Forgets the library's covers, so they're read again: one may have been added since.
    pub fn forget_local(&mut self) {
        let local = |source: &CoverSource| matches!(source, CoverSource::Local { .. });
        self.loaded.retain(|(source, _)| !local(source));
        let output = &mut self.output;
        self.sent.retain(|sent| {
            let keep = !local(&sent.source);
            if !keep {
                kitty::delete(output, sent.id);
            }
            keep
        });
        if self
            .fallback
            .as_ref()
            .is_some_and(|(source, _, _)| local(source))
        {
            self.fallback = None;
        }
    }

    /// Whether the frame shows a cover through placeholder cells, which need colours on.
    pub fn shows_placeholders(&self) -> bool {
        self.placed().is_some()
    }

    fn placed(&self) -> Option<(u32, Rect)> {
        let (source, rect) = self.shown.as_ref()?;
        let cells = (rect.width, rect.height);
        let sent = self
            .sent
            .iter()
            .find(|sent| sent.source == *source && sent.cells == cells)?;
        Some((sent.id, *rect))
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Frees the covers held by the terminal, for when the app quits.
    pub fn close(&mut self) -> Vec<u8> {
        for sent in self.sent.drain(..) {
            kitty::delete(&mut self.output, sent.id);
        }
        self.take_output()
    }

    /// Draws the cover the frame shows; nothing while it loads, a note when there's none.
    pub fn render(&self, frame: &mut Frame) {
        let Some((source, rect)) = &self.shown else {
            return;
        };
        if let Some((id, rect)) = self.placed() {
            frame.render_widget(Placeholders { id, row_offset: 0 }, rect);
            return;
        }
        if let Some((shown, area, protocol)) = &self.fallback
            && shown == source
            && area.as_size() == rect.as_size()
        {
            let size = protocol.size();
            let [area] = Layout::horizontal([Constraint::Length(size.width.min(rect.width))])
                .flex(Flex::Center)
                .areas(*rect);
            frame.render_widget(Image::new(protocol), area);
            return;
        }
        let failed = self
            .loaded
            .iter()
            .any(|(known, loaded)| known == source && matches!(loaded, Loaded::Failed));
        if failed {
            let line = Rect::new(rect.x, rect.y + rect.height / 2, rect.width, 1);
            frame.render_widget(Paragraph::new("No cover".dark_gray()).centered(), line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::ChapterKind;
    use crate::ui::buffer_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn online(id: &str) -> CoverSource {
        CoverSource::Online {
            manga_id: id.to_owned(),
            file: "cover.jpg".to_owned(),
        }
    }

    fn draw(covers: &Covers) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        terminal.draw(|frame| covers.render(frame)).unwrap();
        terminal
    }

    #[test]
    fn sizes_covers_by_rows_unless_too_narrow() {
        let area = Rect::new(2, 3, 40, 30);
        assert_eq!(cover_rect(area, 10, false), Rect::new(2, 3, 14, 10));
        assert_eq!(cover_rect(area, 10, true), Rect::new(15, 3, 14, 10));
        assert_eq!(
            cover_rect(Rect::new(0, 0, 7, 30), 20, false),
            Rect::new(0, 0, 7, 5)
        );
    }

    #[test]
    fn loads_one_cover_at_a_time_and_sends_each_size_once() {
        let shm = tempfile::tempdir().unwrap();
        let medium = Medium::TempFile(shm.path().to_owned());
        let mut covers = Covers::new(Picker::halfblocks(), Some(medium));
        covers.cell = (10, 20);
        let screen = Rect::new(0, 0, 30, 12);
        let rect = Rect::new(1, 1, 7, 5);

        covers.prepare(screen, Some((online("a"), rect)));
        assert_eq!(covers.take_job(), Some(online("a")));
        covers.prepare(screen, Some((online("b"), rect)));
        assert_eq!(covers.take_job(), None, "one at a time");
        assert!(!covers.shows_placeholders());

        covers.on_loaded(online("a"), Ok(RgbImage::new(512, 730)));
        covers.prepare(screen, Some((online("b"), rect)));
        assert_eq!(covers.take_job(), Some(online("b")));
        covers.prepare(screen, Some((online("a"), rect)));
        let output = String::from_utf8(covers.take_output()).unwrap();
        assert!(output.contains("a=T,U=1"), "{output}");
        assert!(output.contains("c=7,r=5"));
        // 7 x 5 cells of 10 x 20 px: 70 x 100 px at most, keeping the cover's shape.
        assert!(output.contains("s=70,v=100"), "{output}");
        assert!(covers.shows_placeholders());
        let terminal = draw(&covers);
        assert!(buffer_text(&terminal).contains('\u{10EEEE}'));

        covers.prepare(screen, Some((online("a"), rect)));
        assert!(covers.take_output().is_empty(), "already sent at that size");
        covers.prepare(screen, Some((online("a"), Rect::new(1, 1, 14, 10))));
        assert!(!covers.take_output().is_empty());

        covers.on_loaded(online("b"), Err("404".to_owned()));
        covers.prepare(screen, Some((online("b"), Rect::new(1, 1, 14, 10))));
        assert!(!covers.shows_placeholders());
        assert!(buffer_text(&draw(&covers)).contains("No cover"));

        let output = String::from_utf8(covers.close()).unwrap();
        assert_eq!(output.matches("a=d").count(), 2);
    }

    #[test]
    fn draws_half_blocks_without_kitty() {
        let mut covers = Covers::new(Picker::halfblocks(), None);
        let screen = Rect::new(0, 0, 30, 12);
        covers.prepare(screen, Some((online("a"), Rect::new(0, 0, 14, 10))));
        let mut image = RgbImage::new(512, 730);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb([200, 30, 30]);
        }
        covers.on_loaded(online("a"), Ok(image));
        covers.prepare(screen, Some((online("a"), Rect::new(0, 0, 14, 10))));
        assert!(covers.take_output().is_empty());
        let terminal = draw(&covers);
        let reds = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|cell| cell.fg == ratatui::style::Color::Rgb(200, 30, 30))
            .count();
        assert!(reds > 50, "{reds} red cells");
    }

    #[test]
    fn finds_a_local_cover_or_the_first_page() {
        let dir = tempfile::tempdir().unwrap();
        let chapter_dir = dir.path().join("Ch. 0001");
        fs::create_dir(&chapter_dir).unwrap();
        RgbImage::new(40, 60)
            .save(chapter_dir.join("001.png"))
            .unwrap();
        let chapter = LocalChapter {
            name: "Ch. 0001".to_owned(),
            file_name: "Ch. 0001".to_owned(),
            path: chapter_dir,
            kind: ChapterKind::Folder,
        };
        assert!(folder_cover(Some(dir.path())).is_err());
        assert_eq!(first_page(Some(&chapter)).unwrap().dimensions(), (40, 60));

        RgbImage::new(1024, 1536)
            .save(dir.path().join("cover.jpg"))
            .unwrap();
        let cover = folder_cover(Some(dir.path())).unwrap();
        assert_eq!(cover.dimensions(), (512, 768), "shrunk to what's kept");
    }
}
