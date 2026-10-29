//! Event loop: terminal input and API responses go in, screen updates and API requests come out.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{execute, queue};
use futures_util::StreamExt;
use image::RgbImage;
use ratatui::layout::Rect;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::api::{Chapter, ChapterSlot, Client, Manga, Quality, SearchPage, Tag};
use crate::config::{Config, expand_home};
use crate::download::{self, Downloader, Job, OutputFormat, SavedQueue};
use crate::library::delete::{self, Deletion};
use crate::library::relocate::{self, MoveReport};
use crate::library::state::{Direction, ReadingState};
use crate::library::updates::{self, CheckEvent, Outcome};
use crate::library::{self, LibraryManga};
use crate::ui::cover::{self, CoverSource, Covers};
use crate::ui::delete_dialog::DeleteDialog;
use crate::ui::download_dialog::DownloadDialog;
use crate::ui::downloads::DownloadsScreen;
use crate::ui::library::LibraryScreen;
use crate::ui::manga::MangaScreen;
use crate::ui::reader::{Fit, Graphics, PageLayout, ReaderResult, ReaderScreen};
use crate::ui::search::{SearchRequest, SearchScreen};
use crate::ui::settings::{Change, MoveDialog, SettingsScreen};
use crate::ui::{display_path, format_size};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Turns on reports of mouse buttons and the wheel (not movement), in SGR encoding. Only while
/// the reader is open, so the rest of the app keeps the terminal's own text selection.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1000l";

/// Something a screen wants the app to do.
#[derive(Debug)]
pub enum Action {
    Search(SearchRequest),
    /// Fetch the tags the search filters offer.
    LoadTags,
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
        /// The manga folder to save them in.
        dir: PathBuf,
    },
    Enqueue(Vec<Job>),
    /// Stop these chapters downloading and delete the pages they saved.
    CancelDownloads(Vec<Job>),
    PauseDownloads(bool),
    /// Ask before deleting these from the library.
    ConfirmDelete(Deletion),
    Delete {
        deletion: Deletion,
        /// The space it frees, to say so afterwards.
        size: u64,
    },
    ChangeSetting(Change),
    /// Move these manga folders and loose chapters into the new download folder `to`.
    MoveLibrary {
        to: PathBuf,
        entries: Vec<PathBuf>,
    },
    /// Check the library for chapters released on MangaDex since they were downloaded.
    CheckForNew,
    /// Fetch a library manga's details from MangaDex, then do `then` with them.
    LookUpManga {
        id: String,
        then: LookUp,
    },
    /// Open a chapter from the online manga screen that's already on disk.
    ReadDownloaded {
        manga: Manga,
        chapter: Chapter,
    },
    OpenReader {
        manga: LibraryManga,
        chapter: usize,
        /// Open at the last page instead of where the user left off (going back a chapter).
        at_end: bool,
    },
    SaveProgress {
        manga: String,
        chapter: String,
        page: usize,
        finished: bool,
    },
    /// Mark these chapters (by file name) read or unread by hand.
    SetRead {
        manga: String,
        chapters: Vec<String>,
        read: bool,
    },
    SetDirection {
        manga: String,
        direction: Direction,
    },
    /// Remember whether this manga is read as a long strip.
    SetStrip {
        manga: String,
        strip: bool,
    },
    CloseReader,
    CloseDialog,
    /// Close the Library or Downloads screen.
    CloseOverlay,
    Back,
    Quit,
}

/// What to do with a library manga once its details arrive from MangaDex.
#[derive(Debug)]
pub enum LookUp {
    /// Ask how to download these new chapters into `dir`; `note` says which were left out.
    Download {
        dir: PathBuf,
        chapters: Vec<Chapter>,
        note: Option<String>,
    },
    /// Show its online manga screen in this language, saving downloads into `dir`.
    Open { dir: PathBuf, language: String },
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
    /// A reader job finished on a worker thread.
    Reader(Box<ReaderResult>),
    NewChapters {
        generation: u64,
        event: CheckEvent,
    },
    Manga {
        result: Result<Manga, String>,
        then: LookUp,
    },
    Deleted {
        deletion: Deletion,
        size: u64,
        result: Result<(), String>,
    },
    Tags(Result<Vec<Tag>, String>),
    Cover {
        source: CoverSource,
        result: Result<RgbImage, String>,
    },
    /// The library move started on entry `index` of `total`, called `name`.
    Moving {
        index: usize,
        total: usize,
        name: String,
    },
    Moved {
        to: PathBuf,
        report: MoveReport,
    },
}

/// A popup over the current screen, which gets the keys while it's open.
enum Dialog {
    Download(DownloadDialog),
    Delete(DeleteDialog),
    Move(MoveDialog),
}

/// A full-screen view shown over search and the manga screen, which keep their state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Library,
    Downloads,
    Settings,
}

pub struct App {
    client: Client,
    tx: UnboundedSender<Message>,
    config: Config,
    downloader: Downloader,
    next_job_id: u64,
    /// Where the download queue is kept between runs.
    queue_path: PathBuf,
    reading: ReadingState,
    graphics: Graphics,
    covers: Covers,
    search: SearchScreen,
    /// Open on top of the search screen, which keeps its state for when the user goes back.
    manga: Option<MangaScreen>,
    /// The library folder the manga screen was opened from: its downloads go there, and going
    /// back returns to the library.
    manga_from_library: Option<PathBuf>,
    downloads: DownloadsScreen,
    library: LibraryScreen,
    settings: SettingsScreen,
    /// Set while the library moves to a new download folder.
    moving: bool,
    overlay: Overlay,
    /// Downloads or Settings was opened from the library, so closing it goes back there.
    back_to_library: bool,
    dialog: Option<Dialog>,
    /// Drawn over everything else while open.
    reader: Option<ReaderScreen>,
    /// Layout and fit of the last closed reader, so the next one opens the same way.
    reader_preferences: (PageLayout, Fit),
    /// Escape sequences for the terminal (kitty images), written just before the next frame.
    terminal_output: Vec<u8>,
    /// The running check for new chapters, and which check is current.
    check_task: Option<JoinHandle<()>>,
    check_generation: u64,
    /// The current check was asked for with u, so "nothing new" is worth saying.
    check_requested: bool,
    /// The current check only re-links the library after its folder changed; its result
    /// isn't announced, so it doesn't hide what the move said.
    check_quiet: bool,
    running: bool,
    tick: usize,
}

/// Runs the app. `graphics` must be worked out before this starts reading terminal events,
/// since finding out what the terminal supports means reading its replies.
pub async fn run(
    terminal: &mut DefaultTerminal,
    client: Client,
    config: Config,
    graphics: Graphics,
) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (update_tx, mut updates) = mpsc::unbounded_channel();
    let root = config.settings.download_root();
    let config_path = config.path().to_owned();
    let downloader = Downloader::spawn(client.clone(), root.clone(), update_tx);
    let (reading, warning) = ReadingState::load(ReadingState::default_path());
    let mut app = App {
        client,
        tx,
        config,
        downloads: DownloadsScreen::new(downloader.root()),
        downloader,
        next_job_id: 0,
        queue_path: SavedQueue::default_path(),
        reading,
        covers: Covers::new(graphics.picker.clone(), graphics.kitty.clone()),
        graphics,
        search: SearchScreen::default(),
        manga: None,
        manga_from_library: None,
        library: LibraryScreen::new(root),
        settings: SettingsScreen::new(
            &config_path,
            std::env::var_os("TOSHOKAN_DIR").map(PathBuf::from),
        ),
        moving: false,
        overlay: Overlay::None,
        back_to_library: false,
        dialog: None,
        reader: None,
        reader_preferences: (PageLayout::default(), Fit::default()),
        terminal_output: Vec::new(),
        check_task: None,
        check_generation: 0,
        check_requested: false,
        check_quiet: false,
        running: true,
        tick: 0,
    };
    // The app opens on the library; Esc goes to search.
    app.open_library();
    if let Some(warning) = warning {
        app.notify(warning);
    }
    app.restore_queue();
    app.check_for_new(false);
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Placeholder cells carry kitty image ids in their colour, so colour output has to be on
    // while the reader is open even when NO_COLOR asks for none; everywhere else, honour it.
    let no_color = crossterm::style::Colored::ansi_color_disabled();
    while app.running {
        let size = terminal.size()?;
        app.prepare_frame(Rect::new(0, 0, size.width, size.height));
        let placeholders = app.reader.is_some() || app.covers.shows_placeholders();
        if no_color {
            crossterm::style::force_color_output(placeholders);
        }
        // Image commands and the cells that show them go out as one synchronized update, so
        // a page or cover never flashes at a stale size or position.
        let synchronize = placeholders || !app.terminal_output.is_empty();
        if synchronize {
            queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        }
        app.write_terminal_output(terminal)?;
        terminal.draw(|frame| app.render(frame))?;
        if synchronize {
            execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
        }
        app.start_reader_jobs();
        app.start_cover_job();
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
    // Free the reader's images in the terminal and give the mouse back before leaving.
    if let Some(mut reader) = app.reader.take() {
        app.terminal_output.extend(reader.close());
        app.terminal_output.extend(MOUSE_OFF);
    }
    app.terminal_output.extend(app.covers.close());
    app.write_terminal_output(terminal)
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
            Overlay::Library => self.library.render(frame, area, &self.reading, spinner),
            Overlay::Downloads => self.downloads.render(frame, area, spinner),
            Overlay::Settings => self
                .settings
                .render(frame, area, &self.config.settings, spinner),
            Overlay::None => match &mut self.manga {
                Some(manga) => manga.render(frame, area, spinner),
                None => self
                    .search
                    .render(frame, area, &self.config.settings, spinner),
            },
        }
        self.covers.render(frame);
        if self.overlay == Overlay::None && self.manga.is_none() {
            self.search.render_popup(frame, area);
        }
        match &self.dialog {
            Some(Dialog::Download(dialog)) => dialog.render(frame, area),
            Some(Dialog::Delete(dialog)) => dialog.render(frame, area),
            Some(Dialog::Move(dialog)) => dialog.render(frame, area),
            None => {}
        }
    }

    /// Lets the reader, or else the cover of the screen shown, update the images the terminal
    /// holds for the coming frame.
    fn prepare_frame(&mut self, area: Rect) {
        if let Some(reader) = &mut self.reader {
            reader.prepare_frame(area);
            self.terminal_output.extend(reader.take_output());
        }
        let wanted = match (&self.reader, self.overlay, &self.manga) {
            (Some(_), _, _) | (None, Overlay::Downloads | Overlay::Settings, _) => None,
            (None, Overlay::Library, _) => self.library.cover(area),
            (None, Overlay::None, Some(manga)) => manga.cover(area),
            (None, Overlay::None, None) => self.search.cover(area),
        };
        self.covers.prepare(area, wanted);
        self.terminal_output.extend(self.covers.take_output());
    }

    /// Writes the images' escape sequences (and anything else queued) to the terminal ahead
    /// of the frame that uses them.
    fn write_terminal_output(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        if self.terminal_output.is_empty() {
            return Ok(());
        }
        let backend = terminal.backend_mut();
        backend.write_all(&self.terminal_output)?;
        backend.flush()?;
        self.terminal_output.clear();
        Ok(())
    }

    /// Hands the reader's slow work (decoding, scaling) to worker threads.
    fn start_reader_jobs(&mut self) {
        let Some(reader) = &mut self.reader else {
            return;
        };
        for job in reader.take_requests() {
            let tx = self.tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(Message::Reader(Box::new(job.run())));
            });
        }
    }

    /// Loads the cover the screen waits for, off the UI thread.
    fn start_cover_job(&mut self) {
        let Some(source) = self.covers.take_job() else {
            return;
        };
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = cover::load(&client, source.clone()).await;
            let _ = tx.send(Message::Cover { source, result });
        });
    }

    fn is_busy(&self) -> bool {
        self.search.is_loading()
            || self.manga.as_ref().is_some_and(MangaScreen::is_loading)
            || self.downloads.is_downloading()
            || self.moving
            || (self.overlay == Overlay::Library && self.library.is_checking())
    }

    fn handle_event(&mut self, event: Event) {
        let key = match event {
            Event::Key(key) => key,
            Event::Mouse(mouse) => {
                let action = self
                    .reader
                    .as_mut()
                    .and_then(|reader| reader.handle_mouse(mouse));
                if let Some(action) = action {
                    self.perform(action);
                }
                return;
            }
            _ => return,
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
            match dialog {
                Dialog::Download(dialog) => dialog.handle_key(key),
                Dialog::Delete(dialog) => dialog.handle_key(key),
                Dialog::Move(dialog) => dialog.handle_key(key),
            }
        } else if key.code == KeyCode::F(1) {
            self.toggle_overlay(Overlay::Library);
            None
        } else if key.code == KeyCode::F(3) {
            self.toggle_overlay(Overlay::Downloads);
            None
        } else if key.code == KeyCode::F(5) {
            self.toggle_overlay(Overlay::Settings);
            None
        } else {
            match self.overlay {
                Overlay::Library => self.library.handle_key(key, &self.reading),
                Overlay::Downloads => self.downloads.handle_key(key),
                Overlay::Settings => self.settings.handle_key(key, &self.config.settings),
                Overlay::None => match &mut self.manga {
                    Some(manga) => manga.handle_key(key),
                    None => self.search.handle_key(key),
                },
            }
        };
        if let Some(action) = action {
            self.perform(action);
        }
    }

    fn toggle_overlay(&mut self, overlay: Overlay) {
        if self.overlay == overlay {
            self.close_overlay();
        } else if overlay == Overlay::Library {
            self.open_library();
        } else {
            self.back_to_library |= self.overlay == Overlay::Library;
            self.overlay = overlay;
        }
    }

    fn open_library(&mut self) {
        // Pick up chapters (and covers) downloaded or added by hand since it was last open.
        self.library.rescan(&self.reading);
        self.library.set_over_manga(self.manga.is_some());
        self.covers.forget_local();
        self.overlay = Overlay::Library;
        self.back_to_library = false;
    }

    /// Goes back to the library when Downloads or Settings was opened from it, else to the
    /// search or manga screen underneath.
    fn close_overlay(&mut self) {
        if self.overlay != Overlay::Library && self.back_to_library {
            self.open_library();
        } else {
            self.overlay = Overlay::None;
        }
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
            Message::Reader(result) => {
                // With no reader open the result is dropped, which also removes its page file.
                if let Some(reader) = &mut self.reader {
                    reader.on_job_done(*result);
                }
            }
            Message::NewChapters { generation, event } => {
                if generation == self.check_generation {
                    self.on_check_event(event);
                }
            }
            Message::Manga { result, then } => self.on_looked_up(result, then),
            Message::Deleted {
                deletion,
                size,
                result,
            } => self.on_deleted(&deletion, size, result),
            Message::Tags(result) => self.search.on_tags(result),
            Message::Cover { source, result } => self.covers.on_loaded(source, result),
            Message::Moving { index, total, name } => {
                self.settings.set_moving(Some((index, total, name)))
            }
            Message::Moved { to, report } => self.on_moved(&to, report),
        }
    }

    fn on_check_event(&mut self, event: CheckEvent) {
        match event {
            CheckEvent::Started { total, links } => {
                for (id, dir) in links {
                    self.downloader.link_folder(&id, dir);
                }
                self.library.check_started(total);
                if total == 0 {
                    self.check_finished();
                }
            }
            CheckEvent::Checked { key, dir, result } => {
                if let Ok(Outcome::Found(found)) = &result {
                    self.downloader.link_folder(&found.link.id, dir);
                }
                self.library.on_checked(key, result);
                if !self.library.is_checking() {
                    self.check_finished();
                }
            }
            CheckEvent::Stopped(reason) => {
                self.library.check_stopped();
                self.check_task = None;
                self.notify(format!("Stopped checking for new chapters: {reason}"));
            }
        }
    }

    fn check_finished(&mut self) {
        self.check_task = None;
        // The manga screen may show chapters of a folder that was just linked.
        self.refresh_downloaded();
        // Not worth interrupting reading for; the library shows it.
        if self.reader.is_some() || self.check_quiet {
            return;
        }
        let count = self.library.with_new_chapters();
        if count > 0 {
            let verb = if count == 1 { "has" } else { "have" };
            let hint = if self.overlay == Overlay::Library {
                "d downloads them"
            } else {
                "F1 shows the library"
            };
            self.notify(format!("{count} manga {verb} new chapters · {hint}"));
        } else if self.check_requested {
            self.notify("No new chapters on MangaDex".to_owned());
        }
    }

    fn on_looked_up(&mut self, result: Result<Manga, String>, then: LookUp) {
        self.library.clear_notice();
        // The user may have moved on while it loaded.
        if self.overlay != Overlay::Library || self.reader.is_some() || self.dialog.is_some() {
            return;
        }
        let manga = match result {
            Ok(manga) => manga,
            Err(err) => return self.notify(format!("Couldn't load the manga: {err}")),
        };
        match then {
            LookUp::Download {
                dir,
                chapters,
                note,
            } => self.open_download_dialog(manga, chapters, Some(dir), note),
            LookUp::Open { dir, language } => {
                self.overlay = Overlay::None;
                self.open_manga(manga, &language);
                self.manga_from_library = Some(dir);
            }
        }
    }

    /// Forgets what went, even when something else couldn't be deleted, and shows the
    /// library without it.
    fn on_deleted(&mut self, deletion: &Deletion, size: u64, result: Result<(), String>) {
        let manga = deletion.manga();
        for chapter in deletion.chapters() {
            if !chapter.path.exists() {
                self.reading.forget_chapter(&manga.key, &chapter.file_name);
            }
        }
        let manga_gone = matches!(deletion, Deletion::Manga(_))
            && manga.chapters.iter().all(|chapter| !chapter.path.exists());
        if manga_gone {
            self.reading.forget_manga(&manga.key);
            self.library.forget(&manga.key);
            if let Some(dir) = &manga.dir {
                self.downloader.forget_folder(dir);
            }
        }
        self.save_reading();
        self.library.refresh(&self.reading);
        self.refresh_downloaded();
        let size = format_size(size);
        let notice = match (result, deletion) {
            (Err(err), _) => format!("Couldn't delete everything: {err}"),
            (Ok(()), Deletion::Manga(manga)) => {
                let mut notice = format!("Deleted {} ({size})", manga.title);
                if manga.dir.as_ref().is_some_and(|dir| dir.exists()) {
                    notice.push_str(" · kept its folder, which holds other files");
                }
                notice
            }
            (Ok(()), Deletion::Chapters { manga, chapters }) => {
                let count = chapters.len();
                let plural = if count == 1 { "" } else { "s" };
                format!(
                    "Deleted {count} chapter{plural} of {} ({size})",
                    manga.title
                )
            }
        };
        self.notify(notice);
    }

    fn handle_update(&mut self, update: download::Update) {
        let finished = matches!(update.event, download::Event::Finished(_));
        let ended = !matches!(update.event, download::Event::Progress { .. });
        self.downloads.on_update(update);
        if ended {
            self.save_queue();
        }
        if finished {
            self.refresh_downloaded();
            if self.overlay == Overlay::Library {
                self.library.refresh(&self.reading);
            }
        }
    }

    /// Tells the manga screen which of its uploads are on disk and which have been read.
    fn refresh_downloaded(&mut self) {
        let Some(screen) = &mut self.manga else {
            return;
        };
        let manga = screen.manga();
        let manga_dir = match &self.manga_from_library {
            Some(dir) => dir.clone(),
            None => self.downloader.manga_dir(manga),
        };
        let manga_key = file_name(&manga_dir);
        let downloaded: HashMap<String, bool> = screen
            .uploads()
            .filter_map(|chapter| {
                let path = download::existing_path(&manga_dir, chapter)?;
                let progress = self.reading.progress(&manga_key, &file_name(&path));
                Some((chapter.id.clone(), progress.is_some_and(|p| p.read)))
            })
            .collect();
        screen.set_downloaded(downloaded);
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Search(request) => self.spawn_search(request),
            Action::LoadTags => {
                let client = self.client.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = client.tags().await.map_err(|err| err.to_string());
                    let _ = tx.send(Message::Tags(result));
                });
            }
            Action::ToggleAdult => {
                let settings = &mut self.config.settings;
                settings.include_adult = !settings.include_adult;
                self.save_settings();
                if let Some(action) = self.search.rerun() {
                    self.perform(action);
                }
            }
            Action::OpenManga(manga) => {
                let language = self.config.settings.language.clone();
                self.open_manga(manga, &language);
            }
            Action::LoadChapters { manga_id, language } => self.spawn_chapters(manga_id, language),
            Action::Download { manga, chapters } => {
                let dir = self.manga_from_library.clone();
                self.open_download_dialog(manga, chapters, dir, None)
            }
            Action::StartDownload {
                manga,
                chapters,
                quality,
                format,
                dir,
            } => {
                self.dialog = None;
                let count = chapters.len();
                for chapter in chapters {
                    self.next_job_id += 1;
                    let job = Job {
                        id: self.next_job_id,
                        dir: dir.clone(),
                        manga: manga.clone(),
                        chapter,
                        quality,
                        format,
                    };
                    self.downloads.add(job.clone());
                    self.downloader.enqueue(job);
                }
                self.save_queue();
                if self.overlay == Overlay::None
                    && let Some(screen) = &mut self.manga
                {
                    screen.clear_selection();
                }
                let plural = if count == 1 { "" } else { "s" };
                let downloads = if self.downloads.is_paused() {
                    "downloads are paused (p on F3 resumes)"
                } else {
                    "F3 shows downloads"
                };
                self.notify(format!("Queued {count} chapter{plural} · {downloads}"));
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
                self.save_queue();
            }
            Action::CancelDownloads(jobs) => {
                for job in &jobs {
                    self.downloader.cancel(job);
                }
                self.save_queue();
            }
            Action::PauseDownloads(paused) => {
                self.downloader.set_paused(paused);
                self.save_queue();
            }
            Action::ConfirmDelete(deletion) => {
                let manga = deletion.manga();
                let downloading = matches!(deletion, Deletion::Manga(_))
                    && manga
                        .dir
                        .as_ref()
                        .is_some_and(|dir| self.downloads.is_saving_into(dir));
                if downloading {
                    let title = manga.title.clone();
                    self.notify(format!(
                        "Chapters of {title} are downloading; cancel them on F3 first"
                    ));
                } else {
                    self.dialog = Some(Dialog::Delete(DeleteDialog::new(deletion)));
                }
            }
            Action::Delete { deletion, size } => {
                self.dialog = None;
                let root = self.downloader.root().to_owned();
                let tx = self.tx.clone();
                tokio::task::spawn_blocking(move || {
                    let result = delete::delete(&root, &deletion).map_err(|err| err.to_string());
                    let _ = tx.send(Message::Deleted {
                        deletion,
                        size,
                        result,
                    });
                });
            }
            Action::ChangeSetting(change) => self.change_setting(change),
            Action::MoveLibrary { to, entries } => {
                self.dialog = None;
                self.moving = true;
                let first = entries
                    .first()
                    .map(|path| file_name(path))
                    .unwrap_or_default();
                self.settings.set_moving(Some((0, entries.len(), first)));
                let tx = self.tx.clone();
                tokio::task::spawn_blocking(move || {
                    let total = entries.len();
                    let report = relocate::move_entries(&entries, &to, |index, name| {
                        let name = name.to_owned();
                        let _ = tx.send(Message::Moving { index, total, name });
                    });
                    let _ = tx.send(Message::Moved { to, report });
                });
            }
            Action::CheckForNew => self.check_for_new(true),
            Action::LookUpManga { id, then } => self.spawn_look_up(id, then),
            Action::ReadDownloaded { manga, chapter } => self.read_downloaded(&manga, &chapter),
            Action::OpenReader {
                manga,
                chapter,
                at_end,
            } => {
                let (layout, fit) = match &self.reader {
                    Some(reader) => reader.preferences(),
                    None => self.reader_preferences,
                };
                let (screen, action) = ReaderScreen::new(
                    manga,
                    chapter,
                    at_end,
                    &self.reading,
                    &self.graphics,
                    layout,
                    fit,
                );
                // Opening another chapter replaces the reader; free the old one's images.
                match self.reader.replace(screen) {
                    Some(mut old) => self.terminal_output.extend(old.close()),
                    None => self.terminal_output.extend(MOUSE_ON),
                }
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
            Action::SetRead {
                manga,
                chapters,
                read,
            } => {
                for chapter in &chapters {
                    self.reading.set_read(&manga, chapter, read);
                }
                self.save_reading();
                // The online manga screen underneath shows which chapters are read too.
                self.refresh_downloaded();
            }
            Action::SetDirection { manga, direction } => {
                self.reading.set_direction(&manga, direction);
                self.save_reading();
            }
            Action::SetStrip { manga, strip } => {
                self.reading.set_strip(&manga, strip);
                self.save_reading();
            }
            Action::CloseReader => {
                if let Some(mut reader) = self.reader.take() {
                    self.terminal_output.extend(reader.close());
                    self.terminal_output.extend(MOUSE_OFF);
                    self.reader_preferences = reader.preferences();
                    let (manga, chapter) = reader.position();
                    self.library.show_chapter(manga, chapter);
                }
                self.refresh_downloaded();
            }
            Action::CloseDialog => self.dialog = None,
            Action::CloseOverlay => self.close_overlay(),
            Action::Back => {
                self.manga = None;
                if self.manga_from_library.take().is_some() {
                    self.open_library();
                }
            }
            // Stopping halfway through a copy would leave a manga half in each folder.
            Action::Quit if self.moving => self.notify(
                "The library is still moving to the new folder; quit once it's done".to_owned(),
            ),
            // Nothing is lost: the queue is saved and carries on next time.
            Action::Quit => self.running = false,
        }
    }

    fn open_manga(&mut self, manga: Manga, language: &str) {
        let (screen, action) = MangaScreen::new(manga, language);
        self.manga = Some(screen);
        self.manga_from_library = None;
        if let Some(action) = action {
            self.perform(action);
        }
    }

    /// Asks how to download `chapters` into `dir`, or the manga's usual folder.
    fn open_download_dialog(
        &mut self,
        manga: Manga,
        chapters: Vec<Chapter>,
        dir: Option<PathBuf>,
        note: Option<String>,
    ) {
        let dir = dir.unwrap_or_else(|| self.downloader.manga_dir(&manga));
        let dialog = DownloadDialog::new(
            manga,
            chapters,
            self.config.settings.quality,
            self.config.settings.format,
            &dir,
        );
        self.dialog = Some(Dialog::Download(dialog.with_note(note)));
    }

    /// Checks the library in a new folder, which links its manga for downloads, without
    /// announcing what it finds.
    fn relink_library(&mut self) {
        self.check_for_new(false);
        self.check_quiet = true;
    }

    /// Starts checking the library for new chapters in the background, replacing any check
    /// still running.
    fn check_for_new(&mut self, requested: bool) {
        if let Some(task) = self.check_task.take() {
            task.abort();
        }
        self.check_generation += 1;
        self.check_requested = requested;
        self.check_quiet = false;
        let generation = self.check_generation;
        let client = self.client.clone();
        let tx = self.tx.clone();
        let root = self.downloader.root().to_owned();
        let language = self.config.settings.language.clone();
        let include_adult = self.config.settings.include_adult;
        self.check_task = Some(tokio::spawn(async move {
            updates::check_library(&client, root, &language, include_adult, |event| {
                let _ = tx.send(Message::NewChapters { generation, event });
            })
            .await;
        }));
    }

    fn spawn_look_up(&self, id: String, then: LookUp) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = client.manga(&id).await.map_err(|err| err.to_string());
            let _ = tx.send(Message::Manga { result, then });
        });
    }

    fn read_downloaded(&mut self, manga: &Manga, chapter: &Chapter) {
        let dir = match &self.manga_from_library {
            Some(dir) => dir.clone(),
            None => self.downloader.manga_dir(manga),
        };
        let Some(path) = download::existing_path(&dir, chapter) else {
            self.notify("That chapter isn't on disk anymore".to_owned());
            return;
        };
        match library::scan_manga(&dir) {
            Ok(local) => match local.chapters.iter().position(|c| c.path == path) {
                Some(index) => self.perform(Action::OpenReader {
                    manga: local,
                    chapter: index,
                    at_end: false,
                }),
                None => self.notify("That chapter isn't on disk anymore".to_owned()),
            },
            Err(err) => self.notify(format!("Couldn't read the manga folder: {err}")),
        }
    }

    fn change_setting(&mut self, change: Change) {
        let settings = &mut self.config.settings;
        match change {
            Change::Language(code) => settings.language = code,
            Change::Quality(quality) => settings.quality = quality,
            Change::Format(format) => settings.format = format,
            Change::DownloadDir(dir) => return self.change_download_dir(dir),
        }
        self.save_settings();
    }

    /// Switches to another download folder and offers to move the library there. Not while
    /// chapters are downloading into the current one, or the library is moving.
    fn change_download_dir(&mut self, dir: String) {
        if self.moving {
            return self.notify("Wait for the library to finish moving".to_owned());
        }
        let active = self.downloads.active_count();
        if active > 0 {
            let plural = if active == 1 { "" } else { "s" };
            return self.notify(format!(
                "{active} chapter{plural} still to download into the current folder; let them \
                 finish or cancel them on F3 first"
            ));
        }
        let old = self.downloader.root().to_owned();
        let new = expand_home(&dir);
        self.config.settings.download_dir = dir;
        self.save_settings();
        if new == old {
            return;
        }
        self.downloader.set_root(new.clone());
        self.library.set_root(new.clone(), &self.reading);
        self.downloads.set_root(&old, &new);
        self.save_queue();
        self.covers.forget_local();
        self.refresh_downloaded();
        self.relink_library();
        match relocate::movable(&old) {
            Ok(entries) if !entries.is_empty() => {
                self.dialog = Some(Dialog::Move(MoveDialog::new(old, new, entries)));
            }
            _ => self.notify(format!("Chapters now go to {}", display_path(&new))),
        }
    }

    fn on_moved(&mut self, to: &std::path::Path, report: MoveReport) {
        self.moving = false;
        self.settings.set_moving(None);
        self.library.rescan(&self.reading);
        self.covers.forget_local();
        self.refresh_downloaded();
        self.relink_library();
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        let mut notice = format!(
            "Moved {} manga folder{} or chapter{} to {}",
            report.moved,
            plural(report.moved),
            plural(report.moved),
            display_path(to)
        );
        if !report.skipped.is_empty() {
            notice.push_str(&format!(
                " · left {} the new folder already had: {}",
                report.skipped.len(),
                report.skipped.join(", ")
            ));
        }
        if let Some((name, err)) = report.failed.first() {
            notice.push_str(&format!(
                " · {} couldn't be moved and stayed (first: {name}: {err})",
                report.failed.len()
            ));
        }
        self.notify(notice);
    }

    fn save_settings(&mut self) {
        if let Err(err) = self.config.save() {
            let path = display_path(self.config.path());
            self.notify(format!("Couldn't save settings to {path}: {err}"));
        }
    }

    /// Puts back the chapters the last run left queued or failed. Queued ones start again
    /// unless downloads were paused.
    fn restore_queue(&mut self) {
        let (saved, warning) = SavedQueue::load(&self.queue_path);
        if let Some(warning) = warning {
            self.notify(warning);
        }
        self.downloads.set_paused(saved.paused);
        self.downloader.set_paused(saved.paused);
        let (mut queued, mut failed) = (0, 0);
        for chapter in saved.chapters {
            self.next_job_id += 1;
            let (job, error) = chapter.into_job(self.next_job_id);
            if error.is_some() {
                failed += 1;
            } else {
                queued += 1;
                self.downloader.enqueue(job.clone());
            }
            self.downloads.restore(job, error);
        }
        let plural = |count| if count == 1 { "" } else { "s" };
        let notice = match (queued, failed) {
            (0, 0) => return,
            (0, _) => format!(
                "{failed} chapter{} failed to download last time · F3 to retry",
                plural(failed)
            ),
            _ if saved.paused => format!(
                "{queued} chapter download{} paused · F3 shows downloads",
                plural(queued)
            ),
            _ => format!(
                "Resuming {queued} chapter download{} · F3 shows downloads",
                plural(queued)
            ),
        };
        self.notify(notice);
    }

    fn save_queue(&mut self) {
        if let Err(err) = self.downloads.saved().save(&self.queue_path) {
            let path = display_path(&self.queue_path);
            self.notify(format!("Couldn't save the download queue to {path}: {err}"));
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
            Overlay::Settings => self.settings.set_notice(notice),
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
                .search_manga(
                    &request.query,
                    &request.filters,
                    request.offset,
                    include_adult,
                )
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
