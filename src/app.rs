//! Event loop: terminal input and API responses go in, screen updates and API requests come out.

use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;

use crate::api::{Chapter, ChapterSlot, Client, Manga, SearchPage};
use crate::ui::manga::MangaScreen;
use crate::ui::search::{SearchRequest, SearchScreen};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Debug, Default)]
pub struct Settings {
    pub include_adult: bool,
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
    Download {
        manga: Manga,
        chapters: Vec<Chapter>,
    },
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
    search: SearchScreen,
    /// Open on top of the search screen, which keeps its state for when the user goes back.
    manga: Option<MangaScreen>,
    running: bool,
    tick: usize,
}

pub async fn run(terminal: &mut DefaultTerminal, client: Client) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App {
        client,
        tx,
        settings: Settings::default(),
        search: SearchScreen::default(),
        manga: None,
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
            // Only wake up on a timer while the spinner is visible.
            _ = ticker.tick(), if app.is_loading() => app.tick = app.tick.wrapping_add(1),
        }
    }
    Ok(())
}

impl App {
    fn render(&mut self, frame: &mut Frame) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        match &mut self.manga {
            Some(manga) => manga.render(frame, frame.area(), spinner),
            None => self
                .search
                .render(frame, frame.area(), &self.settings, spinner),
        }
    }

    fn is_loading(&self) -> bool {
        self.search.is_loading() || self.manga.as_ref().is_some_and(MangaScreen::is_loading)
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
        let action = match &mut self.manga {
            Some(manga) => manga.handle_key(key),
            None => self.search.handle_key(key),
        };
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
            Action::Download { chapters, .. } => {
                if let Some(manga) = &mut self.manga {
                    manga.set_notice(format!(
                        "{} chapters selected; downloading isn't built yet",
                        chapters.len()
                    ));
                }
            }
            Action::Back => self.manga = None,
            Action::Quit => self.running = false,
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
