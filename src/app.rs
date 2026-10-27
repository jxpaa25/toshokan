//! Event loop: terminal input and API responses go in, screen updates and API requests come out.

use std::io;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time::MissedTickBehavior;

use crate::api::{Client, Manga, SearchPage};
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
    Quit,
}

/// Results of background work, delivered back to the event loop.
#[derive(Debug)]
pub enum Message {
    SearchResults {
        generation: u64,
        result: Result<SearchPage, String>,
    },
}

pub struct App {
    client: Client,
    tx: UnboundedSender<Message>,
    settings: Settings,
    search: SearchScreen,
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
            _ = ticker.tick(), if app.search.is_loading() => app.tick = app.tick.wrapping_add(1),
        }
    }
    Ok(())
}

impl App {
    fn render(&mut self, frame: &mut Frame) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        self.search
            .render(frame, frame.area(), &self.settings, spinner);
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
        if let Some(action) = self.search.handle_key(key) {
            self.perform(action);
        }
    }

    fn handle_message(&mut self, message: Message) {
        match message {
            Message::SearchResults { generation, result } => {
                self.search.on_results(generation, result)
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
            Action::OpenManga(manga) => self.search.set_notice(format!(
                "“{}”: the manga screen isn't built yet",
                manga.title
            )),
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
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}
