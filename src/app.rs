//! Event loop: terminal input and API responses go in, screen updates and API requests come out.

use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;

use crate::api::{Chapter, ChapterSlot, Client, Manga, Quality, SearchPage};
use crate::download::{self, Downloader, Job, OutputFormat};
use crate::ui::download_dialog::DownloadDialog;
use crate::ui::downloads::DownloadsScreen;
use crate::ui::manga::MangaScreen;
use crate::ui::search::{SearchRequest, SearchScreen};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug)]
pub struct Settings {
    pub include_adult: bool,
    /// Preselected in the download dialog; updated to whatever was picked last.
    pub quality: Quality,
    pub format: OutputFormat,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            include_adult: false,
            quality: Quality::Data,
            format: OutputFormat::Cbz,
        }
    }
}

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
    CloseDialog,
    CloseDownloads,
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
}

pub struct App {
    client: Client,
    tx: UnboundedSender<Message>,
    settings: Settings,
    downloader: Downloader,
    next_job_id: u64,
    search: SearchScreen,
    /// Open on top of the search screen, which keeps its state for when the user goes back.
    manga: Option<MangaScreen>,
    downloads: DownloadsScreen,
    show_downloads: bool,
    dialog: Option<DownloadDialog>,
    /// Set after a first quit request while downloads are running.
    quit_armed: bool,
    running: bool,
    tick: usize,
}

pub async fn run(terminal: &mut DefaultTerminal, client: Client) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (update_tx, mut updates) = mpsc::unbounded_channel();
    let downloader = Downloader::spawn(client.clone(), download::default_root(), update_tx);
    let mut app = App {
        client,
        tx,
        settings: Settings::default(),
        downloads: DownloadsScreen::new(downloader.root()),
        downloader,
        next_job_id: 0,
        search: SearchScreen::default(),
        manga: None,
        show_downloads: false,
        dialog: None,
        quit_armed: false,
        running: true,
        tick: 0,
    };
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    while app.running {
        terminal.draw(|frame| app.render(frame))?;
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.handle_event(event),
                Some(Err(err)) => return Err(err),
                None => break,
            },
            Some(message) = rx.recv() => app.handle_message(message),
            Some(update) = updates.recv() => app.downloads.on_update(update),
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
        if self.show_downloads {
            self.downloads.render(frame, area, spinner);
        } else if let Some(manga) = &mut self.manga {
            manga.render(frame, area, spinner);
        } else {
            self.search.render(frame, area, &self.settings, spinner);
        }
        if let Some(dialog) = &self.dialog {
            dialog.render(frame, area);
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
        let action = if let Some(dialog) = &mut self.dialog {
            dialog.handle_key(key)
        } else if key.code == KeyCode::F(3) {
            self.show_downloads = !self.show_downloads;
            None
        } else if self.show_downloads {
            self.downloads.handle_key(key)
        } else if let Some(manga) = &mut self.manga {
            manga.handle_key(key)
        } else {
            self.search.handle_key(key)
        };
        if !matches!(action, Some(Action::Quit)) {
            self.quit_armed = false;
        }
        if let Some(action) = action {
            self.perform(action);
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
            }
        }
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Search(request) => self.spawn_search(request),
            Action::ToggleAdult => {
                self.settings.include_adult = !self.settings.include_adult;
                if let Some(action) = self.search.rerun() {
                    self.perform(action);
                }
            }
            Action::OpenManga(manga) => {
                let (screen, action) = MangaScreen::new(manga);
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
                    self.settings.quality,
                    self.settings.format,
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
                self.settings.quality = quality;
                self.settings.format = format;
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
            }
            Action::Enqueue(jobs) => {
                for job in jobs {
                    self.downloader.enqueue(job);
                }
            }
            Action::CloseDialog => self.dialog = None,
            Action::CloseDownloads => self.show_downloads = false,
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

    /// Shows a one-off message in the footer of whichever screen is visible.
    fn notify(&mut self, notice: String) {
        if self.show_downloads {
            self.downloads.set_notice(notice);
        } else if let Some(manga) = &mut self.manga {
            manga.set_notice(notice);
        } else {
            self.search.set_notice(notice);
        }
    }

    fn spawn_search(&self, request: SearchRequest) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let include_adult = self.settings.include_adult;
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

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}
