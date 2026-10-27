//! Search screen: a query box, paginated results, and details for the selected manga.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::input::TextInput;
use super::{join_spans, key_hints, panel, rating_span, strip_link_section};
use crate::api::{ContentRating, Manga, SEARCH_PAGE_SIZE, SearchPage};
use crate::app::Action;
use crate::config::Settings;

/// MangaDex rejects requests past this many results.
const RESULT_WINDOW: u32 = 10_000;
/// Below this width the details pane goes under the results instead of beside them.
const SIDE_BY_SIDE_WIDTH: u16 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    /// Lets the screen ignore responses to searches it has since replaced.
    pub generation: u64,
    pub query: String,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Input,
    Results,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loading {
    Fresh,
    More,
}

pub struct SearchScreen {
    input: TextInput,
    focus: Focus,
    /// The query the shown results belong to.
    query: Option<String>,
    results: Vec<Manga>,
    total: u32,
    list: ListState,
    loading: Option<Loading>,
    error: Option<String>,
    notice: Option<String>,
    generation: u64,
}

impl Default for SearchScreen {
    fn default() -> Self {
        SearchScreen {
            input: TextInput::default(),
            focus: Focus::Input,
            query: None,
            results: Vec::new(),
            total: 0,
            list: ListState::default(),
            loading: None,
            error: None,
            notice: None,
            generation: 0,
        }
    }
}

impl SearchScreen {
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.notice = None;
        if key.code == KeyCode::F(2) {
            return Some(Action::ToggleAdult);
        }
        match self.focus {
            Focus::Input => self.handle_input_key(key),
            Focus::Results => self.handle_results_key(key),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Down | KeyCode::Tab | KeyCode::Esc => {
                if !self.results.is_empty() {
                    self.focus = Focus::Results;
                }
                None
            }
            _ => {
                self.input.handle_key(key);
                None
            }
        }
    }

    fn handle_results_key(&mut self, key: KeyEvent) -> Option<Action> {
        let selected = self.list.selected().unwrap_or(0);
        let last = self.results.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') if selected == 0 => {
                self.focus = Focus::Input;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => self.select(selected - 1),
            KeyCode::Down | KeyCode::Char('j') => self.select((selected + 1).min(last)),
            KeyCode::PageUp => self.select(selected.saturating_sub(10)),
            KeyCode::PageDown => self.select((selected + 10).min(last)),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(last),
            KeyCode::Enter => self.selected().cloned().map(Action::OpenManga),
            KeyCode::Tab | KeyCode::Esc | KeyCode::Char('/') => {
                self.focus = Focus::Input;
                None
            }
            KeyCode::Char('q') => Some(Action::Quit),
            _ => None,
        }
    }

    /// Starts a new search for the input's text, replacing any shown results.
    fn submit(&mut self) -> Option<Action> {
        let query = self.input.value().trim();
        if query.is_empty() {
            return None;
        }
        let query = query.to_owned();
        self.results.clear();
        self.total = 0;
        self.list.select(None);
        Some(self.request(query, 0, Loading::Fresh))
    }

    /// Re-runs the current query from the start, e.g. after the adult setting changed.
    pub fn rerun(&mut self) -> Option<Action> {
        let query = self.query.clone()?;
        self.results.clear();
        self.total = 0;
        self.list.select(None);
        self.focus = Focus::Input;
        Some(self.request(query, 0, Loading::Fresh))
    }

    fn request(&mut self, query: String, offset: u32, loading: Loading) -> Action {
        self.generation += 1;
        self.query = Some(query.clone());
        self.loading = Some(loading);
        self.error = None;
        Action::Search(SearchRequest {
            generation: self.generation,
            query,
            offset,
        })
    }

    pub fn on_results(&mut self, generation: u64, result: Result<SearchPage, String>) {
        if generation != self.generation {
            return;
        }
        let fresh = self.loading == Some(Loading::Fresh);
        self.loading = None;
        match result {
            Ok(page) => {
                if fresh {
                    self.results = page.items;
                    self.list.select((!self.results.is_empty()).then_some(0));
                    if !self.results.is_empty() {
                        self.focus = Focus::Results;
                    }
                } else {
                    // Results can shift between pages, so skip ones already shown.
                    for manga in page.items {
                        if !self.results.iter().any(|known| known.id == manga.id) {
                            self.results.push(manga);
                        }
                    }
                }
                self.total = page.total.min(RESULT_WINDOW);
            }
            Err(err) => self.error = Some(err),
        }
    }

    fn select(&mut self, index: usize) -> Option<Action> {
        self.list.select(Some(index));
        self.load_more_if_at_end()
    }

    fn load_more_if_at_end(&mut self) -> Option<Action> {
        let at_end = self.list.selected()? + 1 >= self.results.len();
        let offset = self.results.len() as u32;
        let more = offset < self.total && offset + SEARCH_PAGE_SIZE <= RESULT_WINDOW;
        if !at_end || !more || self.loading.is_some() {
            return None;
        }
        let query = self.query.clone()?;
        Some(self.request(query, offset, Loading::More))
    }

    fn selected(&self) -> Option<&Manga> {
        self.results.get(self.list.selected()?)
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, settings: &Settings, spinner: &str) {
        let [input_area, body, footer] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        let input_block = panel(" Search MangaDex ", self.focus == Focus::Input);
        self.input.render(
            frame,
            input_area,
            input_block,
            "Type a title and press Enter",
            self.focus == Focus::Input,
        );

        let [results_area, details_area] = if body.width >= SIDE_BY_SIDE_WIDTH {
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body)
        } else {
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body)
        };
        self.render_results(frame, results_area, settings, spinner);
        self.render_details(frame, details_area);
        self.render_footer(frame, footer, settings);
    }

    fn render_results(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        settings: &Settings,
        spinner: &str,
    ) {
        let mut title = String::from(" Results ");
        if !self.results.is_empty() {
            title = format!(" Results ({}/{}) ", self.results.len(), self.total);
        }
        if self.loading.is_some() {
            title.push_str(spinner);
            title.push(' ');
        }
        let block = panel(&title, self.focus == Focus::Results);

        if self.results.is_empty() {
            let message = match (&self.loading, &self.error, &self.query) {
                (Some(_), _, _) => Text::from("Searching…".dark_gray()),
                (_, Some(err), _) => Text::from(format!("Search failed: {err}").red()),
                (_, _, None) => Text::from("Results show up here.".dark_gray()),
                (_, _, Some(query)) => {
                    let mut text = Text::from(format!("No manga found for “{query}”."));
                    if !settings.include_adult {
                        text.push_line("Adult titles are hidden (F2 to show them).".dark_gray());
                    }
                    text
                }
            };
            let paragraph = Paragraph::new(message).wrap(Wrap { trim: true });
            frame.render_widget(paragraph.block(block), area);
            return;
        }

        let items: Vec<ListItem> = self.results.iter().map(result_item).collect();
        let list = List::new(items)
            .block(block)
            .highlight_symbol("▶ ")
            .highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, area, &mut self.list);
    }

    fn render_details(&self, frame: &mut Frame, area: Rect) {
        let block = panel(" Details ", false);
        let Some(manga) = self.selected() else {
            frame.render_widget(block, area);
            return;
        };

        let mut text = Text::from(Line::from(manga.title.clone().bold()));
        if !manga.authors.is_empty() {
            text.push_line(manga.authors.join(", ").italic());
        }
        let mut meta = vec![];
        if let Some(year) = manga.year {
            meta.push(Span::raw(year.to_string()));
        }
        if let Some(status) = &manga.status {
            meta.push(Span::raw(status.clone()));
        }
        meta.push(rating_span(manga.content_rating));
        text.push_line(Line::from(join_spans(meta)));
        if !manga.languages.is_empty() {
            text.push_line(format!("Languages: {}", manga.languages.join(", ")).dark_gray());
        }
        text.push_line("");
        match manga.description.as_deref().map(strip_link_section) {
            Some(description) => text.extend(Text::from(description)),
            None => text.push_line("No description.".dark_gray()),
        }

        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false }).block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, settings: &Settings) {
        let adult = if settings.include_adult {
            Span::raw(" Adult: on ").red()
        } else {
            Span::raw(" Adult: off ").dark_gray()
        };
        let [left, right] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(adult.width() as u16)])
                .areas(area);

        let status = if let Some(notice) = &self.notice {
            Line::from(notice.as_str().yellow())
        } else if let (Some(err), false) = (&self.error, self.results.is_empty()) {
            Line::from(format!("Couldn't load more results: {err}").red())
        } else {
            let keys: &[(&str, &str)] = match self.focus {
                Focus::Input => &[
                    ("Enter", "search"),
                    ("Tab", "results"),
                    ("F2", "adult"),
                    ("F3", "downloads"),
                    ("Ctrl+C", "quit"),
                ],
                Focus::Results => &[
                    ("↑↓", "move"),
                    ("Enter", "open"),
                    ("/", "search"),
                    ("F2", "adult"),
                    ("F3", "downloads"),
                    ("q", "quit"),
                ],
            };
            key_hints(keys)
        };
        frame.render_widget(Paragraph::new(status), left);
        frame.render_widget(Paragraph::new(adult), right);
    }
}

fn result_item(manga: &Manga) -> ListItem<'_> {
    let mut title = vec![Span::raw(manga.title.as_str()).bold()];
    if manga.content_rating != ContentRating::Safe {
        title.push(Span::raw(" "));
        title.push(rating_span(manga.content_rating));
    }
    let mut meta = vec![];
    if let Some(year) = manga.year {
        meta.push(Span::raw(year.to_string()));
    }
    if let Some(status) = &manga.status {
        meta.push(Span::raw(status.as_str()));
    }
    if !manga.authors.is_empty() {
        meta.push(Span::raw(manga.authors.join(", ")));
    }
    let meta = Line::from(join_spans(meta)).dark_gray();
    ListItem::new(vec![Line::from(title), meta])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_query(screen: &mut SearchScreen, text: &str) {
        for c in text.chars() {
            screen.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn manga(id: &str, title: &str) -> Manga {
        Manga {
            id: id.to_owned(),
            title: title.to_owned(),
            description: Some("A story.\n\n---\n[Links](https://example.com)".to_owned()),
            status: Some("completed".to_owned()),
            year: Some(1989),
            content_rating: ContentRating::Suggestive,
            languages: vec!["en".to_owned()],
            authors: vec!["Someone".to_owned()],
            cover_file: None,
        }
    }

    fn page(items: Vec<Manga>, total: u32) -> Result<SearchPage, String> {
        Ok(SearchPage {
            items,
            offset: 0,
            total,
        })
    }

    fn search_generation(action: Option<Action>) -> (u64, u32) {
        match action {
            Some(Action::Search(request)) => (request.generation, request.offset),
            _ => panic!("expected a search"),
        }
    }

    #[test]
    fn enter_searches_trimmed_query_and_ignores_blank() {
        let mut screen = SearchScreen::default();
        type_query(&mut screen, "   ");
        assert!(screen.handle_key(key(KeyCode::Enter)).is_none());

        type_query(&mut screen, "berserk ");
        match screen.handle_key(key(KeyCode::Enter)) {
            Some(Action::Search(request)) => {
                assert_eq!(request.query, "berserk");
                assert_eq!(request.offset, 0);
            }
            _ => panic!("expected a search"),
        }
        assert!(screen.is_loading());
    }

    #[test]
    fn ignores_results_from_replaced_searches() {
        let mut screen = SearchScreen::default();
        type_query(&mut screen, "a");
        let (old, _) = search_generation(screen.handle_key(key(KeyCode::Enter)));
        let (new, _) = search_generation(screen.handle_key(key(KeyCode::Enter)));

        screen.on_results(old, page(vec![manga("1", "Old")], 1));
        assert!(screen.results.is_empty());
        screen.on_results(new, page(vec![manga("2", "New")], 1));
        assert_eq!(screen.results[0].title, "New");
        assert_eq!(screen.focus, Focus::Results);
    }

    #[test]
    fn reaching_the_end_loads_the_next_page_once() {
        let mut screen = SearchScreen::default();
        type_query(&mut screen, "a");
        let (generation, _) = search_generation(screen.handle_key(key(KeyCode::Enter)));
        screen.on_results(
            generation,
            page(vec![manga("1", "One"), manga("2", "Two")], 5),
        );

        let (generation, offset) = search_generation(screen.handle_key(key(KeyCode::Down)));
        assert_eq!(offset, 2);
        assert!(
            screen.handle_key(key(KeyCode::Down)).is_none(),
            "already loading"
        );

        screen.on_results(
            generation,
            page(vec![manga("2", "Two"), manga("3", "Three")], 5),
        );
        let titles: Vec<_> = screen.results.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(titles, ["One", "Two", "Three"]);
        assert_eq!(screen.list.selected(), Some(1));
    }

    #[test]
    fn renders_results_and_details() {
        let mut screen = SearchScreen::default();
        type_query(&mut screen, "berserk");
        let (generation, _) = search_generation(screen.handle_key(key(KeyCode::Enter)));
        screen.on_results(generation, page(vec![manga("1", "Berserk")], 1));

        let mut terminal = Terminal::new(TestBackend::new(110, 20)).unwrap();
        let settings = Settings::default();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &settings, "⠋"))
            .unwrap();
        let screen_text = buffer_text(&terminal);
        assert!(screen_text.contains("Results (1/1)"));
        assert!(screen_text.contains("▶ Berserk suggestive"));
        assert!(screen_text.contains("1989 · completed · suggestive"));
        assert!(screen_text.contains("A story."));
        assert!(!screen_text.contains("Links"));
        assert!(screen_text.contains("Adult: off"));
    }

    #[test]
    fn empty_results_mention_hidden_adult_titles() {
        let mut screen = SearchScreen::default();
        type_query(&mut screen, "zzz");
        let (generation, _) = search_generation(screen.handle_key(key(KeyCode::Enter)));
        screen.on_results(generation, page(vec![], 0));

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), &Settings::default(), "⠋"))
            .unwrap();
        let screen_text = buffer_text(&terminal);
        assert!(screen_text.contains("No manga found for “zzz”."));
        assert!(screen_text.contains("Adult titles are hidden"));
    }
}
