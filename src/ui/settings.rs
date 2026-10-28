//! Settings screen: everything in config.toml, changed in place and saved right away.

use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use super::input::TextInput;
use super::languages::{LANGUAGES, language_name};
use super::{display_path, key_hints, panel, truncate_start};
use crate::api::Quality;
use crate::app::Action;
use crate::config::{Settings, expand_home};
use crate::download::OutputFormat;

/// The label column.
const LABEL: usize = 18;

/// A setting changed on the screen, for the app to apply and save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Language(String),
    Quality(Quality),
    Format(OutputFormat),
    /// As typed, `~` and all.
    DownloadDir(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Language,
    Folder,
    Adult,
    Quality,
    Format,
}

const ROWS: [Row; 5] = [
    Row::Language,
    Row::Folder,
    Row::Adult,
    Row::Quality,
    Row::Format,
];

/// A setting being changed in a popup.
enum Editing {
    Language {
        filter: TextInput,
        list: ListState,
    },
    Folder {
        input: TextInput,
        /// Folders Tab could complete to, when there's more than one.
        candidates: Vec<String>,
        problem: Option<String>,
    },
}

pub struct SettingsScreen {
    row: Row,
    editing: Option<Editing>,
    notice: Option<String>,
    config_path: String,
    /// `$TOSHOKAN_DIR`, which decides the download folder when set.
    override_dir: Option<PathBuf>,
    /// While the library moves to a new folder: how many are done, of how many, and the one
    /// moving now.
    moving: Option<(usize, usize, String)>,
}

impl SettingsScreen {
    pub fn new(config_path: &Path, override_dir: Option<PathBuf>) -> Self {
        SettingsScreen {
            row: Row::Language,
            editing: None,
            notice: None,
            config_path: display_path(config_path),
            override_dir,
            moving: None,
        }
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub fn set_moving(&mut self, moving: Option<(usize, usize, String)>) {
        self.moving = moving;
    }

    pub fn handle_key(&mut self, key: KeyEvent, settings: &Settings) -> Option<Action> {
        self.notice = None;
        if self.editing.is_some() {
            return self.edit(key);
        }
        let index = ROWS.iter().position(|row| *row == self.row).unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.row = ROWS[index.saturating_sub(1)],
            KeyCode::Down | KeyCode::Char('j') => self.row = ROWS[(index + 1).min(ROWS.len() - 1)],
            KeyCode::Home | KeyCode::Char('g') => self.row = ROWS[0],
            KeyCode::End | KeyCode::Char('G') => self.row = ROWS[ROWS.len() - 1],
            KeyCode::Enter
            | KeyCode::Char(' ')
            | KeyCode::Right
            | KeyCode::Left
            | KeyCode::Char('l')
            | KeyCode::Char('h') => return self.change(settings),
            KeyCode::Esc | KeyCode::Backspace => return Some(Action::CloseOverlay),
            KeyCode::Char('q') => return Some(Action::Quit),
            _ => {}
        }
        None
    }

    /// Flips the selected setting, or opens a popup to pick its new value.
    fn change(&mut self, settings: &Settings) -> Option<Action> {
        let change = match self.row {
            Row::Language => {
                let current = LANGUAGES
                    .iter()
                    .position(|(code, _)| *code == settings.language);
                let mut list = ListState::default();
                list.select(Some(current.unwrap_or(0)));
                self.editing = Some(Editing::Language {
                    filter: TextInput::default(),
                    list,
                });
                return None;
            }
            Row::Folder => {
                if let Some(dir) = &self.override_dir {
                    self.notice = Some(format!(
                        "TOSHOKAN_DIR sets the download folder ({}); unset it to change it here",
                        display_path(dir)
                    ));
                    return None;
                }
                let mut input = TextInput::default();
                input.set(&settings.download_dir);
                self.editing = Some(Editing::Folder {
                    input,
                    candidates: Vec::new(),
                    problem: None,
                });
                return None;
            }
            Row::Adult => return Some(Action::ToggleAdult),
            Row::Quality => Change::Quality(match settings.quality {
                Quality::Data => Quality::DataSaver,
                Quality::DataSaver => Quality::Data,
            }),
            Row::Format => Change::Format(settings.format.toggled()),
        };
        Some(Action::ChangeSetting(change))
    }

    fn edit(&mut self, key: KeyEvent) -> Option<Action> {
        if key.code == KeyCode::Esc {
            self.editing = None;
            return None;
        }
        match self.editing.as_mut()? {
            Editing::Language { filter, list } => {
                let matches = matching(filter.value());
                let selected = list.selected().unwrap_or(0);
                let last = matches.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up => list.select(Some(selected.saturating_sub(1))),
                    KeyCode::Down => list.select(Some((selected + 1).min(last))),
                    KeyCode::PageUp => list.select(Some(selected.saturating_sub(10))),
                    KeyCode::PageDown => list.select(Some((selected + 10).min(last))),
                    KeyCode::Enter => {
                        let (code, _) = matches.get(selected)?;
                        let change = Change::Language((*code).to_owned());
                        self.editing = None;
                        return Some(Action::ChangeSetting(change));
                    }
                    _ => {
                        if filter.handle_key(key) {
                            list.select(Some(0));
                        }
                    }
                }
            }
            Editing::Folder {
                input,
                candidates,
                problem,
            } => match key.code {
                KeyCode::Tab => {
                    let (completed, found) = complete_path(input.value());
                    input.set(&completed);
                    *candidates = found;
                    *problem = None;
                }
                KeyCode::Enter => match check_folder(input.value()) {
                    Ok(dir) => {
                        self.editing = None;
                        return Some(Action::ChangeSetting(Change::DownloadDir(dir)));
                    }
                    Err(err) => *problem = Some(err),
                },
                _ => {
                    if input.handle_key(key) {
                        candidates.clear();
                        *problem = None;
                    }
                }
            },
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, settings: &Settings, spinner: &str) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        // Long paths lose their start, so the folder's own name stays in view.
        let room = usize::from(body.width).saturating_sub(2 + LABEL);
        let folder = match &self.override_dir {
            Some(dir) => {
                let note = "  (set by TOSHOKAN_DIR)";
                vec![
                    Span::raw(truncate_start(
                        &display_path(dir),
                        room.saturating_sub(note.len()),
                    )),
                    Span::raw(note).dark_gray(),
                ]
            }
            None => vec![Span::raw(truncate_start(&settings.download_dir, room))],
        };
        let quality = match settings.quality {
            Quality::Data => "Original",
            Quality::DataSaver => "Data saver",
        };
        let adult = if settings.include_adult {
            "on".red()
        } else {
            "off".into()
        };
        let rows = [
            (
                Row::Language,
                "Language",
                vec![Span::raw(format!(
                    "{} ({})",
                    language_name(&settings.language),
                    settings.language
                ))],
                "Translation opened first on a manga's page; also used to find older downloads \
                 on MangaDex.",
            ),
            (
                Row::Folder,
                "Download folder",
                folder,
                "Where chapters are saved and the library reads from. Changing it offers to move \
                 the library there.",
            ),
            (
                Row::Adult,
                "Adult content",
                vec![adult],
                "Show erotica and pornographic titles in search. F2 toggles it anywhere.",
            ),
            (
                Row::Quality,
                "Quality",
                vec![Span::raw(quality)],
                "Preselected when downloading: original pages (1–3 MB each) or data saver \
                 (~300 KB).",
            ),
            (
                Row::Format,
                "Format",
                vec![Span::raw(settings.format.label())],
                "Preselected when downloading: CBZ archives or plain image folders.",
            ),
        ];
        let mut lines = vec![Line::default()];
        for (row, label, value, about) in rows {
            let current = self.row == row;
            let cursor = if current { "▶ ".cyan() } else { "  ".into() };
            let label = Span::raw(format!("{label:<width$}", width = LABEL - 2));
            let mut spans = vec![cursor, if current { label.bold() } else { label }];
            spans.extend(value);
            lines.push(Line::from(spans));
            lines.push(Line::from(format!("    {about}").dark_gray()));
        }
        lines.push(Line::default());
        let room = usize::from(body.width).saturating_sub(13);
        lines.push(Line::from(
            format!("  Saved to {}", truncate_start(&self.config_path, room)).dark_gray(),
        ));
        if let Some((done, total, name)) = &self.moving {
            lines.push(Line::default());
            lines.push(Line::from(
                format!("  Moving the library: {done} of {total} · {name} {spinner}").yellow(),
            ));
        }
        frame.render_widget(Paragraph::new(lines).block(panel(" Settings ", true)), body);

        let status = match &self.notice {
            Some(notice) => Line::from(notice.as_str().yellow()),
            None => key_hints(&[
                ("↑↓", "move"),
                ("Enter", "change"),
                ("Esc", "back"),
                ("q", "quit"),
            ]),
        };
        frame.render_widget(Paragraph::new(status), footer);

        match &mut self.editing {
            Some(Editing::Language { filter, list }) => {
                render_language_picker(frame, area, filter, list, &settings.language)
            }
            Some(Editing::Folder {
                input,
                candidates,
                problem,
            }) => render_folder_input(frame, area, input, candidates, problem.as_deref()),
            None => {}
        }
    }
}

fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn render_language_picker(
    frame: &mut Frame,
    area: Rect,
    filter: &TextInput,
    list: &mut ListState,
    current: &str,
) {
    let area = popup(area, 48, 20);
    frame.render_widget(Clear, area);
    let block = panel(" Language ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [find, choices, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);
    let mut find_line = vec![Span::raw(" find: ").dark_gray(), Span::raw(filter.value())];
    find_line.push(Span::raw("▏"));
    frame.render_widget(Paragraph::new(Line::from(find_line)), find);
    let matches = matching(filter.value());
    if matches.is_empty() {
        let message = format!(" No language matches “{}”", filter.value()).dark_gray();
        frame.render_widget(Paragraph::new(message), choices);
    } else {
        let items: Vec<ListItem> = matches
            .iter()
            .map(|(code, name)| {
                let marker = if *code == current {
                    "● ".green()
                } else {
                    "  ".into()
                };
                ListItem::new(Line::from(vec![
                    marker,
                    Span::raw(*name),
                    Span::raw(format!(" {code}")).dark_gray(),
                ]))
            })
            .collect();
        let widget = List::new(items).highlight_symbol("▶ ").highlight_style(
            Style::new()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_stateful_widget(widget, choices, list);
    }
    let keys = key_hints(&[("type", "to find"), ("Enter", "set"), ("Esc", "cancel")]);
    frame.render_widget(Paragraph::new(keys), hints);
}

fn render_folder_input(
    frame: &mut Frame,
    area: Rect,
    input: &TextInput,
    candidates: &[String],
    problem: Option<&str>,
) {
    let area = popup(area, 72, 8);
    frame.render_widget(Clear, area);
    let block = panel(" Download folder ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [field, below, hints] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);
    input.render(frame, field, Block::bordered(), "~/Manga", true);
    let note = match problem {
        Some(problem) => Line::from(format!(" {problem}").red()),
        None if !candidates.is_empty() => {
            Line::from(format!(" {}", candidates.join("  ")).dark_gray())
        }
        None => Line::default(),
    };
    frame.render_widget(Paragraph::new(note), below);
    let keys = key_hints(&[("Tab", "complete"), ("Enter", "save"), ("Esc", "cancel")]);
    frame.render_widget(Paragraph::new(keys), hints);
}

/// Asks whether to move the library from the old download folder into the new one.
pub struct MoveDialog {
    from: PathBuf,
    to: PathBuf,
    /// The manga folders and loose chapters to move.
    entries: Vec<PathBuf>,
}

impl MoveDialog {
    pub fn new(from: PathBuf, to: PathBuf, entries: Vec<PathBuf>) -> Self {
        MoveDialog { from, to, entries }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        match key.code {
            KeyCode::Char('y') => Some(Action::MoveLibrary {
                to: self.to.clone(),
                entries: std::mem::take(&mut self.entries),
            }),
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => Some(Action::CloseDialog),
            _ => None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let count = self.entries.len();
        let plural = if count == 1 { "" } else { "s" };
        let lines = vec![
            Line::default(),
            Line::from(format!(
                "  {count} manga folder{plural} or chapter{plural} in the old folder:"
            )),
            Line::from(
                format!("    {}", truncate_start(&display_path(&self.from), 64)).dark_gray(),
            ),
            Line::from("  Move them to the new one?"),
            Line::from(format!("    {}", truncate_start(&display_path(&self.to), 64)).dark_gray()),
            Line::default(),
            Line::from("  Anything else in the old folder stays, and so does anything the"),
            Line::from("  new folder already has a folder of the same name for."),
            Line::default(),
            key_hints(&[("y", "move"), ("n", "leave them")]),
        ];
        let area = popup(area, 72, lines.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let block = panel(" Move your library? ", true);
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}

/// The languages whose name or code contains `filter`, ignoring case.
fn matching(filter: &str) -> Vec<(&'static str, &'static str)> {
    let filter = filter.to_lowercase();
    LANGUAGES
        .iter()
        .filter(|(code, name)| code.contains(&filter) || name.to_lowercase().contains(&filter))
        .copied()
        .collect()
}

/// Completes the last part of a typed path to the folders whose names start with it, as far
/// as they agree. Returns the new text and, when several folders match, their names.
pub fn complete_path(typed: &str) -> (String, Vec<String>) {
    if typed == "~" {
        return ("~/".to_owned(), Vec::new());
    }
    let Some(slash) = typed.rfind('/') else {
        return (typed.to_owned(), Vec::new());
    };
    let (parent, prefix) = typed.split_at(slash + 1);
    let Ok(entries) = fs::read_dir(expand_home(parent)) else {
        return (typed.to_owned(), Vec::new());
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix))
        // Hidden folders only when asked for.
        .filter(|name| prefix.starts_with('.') || !name.starts_with('.'))
        .collect();
    names.sort();
    match names.as_slice() {
        [] => (typed.to_owned(), Vec::new()),
        [only] => (format!("{parent}{only}/"), Vec::new()),
        [first, rest @ ..] => {
            let mut common = first.as_str();
            for name in rest {
                let agreed = common
                    .char_indices()
                    .zip(name.chars())
                    .take_while(|((_, a), b)| a == b)
                    .last()
                    .map_or(0, |((index, c), _)| index + c.len_utf8());
                common = &common[..agreed];
            }
            (format!("{parent}{common}"), names)
        }
    }
}

/// Checks a typed download folder: a full path (or one from `~`) that isn't a file. Returns
/// it without a trailing slash.
pub fn check_folder(typed: &str) -> Result<String, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("Type a folder, like ~/Manga".to_owned());
    }
    if !typed.starts_with('/') && !typed.starts_with('~') {
        return Err("Use a full path, like ~/Manga or /mnt/books/manga".to_owned());
    }
    let dir = match typed.trim_end_matches('/') {
        "" => "/",
        dir => dir,
    };
    let path = expand_home(dir);
    if path.exists() && !path.is_dir() {
        return Err(format!("{} is a file, not a folder", display_path(&path)));
    }
    Ok(dir.to_owned())
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

    fn draw(screen: &mut SettingsScreen, settings: &Settings) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area(), settings, "⠋"))
            .unwrap();
        buffer_text(&terminal)
    }

    fn changed(action: Option<Action>) -> Change {
        match action {
            Some(Action::ChangeSetting(change)) => change,
            other => panic!("expected a change, got {other:?}"),
        }
    }

    #[test]
    fn lists_the_settings_and_flips_the_simple_ones() {
        let settings = Settings::default();
        let mut screen = SettingsScreen::new(Path::new("/tmp/config.toml"), None);
        let text = draw(&mut screen, &settings);
        assert!(text.contains("▶ Language        English (en)"), "{text}");
        assert!(text.contains("  Download folder ~/Manga"));
        assert!(text.contains("  Adult content   off"));
        assert!(text.contains("  Quality         Original"));
        assert!(text.contains("  Format          CBZ archive"));
        assert!(text.contains("Saved to /tmp/config.toml"));

        screen.handle_key(key(KeyCode::Down), &settings);
        screen.handle_key(key(KeyCode::Down), &settings);
        assert!(matches!(
            screen.handle_key(key(KeyCode::Enter), &settings),
            Some(Action::ToggleAdult)
        ));
        screen.handle_key(key(KeyCode::Down), &settings);
        assert_eq!(
            changed(screen.handle_key(key(KeyCode::Enter), &settings)),
            Change::Quality(Quality::DataSaver)
        );
        screen.handle_key(key(KeyCode::End), &settings);
        assert_eq!(
            changed(screen.handle_key(key(KeyCode::Char(' ')), &settings)),
            Change::Format(OutputFormat::Folder)
        );
        assert!(matches!(
            screen.handle_key(key(KeyCode::Esc), &settings),
            Some(Action::CloseOverlay)
        ));
    }

    #[test]
    fn picks_a_language_from_a_filtered_list() {
        let settings = Settings::default();
        let mut screen = SettingsScreen::new(Path::new("/tmp/config.toml"), None);
        screen.handle_key(key(KeyCode::Enter), &settings);
        let text = draw(&mut screen, &settings);
        assert!(text.contains("▶ ● English en"), "{text}");
        for c in "span".chars() {
            screen.handle_key(key(KeyCode::Char(c)), &settings);
        }
        let text = draw(&mut screen, &settings);
        assert!(text.contains("find: span▏"));
        assert!(text.contains("Spanish (LATAM) es-la"));
        assert!(
            !text.contains("English en"),
            "only the screen behind says English"
        );
        screen.handle_key(key(KeyCode::Down), &settings);
        assert_eq!(
            changed(screen.handle_key(key(KeyCode::Enter), &settings)),
            Change::Language("es-la".to_owned())
        );
        assert!(screen.editing.is_none());

        // Esc leaves the language as it was.
        screen.handle_key(key(KeyCode::Enter), &settings);
        screen.handle_key(key(KeyCode::Char('x')), &settings);
        assert!(screen.handle_key(key(KeyCode::Esc), &settings).is_none());
        assert!(screen.editing.is_none());
    }

    #[test]
    fn types_a_folder_with_tab_completion() {
        let home = tempfile::tempdir().unwrap();
        for dir in ["Manga", "Media", "Music", ".hidden"] {
            fs::create_dir(home.path().join(dir)).unwrap();
        }
        fs::write(home.path().join("Maps.txt"), b"").unwrap();
        let base = home.path().display().to_string();

        let settings = Settings::default();
        let mut screen = SettingsScreen::new(Path::new("/tmp/config.toml"), None);
        screen.handle_key(key(KeyCode::Down), &settings);
        screen.handle_key(key(KeyCode::Enter), &settings);
        assert!(draw(&mut screen, &settings).contains("│~/Manga"));
        for _ in 0..7 {
            screen.handle_key(key(KeyCode::Backspace), &settings);
        }
        for c in format!("{base}/M").chars() {
            screen.handle_key(key(KeyCode::Char(c)), &settings);
        }
        screen.handle_key(key(KeyCode::Tab), &settings);
        let text = draw(&mut screen, &settings);
        assert!(text.contains("Manga  Media  Music"), "{text}");
        for c in "an".chars() {
            screen.handle_key(key(KeyCode::Char(c)), &settings);
        }
        screen.handle_key(key(KeyCode::Tab), &settings);
        assert_eq!(
            changed(screen.handle_key(key(KeyCode::Enter), &settings)),
            Change::DownloadDir(format!("{base}/Manga"))
        );

        // A relative path isn't accepted.
        screen.handle_key(key(KeyCode::Enter), &settings);
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        screen.handle_key(ctrl_u, &settings);
        for c in "manga".chars() {
            screen.handle_key(key(KeyCode::Char(c)), &settings);
        }
        assert!(screen.handle_key(key(KeyCode::Enter), &settings).is_none());
        assert!(draw(&mut screen, &settings).contains("Use a full path, like ~/Manga"));
    }

    #[test]
    fn the_folder_is_locked_when_the_environment_sets_it() {
        let settings = Settings::default();
        let mut screen =
            SettingsScreen::new(Path::new("/tmp/config.toml"), Some("/srv/manga".into()));
        let text = draw(&mut screen, &settings);
        assert!(text.contains("/srv/manga  (set by TOSHOKAN_DIR)"));
        screen.handle_key(key(KeyCode::Down), &settings);
        assert!(screen.handle_key(key(KeyCode::Enter), &settings).is_none());
        assert!(screen.editing.is_none());
        assert!(draw(&mut screen, &settings).contains("unset it to change it here"));
    }

    #[test]
    fn completes_paths_like_a_shell() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["Manga", "Manhwa", ".config"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        let base = root.path().display().to_string();
        let (text, candidates) = complete_path(&format!("{base}/Ma"));
        assert_eq!(text, format!("{base}/Man"));
        assert_eq!(candidates, ["Manga", "Manhwa"]);
        assert_eq!(
            complete_path(&format!("{base}/Mang")).0,
            format!("{base}/Manga/")
        );
        assert_eq!(
            complete_path(&format!("{base}/.c")).0,
            format!("{base}/.config/")
        );
        assert_eq!(complete_path(&format!("{base}/")).1, ["Manga", "Manhwa"]);
        assert_eq!(complete_path(&format!("{base}/Zz")).0, format!("{base}/Zz"));
        assert_eq!(complete_path("~").0, "~/");
    }

    #[test]
    fn checks_typed_folders() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("notes.txt");
        fs::write(&file, b"").unwrap();
        assert_eq!(check_folder(" ~/Manga/ "), Ok("~/Manga".to_owned()));
        assert_eq!(check_folder("/"), Ok("/".to_owned()));
        assert!(check_folder("").is_err());
        assert!(check_folder("Manga").unwrap_err().contains("full path"));
        assert!(
            check_folder(&file.display().to_string())
                .unwrap_err()
                .contains("is a file")
        );
    }
}
