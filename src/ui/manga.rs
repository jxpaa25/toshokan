//! Manga screen: pick a translation language, browse its chapters, choose which scanlation
//! group's release to use for each chapter, and select chapters to download.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::{join_spans, key_hints, panel, rating_span, strip_link_section};
use crate::api::{Chapter, ChapterSlot, Manga};
use crate::app::Action;

const HEADER_HEIGHT: u16 = 7;
const LANGUAGES_WIDTH: u16 = 34;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Languages,
    Chapters,
}

enum Chapters {
    Loading,
    Failed(String),
    Loaded(ChapterList),
}

struct ChapterList {
    slots: Vec<ChapterSlot>,
    /// For each slot, the index of the upload (group release) the user picked.
    chosen: Vec<usize>,
    selected: Vec<bool>,
}

impl ChapterList {
    fn new(slots: Vec<ChapterSlot>) -> Self {
        let count = slots.len();
        ChapterList {
            slots,
            chosen: vec![0; count],
            selected: vec![false; count],
        }
    }

    fn chosen_upload(&self, index: usize) -> &Chapter {
        &self.slots[index].uploads[self.chosen[index]]
    }

    fn cycle_group(&mut self, index: usize, forward: bool) {
        let count = self.slots[index].uploads.len();
        let current = self.chosen[index];
        self.chosen[index] = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
    }

    /// Switches every chapter that has a release by the same groups as `index`'s chosen one
    /// over to that group. Returns how many chapters now use it.
    fn prefer_groups_of(&mut self, index: usize) -> usize {
        let groups = self.chosen_upload(index).groups.clone();
        let mut matched = 0;
        for (slot, chosen) in self.slots.iter().zip(&mut self.chosen) {
            if let Some(upload) = slot.uploads.iter().position(|u| u.groups == groups) {
                *chosen = upload;
                matched += 1;
            }
        }
        matched
    }

    fn toggle_all(&mut self) {
        let all = self.selected.iter().all(|selected| *selected);
        self.selected.fill(!all);
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }

    fn selected_uploads(&self) -> Vec<Chapter> {
        (0..self.slots.len())
            .filter(|&index| self.selected[index])
            .map(|index| self.chosen_upload(index).clone())
            .collect()
    }
}

pub struct MangaScreen {
    manga: Manga,
    languages: Vec<String>,
    language_list: ListState,
    /// The language whose chapters are shown.
    language: Option<String>,
    /// Chapters per language, kept so switching back doesn't refetch or lose selections.
    chapters: HashMap<String, Chapters>,
    chapter_list: ListState,
    focus: Focus,
    notice: Option<String>,
}

impl MangaScreen {
    /// Returns the screen and the request for its first language's chapters.
    /// `preferred_language` is listed and opened first when the manga has it.
    pub fn new(manga: Manga, preferred_language: &str) -> (Self, Option<Action>) {
        let mut languages = manga.languages.clone();
        languages.sort_by_cached_key(|code| {
            (
                code != preferred_language,
                language_name(code).to_lowercase(),
            )
        });
        languages.dedup();
        let mut screen = MangaScreen {
            manga,
            languages,
            language_list: ListState::default(),
            language: None,
            chapters: HashMap::new(),
            chapter_list: ListState::default(),
            focus: Focus::Chapters,
            notice: None,
        };
        let action = screen.choose_language(0);
        (screen, action)
    }

    pub fn is_loading(&self) -> bool {
        self.chapters
            .values()
            .any(|chapters| matches!(chapters, Chapters::Loading))
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn on_chapters(
        &mut self,
        manga_id: &str,
        language: String,
        result: Result<Vec<ChapterSlot>, String>,
    ) {
        if manga_id != self.manga.id {
            return;
        }
        let has_chapters = matches!(&result, Ok(slots) if !slots.is_empty());
        let chapters = match result {
            Ok(slots) => Chapters::Loaded(ChapterList::new(slots)),
            Err(err) => Chapters::Failed(err),
        };
        if self.language.as_ref() == Some(&language) && has_chapters {
            self.chapter_list.select(Some(0));
        }
        self.chapters.insert(language, chapters);
    }

    /// Unselects every chapter in the shown language, e.g. after they were queued.
    pub fn clear_selection(&mut self) {
        let language = self.language.as_ref();
        if let Some(Chapters::Loaded(list)) = language.and_then(|code| self.chapters.get_mut(code))
        {
            list.selected.fill(false);
        }
    }

    fn choose_language(&mut self, index: usize) -> Option<Action> {
        let code = self.languages.get(index)?.clone();
        self.language_list.select(Some(index));
        self.language = Some(code.clone());
        self.chapter_list.select(None);
        match self.chapters.get(&code) {
            Some(Chapters::Loaded(list)) => {
                self.chapter_list
                    .select((!list.slots.is_empty()).then_some(0));
                None
            }
            Some(Chapters::Loading) => None,
            Some(Chapters::Failed(_)) | None => {
                self.chapters.insert(code.clone(), Chapters::Loading);
                Some(Action::LoadChapters {
                    manga_id: self.manga.id.clone(),
                    language: code,
                })
            }
        }
    }

    fn current(&self) -> Option<&Chapters> {
        self.chapters.get(self.language.as_ref()?)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        self.notice = None;
        match key.code {
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Esc | KeyCode::Backspace => Some(Action::Back),
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Languages => Focus::Chapters,
                    Focus::Chapters => Focus::Languages,
                };
                None
            }
            _ => match self.focus {
                Focus::Languages => self.handle_language_key(key),
                Focus::Chapters => self.handle_chapter_key(key),
            },
        }
    }

    fn handle_language_key(&mut self, key: KeyEvent) -> Option<Action> {
        let selected = self.language_list.selected().unwrap_or(0);
        let last = self.languages.len().saturating_sub(1);
        let cursor = match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::Chapters;
                return self.choose_language(selected);
            }
            _ => return None,
        };
        self.language_list.select(Some(cursor));
        None
    }

    fn handle_chapter_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Char('r') && matches!(self.current(), Some(Chapters::Failed(_))) {
            let index = self
                .languages
                .iter()
                .position(|code| Some(code) == self.language.as_ref())?;
            return self.choose_language(index);
        }
        let language = self.language.as_ref()?;
        let Some(Chapters::Loaded(list)) = self.chapters.get_mut(language) else {
            return None;
        };
        let selected = self.chapter_list.selected()?;
        let last = list.slots.len() - 1;
        let cursor = match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (selected + 1).min(last),
            KeyCode::PageUp => selected.saturating_sub(10),
            KeyCode::PageDown => (selected + 10).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            KeyCode::Left | KeyCode::Char('h') => {
                list.cycle_group(selected, false);
                selected
            }
            KeyCode::Right | KeyCode::Char('l') => {
                list.cycle_group(selected, true);
                selected
            }
            KeyCode::Char('p') => {
                let matched = list.prefer_groups_of(selected);
                let group = group_label(list.chosen_upload(selected));
                self.notice = Some(format!(
                    "Using {group} for {matched} of {} chapters",
                    list.slots.len()
                ));
                selected
            }
            // Space advances after toggling so runs of chapters are quick to select.
            KeyCode::Char(' ') => {
                list.selected[selected] = !list.selected[selected];
                (selected + 1).min(last)
            }
            KeyCode::Char('a') => {
                list.toggle_all();
                selected
            }
            KeyCode::Char('d') => {
                let chapters = list.selected_uploads();
                if chapters.is_empty() {
                    self.notice =
                        Some("Select chapters with Space (or a for all) first".to_owned());
                    return None;
                }
                return Some(Action::Download {
                    manga: self.manga.clone(),
                    chapters,
                });
            }
            _ => return None,
        };
        self.chapter_list.select(Some(cursor));
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, spinner: &str) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        let [languages_area, chapters_area] =
            Layout::horizontal([Constraint::Length(LANGUAGES_WIDTH), Constraint::Min(0)])
                .areas(body);

        self.render_header(frame, header);
        self.render_languages(frame, languages_area);
        self.render_chapters(frame, chapters_area, spinner);
        self.render_footer(frame, footer);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let manga = &self.manga;
        let mut meta = vec![];
        if !manga.authors.is_empty() {
            meta.push(Span::raw(manga.authors.join(", ")).italic());
        }
        if let Some(year) = manga.year {
            meta.push(Span::raw(year.to_string()));
        }
        if let Some(status) = &manga.status {
            meta.push(Span::raw(status.as_str()));
        }
        meta.push(rating_span(manga.content_rating));

        let mut text = Text::from(Line::from(manga.title.as_str().bold()));
        text.push_line(Line::from(join_spans(meta)));
        match manga.description.as_deref().map(strip_link_section) {
            Some(description) => text.extend(Text::from(description).dark_gray()),
            None => text.push_line("No description.".dark_gray()),
        }
        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(" Manga ", false));
        frame.render_widget(paragraph, area);
    }

    fn render_languages(&mut self, frame: &mut Frame, area: Rect) {
        let block = panel(" Language ", self.focus == Focus::Languages);
        if self.languages.is_empty() {
            let message = Paragraph::new("No translations yet.".dark_gray())
                .wrap(Wrap { trim: true })
                .block(block);
            frame.render_widget(message, area);
            return;
        }
        let items: Vec<ListItem> = self
            .languages
            .iter()
            .map(|code| {
                let active = self.language.as_ref() == Some(code);
                let marker = if active { "● ".green() } else { "  ".into() };
                ListItem::new(Line::from(vec![
                    marker,
                    Span::raw(language_name(code)),
                    Span::raw(format!(" {code}")).dark_gray(),
                ]))
            })
            .collect();
        frame.render_stateful_widget(list_widget(items, block), area, &mut self.language_list);
    }

    fn render_chapters(&mut self, frame: &mut Frame, area: Rect, spinner: &str) {
        let language = self.language.as_deref().map_or("none", language_name);
        let chapters = self
            .language
            .as_ref()
            .and_then(|code| self.chapters.get(code));

        let mut title = format!(" Chapters · {language} ");
        match chapters {
            Some(Chapters::Loading) => title.push_str(&format!("{spinner} ")),
            Some(Chapters::Loaded(list)) if !list.slots.is_empty() => {
                title.push_str(&format!("· {} ", list.slots.len()));
                let selected = list.selected_count();
                if selected > 0 {
                    title.push_str(&format!("· {selected} selected "));
                }
            }
            _ => {}
        }
        let block = panel(&title, self.focus == Focus::Chapters);

        let message = match chapters {
            Some(Chapters::Loaded(list)) if !list.slots.is_empty() => {
                let items: Vec<ListItem> = (0..list.slots.len())
                    .map(|index| chapter_item(list, index))
                    .collect();
                frame.render_stateful_widget(
                    list_widget(items, block),
                    area,
                    &mut self.chapter_list,
                );
                return;
            }
            Some(Chapters::Loaded(_)) => {
                let mut text = Text::from(format!("No chapters hosted on MangaDex in {language}."));
                text.push_line(
                    "They may have been removed or only be on official sites.".dark_gray(),
                );
                text
            }
            Some(Chapters::Loading) => Text::from("Loading chapters…".dark_gray()),
            Some(Chapters::Failed(err)) => {
                let mut text = Text::from(format!("Couldn't load chapters: {err}").red());
                text.push_line("Press r to retry.".dark_gray());
                text
            }
            None => Text::from("No chapters to show.".dark_gray()),
        };
        let paragraph = Paragraph::new(message)
            .wrap(Wrap { trim: true })
            .block(block);
        frame.render_widget(paragraph, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(match self.focus {
                Focus::Languages => &[
                    ("↑↓", "move"),
                    ("Enter", "choose"),
                    ("Tab", "chapters"),
                    ("F3", "downloads"),
                    ("Esc", "back"),
                    ("q", "quit"),
                ],
                Focus::Chapters => &[
                    ("Space", "select"),
                    ("a", "all"),
                    ("←→", "group"),
                    ("p", "use group everywhere"),
                    ("d", "download"),
                    ("Tab", "languages"),
                    ("F3", "downloads"),
                    ("Esc", "back"),
                ],
            }),
        };
        frame.render_widget(Paragraph::new(status), area);
    }
}

fn list_widget<'a>(items: Vec<ListItem<'a>>, block: ratatui::widgets::Block<'a>) -> List<'a> {
    List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
}

fn chapter_item(list: &ChapterList, index: usize) -> ListItem<'_> {
    let slot = &list.slots[index];
    let upload = list.chosen_upload(index);

    let checkbox = if list.selected[index] {
        "[x] ".green().bold()
    } else {
        "[ ] ".dark_gray()
    };
    let mut first = vec![checkbox, Span::raw(chapter_label(slot)).bold()];
    // Not every group fills in the title, so fall back to another upload's.
    let title = upload
        .title
        .as_deref()
        .or_else(|| slot.uploads.iter().find_map(|u| u.title.as_deref()));
    if let Some(title) = title {
        first.push(Span::raw(format!(" · {title}")));
    }

    let mut second = vec![Span::raw("    ")];
    if slot.uploads.len() > 1 {
        second.push(Span::raw(format!("◂ {} ▸", group_label(upload))).cyan());
        second.push(
            Span::raw(format!(
                " {}/{}",
                list.chosen[index] + 1,
                slot.uploads.len()
            ))
            .dark_gray(),
        );
    } else {
        second.push(Span::raw(group_label(upload)).dark_gray());
    }
    let date = upload.publish_at.get(..10).unwrap_or(&upload.publish_at);
    second.push(Span::raw(format!(" · {} pages · {date}", upload.pages)).dark_gray());

    ListItem::new(vec![Line::from(first), Line::from(second)])
}

fn chapter_label(slot: &ChapterSlot) -> String {
    match (&slot.volume, &slot.number) {
        (Some(volume), Some(number)) => format!("Vol. {volume} Ch. {number}"),
        (None, Some(number)) => format!("Ch. {number}"),
        (Some(volume), None) => format!("Vol. {volume} Oneshot"),
        (None, None) => "Oneshot".to_owned(),
    }
}

fn group_label(upload: &Chapter) -> String {
    if upload.groups.is_empty() {
        "No group".to_owned()
    } else {
        upload.groups.join(" & ")
    }
}

/// English name for a MangaDex language code, or the code itself when unknown.
fn language_name(code: &str) -> &str {
    match code {
        "en" => "English",
        "ja" => "Japanese",
        "ja-ro" => "Japanese (romaji)",
        "ko" => "Korean",
        "ko-ro" => "Korean (romanized)",
        "zh" => "Chinese (simplified)",
        "zh-hk" => "Chinese (traditional)",
        "zh-ro" => "Chinese (romanized)",
        "es" => "Spanish",
        "es-la" => "Spanish (LATAM)",
        "pt" => "Portuguese",
        "pt-br" => "Portuguese (Brazil)",
        "fr" => "French",
        "de" => "German",
        "it" => "Italian",
        "nl" => "Dutch",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "be" => "Belarusian",
        "pl" => "Polish",
        "cs" => "Czech",
        "sk" => "Slovak",
        "hu" => "Hungarian",
        "ro" => "Romanian",
        "bg" => "Bulgarian",
        "sr" => "Serbian",
        "hr" => "Croatian",
        "el" => "Greek",
        "tr" => "Turkish",
        "ar" => "Arabic",
        "fa" => "Persian",
        "he" => "Hebrew",
        "hi" => "Hindi",
        "bn" => "Bengali",
        "ne" => "Nepali",
        "ta" => "Tamil",
        "th" => "Thai",
        "vi" => "Vietnamese",
        "id" => "Indonesian",
        "ms" => "Malay",
        "tl" => "Filipino",
        "my" => "Burmese",
        "mn" => "Mongolian",
        "kk" => "Kazakh",
        "uz" => "Uzbek",
        "az" => "Azerbaijani",
        "ka" => "Georgian",
        "lt" => "Lithuanian",
        "lv" => "Latvian",
        "et" => "Estonian",
        "fi" => "Finnish",
        "sv" => "Swedish",
        "no" => "Norwegian",
        "da" => "Danish",
        "ca" => "Catalan",
        "eu" => "Basque",
        "gl" => "Galician",
        "la" => "Latin",
        "eo" => "Esperanto",
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;
    use crate::ui::buffer_text;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn manga(languages: &[&str]) -> Manga {
        Manga {
            id: "m1".to_owned(),
            title: "Berserk".to_owned(),
            description: Some("Guts.".to_owned()),
            status: Some("ongoing".to_owned()),
            year: Some(1989),
            content_rating: ContentRating::Erotica,
            languages: languages.iter().map(|l| l.to_string()).collect(),
            authors: vec!["Miura Kentarou".to_owned()],
            cover_file: None,
        }
    }

    fn upload(id: &str, group: &str) -> Chapter {
        Chapter {
            id: id.to_owned(),
            volume: Some("1".to_owned()),
            number: None,
            title: None,
            language: "en".to_owned(),
            pages: 20,
            groups: vec![group.to_owned()],
            publish_at: "2018-01-31T07:07:06+00:00".to_owned(),
        }
    }

    fn slot(number: &str, uploads: Vec<Chapter>) -> ChapterSlot {
        ChapterSlot {
            volume: Some("1".to_owned()),
            number: Some(number.to_owned()),
            uploads,
        }
    }

    /// Two chapters: "1" by A or B, "2" by B only.
    fn loaded_screen() -> MangaScreen {
        let (mut screen, _) = MangaScreen::new(manga(&["en"]), "en");
        let slots = vec![
            slot("1", vec![upload("1a", "A"), upload("1b", "B")]),
            slot("2", vec![upload("2b", "B")]),
        ];
        screen.on_chapters("m1", "en".to_owned(), Ok(slots));
        screen
    }

    fn downloaded_ids(action: Option<Action>) -> Vec<String> {
        match action {
            Some(Action::Download { chapters, .. }) => chapters.into_iter().map(|c| c.id).collect(),
            other => panic!("expected a download, got {other:?}"),
        }
    }

    #[test]
    fn opens_english_first_and_caches_languages() {
        let (mut screen, action) = MangaScreen::new(manga(&["fr", "en", "es-la"]), "en");
        assert_eq!(screen.languages, ["en", "fr", "es-la"]);
        assert!(
            matches!(action, Some(Action::LoadChapters { ref language, .. }) if language == "en")
        );

        screen.on_chapters("m1", "en".to_owned(), Ok(vec![]));
        screen.focus = Focus::Languages;
        screen.handle_key(key(KeyCode::Down));
        let action = screen.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, Some(Action::LoadChapters { ref language, .. }) if language == "fr")
        );

        screen.focus = Focus::Languages;
        screen.handle_key(key(KeyCode::Up));
        assert!(
            screen.handle_key(key(KeyCode::Enter)).is_none(),
            "English is cached"
        );
    }

    #[test]
    fn ignores_chapters_for_another_manga() {
        let (mut screen, _) = MangaScreen::new(manga(&["en"]), "en");
        screen.on_chapters("other", "en".to_owned(), Ok(vec![]));
        assert!(screen.is_loading());
    }

    #[test]
    fn downloads_the_chosen_group_for_selected_chapters() {
        let mut screen = loaded_screen();
        assert!(screen.handle_key(key(KeyCode::Char('d'))).is_none());
        assert!(screen.notice.is_some());

        screen.handle_key(key(KeyCode::Right));
        screen.handle_key(key(KeyCode::Char(' ')));
        screen.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            downloaded_ids(screen.handle_key(key(KeyCode::Char('d')))),
            ["1b", "2b"]
        );

        screen.handle_key(key(KeyCode::Char('a')));
        assert!(
            screen.handle_key(key(KeyCode::Char('d'))).is_none(),
            "a clears a full selection"
        );
    }

    #[test]
    fn preferring_a_group_applies_it_where_available() {
        let mut screen = loaded_screen();
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            screen.notice.as_deref(),
            Some("Using B for 2 of 2 chapters")
        );

        screen.handle_key(key(KeyCode::Char('a')));
        assert_eq!(
            downloaded_ids(screen.handle_key(key(KeyCode::Char('d')))),
            ["1b", "2b"]
        );
    }

    #[test]
    fn retries_after_a_failure() {
        let (mut screen, _) = MangaScreen::new(manga(&["en"]), "en");
        screen.on_chapters("m1", "en".to_owned(), Err("timed out".to_owned()));
        assert!(!screen.is_loading());
        let action = screen.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(action, Some(Action::LoadChapters { .. })));
        assert!(screen.is_loading());
    }

    #[test]
    fn renders_chapters_with_group_choice() {
        let mut screen = loaded_screen();
        screen.handle_key(key(KeyCode::Char(' ')));

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), "⠋"))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Miura Kentarou · 1989 · ongoing · erotica"));
        assert!(text.contains("● English en"));
        assert!(text.contains("Chapters · English · 2 · 1 selected"));
        assert!(text.contains("[x] Vol. 1 Ch. 1"));
        assert!(text.contains("◂ A ▸ 1/2 · 20 pages · 2018-01-31"));
        assert!(text.contains("B · 20 pages"));
    }
}
