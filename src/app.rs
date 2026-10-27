//! Event loop: terminal input and API responses go in, screen updates and API requests come out.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, Frame};
use ratatui_image::picker::Picker;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;

use crate::api::{Chapter, ChapterSlot, Client, Manga, Quality, SearchPage};
use crate::config::Config;
use crate::download::{self, Downloader, Job, OutputFormat};
use crate::library::state::{Direction, ReadingState};
use crate::library::{self, LibraryManga};
use crate::ui::display_path;
use crate::ui::download_dialog::DownloadDialog;
use crate::ui::downloads::DownloadsScreen;
use crate::ui::library::LibraryScreen;
use crate::ui::manga::MangaScreen;
use crate::ui::reader::{self, Fit, PageLayout, ReaderScreen, Rendered};
use crate::ui::search::{SearchRequest, SearchScreen};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Something a screen wants the app to do.
#[derive(Debug)]
pub enum Action {
    Search(SearchRequest),
    ToggleAdult,
    OpenManga(Manga),
    LoadChapters {
        manga_id: String,
        language: String,
    },
    /// Ask how to download these chapters.
    Download {
        manga: Manga,
        chapters: Vec<Chapter>,
    },
    StartDownload {
        manga: Manga,
        chapters: Vec<Chapter>,
        quality: Quality,
        format: OutputFormat,
    },
    Enqueue(Vec<Job>),
    /// Open a chapter from the online manga screen that's already on disk.
    ReadDownloaded {
        manga: Manga,
        chapter: Chapter,
    },
    OpenReader {
        manga: LibraryManga,
        chapter: usize,
    },
    SaveProgress {
        manga: String,
        chapter: String,
        page: usize,
        finished: bool,
    },
    SetDirection {
        manga: String,
        direction: Direction,
    },
    CloseReader,
    CloseDialog,
    /// Close the Library or Downloads screen.
    CloseOverlay,
    Back,
    Quit,
}

/// Results of background work, delivered back to the event loop.
#[derive(Debug)]
pub enum Message {
    SearchResults {
        generation: u64,
        result: Result<SearchPage, String>,
    },
    Chapters {
        manga_id: String,
        language: String,
        result: Result<Vec<ChapterSlot>, String>,
    },
    PageRendered(Box<Rendered>),
}

/// A full-screen view shown over search and the manga screen, which keep their state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Library,
    Downloads,
}

pub struct App {
    client: Client,
    tx: UnboundedSender<Message>,
    config: Config,
    downloader: Downloader,
    next_job_id: u64,
    reading: ReadingState,
    picker: Picker,
    search: SearchScreen,
    /// Open on top of the search screen, which keeps its state for when the user goes back.
    manga: Option<MangaScreen>,
    downloads: DownloadsScreen,
    library: LibraryScreen,
    overlay: Overlay,
    dialog: Option<DownloadDialog>,
    /// Drawn over everything else while open.
    reader: Option<ReaderScreen>,
    /// Layout and fit of the last closed reader, so the next one opens the same way.
    reader_preferences: (PageLayout, Fit),
    /// Set after a first quit request while downloads are running.
    quit_armed: bool,
    running: bool,
    tick: usize,
}

/// Runs the app. `picker` must be created before this starts reading terminal events, since
/// it queries the terminal's graphics support.
pub async fn run(
    terminal: &mut DefaultTerminal,
    client: Client,
    config: Config,
    picker: Picker,
) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (update_tx, mut updates) = mpsc::unbounded_channel();
    let root = config.settings.download_root();
    let downloader = Downloader::spawn(client.clone(), root.clone(), update_tx);
    let (reading, warning) = ReadingState::load(ReadingState::default_path());
    let mut app = App {
        client,
        tx,
        config,
        downloads: DownloadsScreen::new(downloader.root()),
        downloader,
        next_job_id: 0,
        reading,
        picker,
        search: SearchScreen::default(),
        manga: None,
        library: LibraryScreen::new(root),
        overlay: Overlay::None,
        dialog: None,
        reader: None,
        reader_preferences: (PageLayout::default(), Fit::default()),
        quit_armed: false,
        running: true,
        tick: 0,
    };
    if let Some(warning) = warning {
        app.notify(warning);
    }
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while app.running {
        terminal.draw(|frame| app.render(frame))?;
        app.start_render_jobs();
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.handle_event(event),
                Some(Err(err)) => return Err(err),
                None => break,
            },
            Some(message) = rx.recv() => app.handle_message(message),
            Some(update) = updates.recv() => app.handle_update(update),
            // Only wake up on a timer while a spinner is visible.
            _ = ticker.tick(), if app.is_busy() => app.tick = app.tick.wrapping_add(1),
        }
    }
    Ok(())
}

impl App {
    fn render(&mut self, frame: &mut Frame) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let area = frame.area();
        if let Some(reader) = &mut self.reader {
            reader.render(frame, area);
            return;
        }
        match self.overlay {
            Overlay::Library => self.library.render(frame, area, &self.reading),
            Overlay::Downloads => self.downloads.render(frame, area, spinner),
            Overlay::None => match &mut self.manga {
                Some(manga) => manga.render(frame, area, spinner),
                None => self
                    .search
                    .render(frame, area, &self.config.settings, spinner),
            },
        }
        if let Some(dialog) = &self.dialog {
            dialog.render(frame, area);
        }
    }

    /// Hands page decoding requested by the reader's last render to background threads.
    fn start_render_jobs(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        for job in reader.take_requests() {
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let rendered = reader::render_job(job);
                let _ = tx.send(Message::PageRendered(Box::new(rendered)));
            });
        }
    }

    fn is_busy(&self) -> bool {
        self.search.is_loading()
            || self.manga.as_ref().is_some_and(MangaScreen::is_loading)
            || self.downloads.active_count() > 0
    }

    fn handle_event(&mut self, event: Event) {
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if is_ctrl_c(key) {
            self.running = false;
            return;
        }
        let action = if let Some(reader) = &mut self.reader {
            reader.handle_key(key)
        } else if let Some(dialog) = &mut self.dialog {
            dialog.handle_key(key)
        } else if key.code == KeyCode::F(1) {
            self.toggle_overlay(Overlay::Library);
            None
        } else if key.code == KeyCode::F(3) {
            self.toggle_overlay(Overlay::Downloads);
            None
        } else {
            match self.overlay {
                Overlay::Library => self.library.handle_key(key, &self.reading),
                Overlay::Downloads => self.downloads.handle_key(key),
                Overlay::None => match &mut self.manga {
                    Some(manga) => manga.handle_key(key),
                    None => self.search.handle_key(key),
                },
            }
        };
        if !matches!(action, Some(Action::Quit)) {
            self.quit_armed = false;
        }
        if let Some(action) = action {
            self.perform(action);
        }
    }

    fn toggle_overlay(&mut self, overlay: Overlay) {
        if self.overlay == overlay {
            self.overlay = Overlay::None;
            return;
        }
        if overlay == Overlay::Library {
            // Pick up chapters downloaded or added by hand since it was last open.
            self.library.rescan(&self.reading);
        }
        self.overlay = overlay;
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::SearchResults { generation, result } => {
                self.search.on_results(generation, result)
            }
            Message::Chapters {
                manga_id,
                language,
                result,
            } => {
                if let Some(manga) = &mut self.manga {
                    manga.on_chapters(&manga_id, language, result);
                }
                self.refresh_downloaded();
            }
            Message::PageRendered(rendered) => {
                if let Some(reader) = &mut self.reader {
                    reader.on_rendered(*rendered);
                }
            }
        }
    }

    fn handle_update(&mut self, update: download::Update) {
        let finished = matches!(update.event, download::Event::Finished(_));
        self.downloads.on_update(update);
        if finished {
            self.refresh_downloaded();
        }
    }

    /// Tells the manga screen which of its uploads are on disk and which have been read.
    fn refresh_downloaded(&mut self) {
        let Some(screen) = &mut self.manga else {
            return;
        };
        let manga = screen.manga();
        let manga_dir = self.downloader.manga_dir(manga);
        let manga_key = file_name(&manga_dir);
        let downloaded: HashMap<String, bool> = screen
            .uploads()
            .filter_map(|chapter| {
                let path = self.downloader.existing_path(manga, chapter)?;
                let progress = self.reading.progress(&manga_key, &file_name(&path));
                Some((chapter.id.clone(), progress.is_some_and(|p| p.read)))
            })
            .collect();
        screen.set_downloaded(downloaded);
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Search(request) => self.spawn_search(request),
            Action::ToggleAdult => {
                let settings = &mut self.config.settings;
                settings.include_adult = !settings.include_adult;
                self.save_settings();
                if let Some(action) = self.search.rerun() {
                    self.perform(action);
                }
            }
            Action::OpenManga(manga) => {
                let (screen, action) = MangaScreen::new(manga, &self.config.settings.language);
                self.manga = Some(screen);
                if let Some(action) = action {
                    self.perform(action);
                }
            }
            Action::LoadChapters { manga_id, language } => self.spawn_chapters(manga_id, language),
            Action::Download { manga, chapters } => {
                let destination = self.downloader.manga_dir(&manga);
                self.dialog = Some(DownloadDialog::new(
                    manga,
                    chapters,
                    self.config.settings.quality,
                    self.config.settings.format,
                    &destination,
                ));
            }
            Action::StartDownload {
                manga,
                chapters,
                quality,
                format,
            } => {
                self.dialog = None;
                let count = chapters.len();
                for chapter in chapters {
                    self.next_job_id += 1;
                    let job = Job {
                        id: self.next_job_id,
                        manga: manga.clone(),
                        chapter,
                        quality,
                        format,
                    };
                    self.downloads.add(job.clone());
                    self.downloader.enqueue(job);
                }
                if let Some(screen) = &mut self.manga {
                    screen.clear_selection();
                }
                let plural = if count == 1 { "" } else { "s" };
                self.notify(format!(
                    "Queued {count} chapter{plural} · F3 shows downloads"
                ));
                // Remember the choices so the next dialog starts with them.
                let settings = &mut self.config.settings;
                if (settings.quality, settings.format) != (quality, format) {
                    settings.quality = quality;
                    settings.format = format;
                    self.save_settings();
                }
            }
            Action::Enqueue(jobs) => {
                for job in jobs {
                    self.downloader.enqueue(job);
                }
            }
            Action::ReadDownloaded { manga, chapter } => self.read_downloaded(&manga, &chapter),
            Action::OpenReader { manga, chapter } => {
                let (layout, fit) = match &self.reader {
                    Some(reader) => reader.preferences(),
                    None => self.reader_preferences,
                };
                let (screen, action) = ReaderScreen::new(
                    manga,
                    chapter,
                    &self.reading,
                    self.picker.clone(),
                    layout,
                    fit,
                );
                self.reader = Some(screen);
                if let Some(action) = action {
                    self.perform(action);
                }
            }
            Action::SaveProgress {
                manga,
                chapter,
                page,
                finished,
            } => {
                self.reading.record(&manga, &chapter, page, finished);
                self.save_reading();
            }
            Action::SetDirection { manga, direction } => {
                self.reading.set_direction(&manga, direction);
                self.save_reading();
            }
            Action::CloseReader => {
                if let Some(reader) = self.reader.take() {
                    self.reader_preferences = reader.preferences();
                    let (manga, chapter) = reader.position();
                    self.library.show_chapter(manga, chapter);
                }
                self.refresh_downloaded();
            }
            Action::CloseDialog => self.dialog = None,
            Action::CloseOverlay => self.overlay = Overlay::None,
            Action::Back => self.manga = None,
            Action::Quit => {
                let active = self.downloads.active_count();
                if active > 0 && !self.quit_armed {
                    self.quit_armed = true;
                    let plural = if active == 1 { "" } else { "s" };
                    self.notify(format!(
                        "{active} chapter{plural} still downloading. Press q again to quit; \
                         partial chapters resume when downloaded again."
                    ));
                } else {
                    self.running = false;
                }
            }
        }
    }

    fn read_downloaded(&mut self, manga: &Manga, chapter: &Chapter) {
        let Some(path) = self.downloader.existing_path(manga, chapter) else {
            self.notify("That chapter isn't on disk anymore".to_owned());
            return;
        };
        match library::scan_manga(&self.downloader.manga_dir(manga)) {
            Ok(local) => match local.chapters.iter().position(|c| c.path == path) {
                Some(index) => self.perform(Action::OpenReader {
                    manga: local,
                    chapter: index,
                }),
                None => self.notify("That chapter isn't on disk anymore".to_owned()),
            },
            Err(err) => self.notify(format!("Couldn't read the manga folder: {err}")),
        }
    }

    fn save_settings(&mut self) {
        if let Err(err) = self.config.save() {
            let path = display_path(self.config.path());
            self.notify(format!("Couldn't save settings to {path}: {err}"));
        }
    }

    fn save_reading(&mut self) {
        if let Err(err) = self.reading.save() {
            let path = display_path(self.reading.path());
            self.notify(format!("Couldn't save reading progress to {path}: {err}"));
        }
    }

    /// Shows a one-off message in the footer of whichever screen is visible.
    fn notify(&mut self, notice: String) {
        if let Some(reader) = &mut self.reader {
            reader.set_notice(notice);
            return;
        }
        match self.overlay {
            Overlay::Library => self.library.set_notice(notice),
            Overlay::Downloads => self.downloads.set_notice(notice),
            Overlay::None => match &mut self.manga {
                Some(manga) => manga.set_notice(notice),
                None => self.search.set_notice(notice),
            },
        }
    }

    fn spawn_search(&self, request: SearchRequest) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let include_adult = self.config.settings.include_adult;
        tokio::spawn(async move {
            let result = client
                .search_manga(&request.query, request.offset, include_adult)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(Message::SearchResults {
                generation: request.generation,
                result,
            });
        });
    }

    fn spawn_chapters(&self, manga_id: String, language: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client
                .chapters(&manga_id, &language)
                .await
                .map_err(|err| err.to_string());
            let _ = tx.send(Message::Chapters {
                manga_id,
                language,
                result,
            });
        });
    }
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}
