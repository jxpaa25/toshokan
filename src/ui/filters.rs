//! Popup for the search filters and sort order. Changes are a draft until Enter searches with
//! them; Esc drops them.

use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::{key_hints, panel};
use crate::api::{Demographic, SearchFilters, Sort, Status, Tag, TagFilter};

const WIDTH: u16 = 78;
/// Lines of matching tags shown under the finder.
const TAG_LINES: usize = 3;
/// The cursor and label column in front of each row's values.
const LABEL: usize = 15;

/// The tags to pick from, fetched when the popup first opens.
pub enum TagList {
    NotLoaded,
    Loading,
    Loaded(Vec<Tag>),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Sort,
    Status,
    Demographic,
    Year,
    Tags,
}

const ROWS: [Row; 5] = [
    Row::Sort,
    Row::Status,
    Row::Demographic,
    Row::Year,
    Row::Tags,
];

pub enum Closed {
    /// Search with these filters.
    Search(SearchFilters),
    Cancel,
}

pub struct FilterPopup {
    draft: SearchFilters,
    row: Row,
    /// The highlighted option in the status and demographic rows.
    status: usize,
    demographic: usize,
    /// Digits typed in the year row; only a full year counts.
    year: String,
    /// Narrows the tags shown to those whose name contains it.
    finder: String,
    /// The highlighted tag among those the finder matches.
    tag: usize,
}

impl FilterPopup {
    pub fn new(filters: &SearchFilters) -> Self {
        FilterPopup {
            draft: filters.clone(),
            row: Row::Sort,
            status: 0,
            demographic: 0,
            year: filters
                .year
                .map(|year| year.to_string())
                .unwrap_or_default(),
            finder: String::new(),
            tag: 0,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, tags: &TagList) -> Option<Closed> {
        match key.code {
            KeyCode::Esc => return Some(Closed::Cancel),
            KeyCode::Enter => {
                let mut filters = self.draft.clone();
                filters.year = self.year.parse().ok().filter(|_| self.year.len() == 4);
                return Some(Closed::Search(filters));
            }
            KeyCode::Up | KeyCode::BackTab => self.move_row(ROWS.len() - 1),
            KeyCode::Down | KeyCode::Tab => self.move_row(1),
            KeyCode::Delete => {
                *self = FilterPopup {
                    row: self.row,
                    ..FilterPopup::new(&SearchFilters::default())
                }
            }
            _ => self.edit_row(key, tags),
        }
        None
    }

    /// Moves `steps` rows down, wrapping around.
    fn move_row(&mut self, steps: usize) {
        let index = ROWS.iter().position(|row| *row == self.row).unwrap_or(0);
        self.row = ROWS[(index + steps) % ROWS.len()];
    }

    fn edit_row(&mut self, key: KeyEvent, tags: &TagList) {
        let typed = match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(c),
            _ => None,
        };
        match self.row {
            Row::Sort => {
                let index = Sort::ALL
                    .iter()
                    .position(|sort| *sort == self.draft.sort)
                    .unwrap_or(0);
                let steps = match key.code {
                    KeyCode::Left => Sort::ALL.len() - 1,
                    KeyCode::Right | KeyCode::Char(' ') => 1,
                    _ => return,
                };
                self.draft.sort = Sort::ALL[(index + steps) % Sort::ALL.len()];
            }
            Row::Status => {
                if let Some(status) = pick(&mut self.status, Status::ALL.len(), key.code) {
                    self.draft.toggle_status(Status::ALL[status]);
                }
            }
            Row::Demographic => {
                let count = Demographic::ALL.len();
                if let Some(demographic) = pick(&mut self.demographic, count, key.code) {
                    self.draft.toggle_demographic(Demographic::ALL[demographic]);
                }
            }
            Row::Year => match (key.code, typed) {
                (_, Some(digit)) if digit.is_ascii_digit() && self.year.len() < 4 => {
                    self.year.push(digit)
                }
                (KeyCode::Backspace, _) => {
                    self.year.pop();
                }
                _ => {}
            },
            Row::Tags => {
                let matches = matching(tags, &self.finder);
                let last = matches.len().saturating_sub(1);
                match (key.code, typed) {
                    (KeyCode::Left, _) => self.tag = self.tag.saturating_sub(1),
                    (KeyCode::Right, _) => self.tag = (self.tag + 1).min(last),
                    (KeyCode::Home, _) => self.tag = 0,
                    (KeyCode::End, _) => self.tag = last,
                    (KeyCode::Backspace, _) => {
                        self.finder.pop();
                        self.tag = 0;
                    }
                    (_, Some(' ')) => {
                        if let Some(tag) = matches.get(self.tag) {
                            self.draft.cycle_tag(tag);
                        }
                    }
                    (_, Some(c)) => {
                        self.finder.push(c);
                        self.tag = 0;
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, tags: &TagList) {
        let width = WIDTH.min(area.width);
        let room = (width as usize).saturating_sub(2 + LABEL);
        let mut lines = vec![Line::default()];

        let sort = vec![
            Span::raw("◂ ").dark_gray(),
            Span::raw(self.draft.sort.label()).bold(),
            Span::raw(" ▸").dark_gray(),
        ];
        lines.push(self.row_line(Row::Sort, "Sort", sort));
        let statuses = Status::ALL
            .iter()
            .map(|status| (status.as_str(), self.draft.statuses.contains(status)));
        let options = self.options(Row::Status, self.status, statuses);
        lines.push(self.row_line(Row::Status, "Status", options));
        let demographics = Demographic::ALL
            .iter()
            .map(|d| (d.as_str(), self.draft.demographics.contains(d)));
        let options = self.options(Row::Demographic, self.demographic, demographics);
        lines.push(self.row_line(Row::Demographic, "Demographic", options));
        let mut year = match self.year.as_str() {
            "" if self.row != Row::Year => vec![Span::raw("any").dark_gray()],
            typed => vec![Span::raw(typed.to_owned())],
        };
        if self.row == Row::Year {
            year.push(Span::raw("▏"));
        }
        lines.push(self.row_line(Row::Year, "Year", year));

        let chosen: Vec<Span> = self
            .draft
            .included_tags
            .iter()
            .map(|tag| Span::raw(format!("+{}", tag.name)).green())
            .chain(
                self.draft
                    .excluded_tags
                    .iter()
                    .map(|tag| Span::raw(format!("−{}", tag.name)).red()),
            )
            .flat_map(|span| [span, Span::raw("  ")])
            .collect();
        let chosen = if chosen.is_empty() {
            vec![Span::raw("any").dark_gray()]
        } else {
            chosen
        };
        lines.push(self.row_line(Row::Tags, "Tags", chosen));
        lines.extend(self.tag_lines(tags, room));

        lines.push(Line::default());
        let row_keys: &[(&str, &str)] = match self.row {
            Row::Sort => &[("←→", "change")],
            Row::Status | Row::Demographic => &[("←→", "move"), ("Space", "toggle")],
            Row::Year => &[("0-9", "type a year"), ("Backspace", "erase")],
            Row::Tags => &[
                ("type", "to find"),
                ("←→", "move"),
                ("Space", "include/exclude/off"),
            ],
        };
        lines.push(key_hints(row_keys));
        lines.push(key_hints(&[
            ("↑↓", "row"),
            ("Del", "reset all"),
            ("Enter", "search"),
            ("Esc", "cancel"),
        ]));

        let height = (lines.len() as u16 + 2).min(area.height);
        let [area] = Layout::horizontal([Constraint::Length(width)])
            .flex(Flex::Center)
            .areas(area);
        let [area] = Layout::vertical([Constraint::Length(height)])
            .flex(Flex::Center)
            .areas(area);
        frame.render_widget(Clear, area);
        frame.render_widget(Paragraph::new(lines).block(panel(" Filters ", true)), area);
    }

    /// A row with the cursor, its label, and then `values`.
    fn row_line<'a>(&self, row: Row, label: &'a str, values: Vec<Span<'a>>) -> Line<'a> {
        let current = self.row == row;
        let cursor = if current { "▶ ".cyan() } else { "  ".into() };
        let label = Span::raw(format!("{label:<width$}", width = LABEL - 2));
        let mut spans = vec![cursor, if current { label.bold() } else { label }];
        spans.extend(values);
        Line::from(spans)
    }

    /// Checkboxes, with the highlighted one marked while its row is current.
    fn options<'a>(
        &self,
        row: Row,
        cursor: usize,
        options: impl Iterator<Item = (&'a str, bool)>,
    ) -> Vec<Span<'a>> {
        options
            .enumerate()
            .flat_map(|(index, (name, on))| {
                let check = if on { "[x] " } else { "[ ] " };
                let option = Span::raw(format!("{check}{name}"));
                let option = if self.row == row && index == cursor {
                    option.reversed()
                } else if on {
                    option.cyan()
                } else {
                    option
                };
                [option, Span::raw("  ")]
            })
            .collect()
    }

    /// The finder and the tags it matches, a few lines of them around the highlighted one.
    fn tag_lines<'a>(&'a self, tags: &'a TagList, room: usize) -> Vec<Line<'a>> {
        let indent = || Span::raw(" ".repeat(LABEL));
        let all = match tags {
            TagList::Loaded(all) => all,
            TagList::NotLoaded | TagList::Loading => {
                return vec![Line::from(vec![indent(), "Loading tags…".dark_gray()])];
            }
            TagList::Failed(err) => {
                let message = format!("Couldn't load tags: {err}").red();
                return vec![Line::from(vec![indent(), message])];
            }
        };
        let matches = matching(tags, &self.finder);
        let current = self.row == Row::Tags;
        let mut finder = vec![indent(), Span::raw("find: ").dark_gray()];
        finder.push(Span::raw(self.finder.as_str()));
        if current {
            finder.push(Span::raw("▏"));
        }
        finder.push(Span::raw(format!("   {} of {} tags", matches.len(), all.len())).dark_gray());
        let mut lines = vec![Line::from(finder)];
        if matches.is_empty() {
            return lines;
        }

        let labels: Vec<Span> = matches
            .iter()
            .enumerate()
            .map(|(index, tag)| {
                let label = match self.draft.tag_filter(tag) {
                    TagFilter::Off => Span::raw(tag.name.as_str()),
                    TagFilter::Include => Span::raw(format!("+{}", tag.name)).green(),
                    TagFilter::Exclude => Span::raw(format!("−{}", tag.name)).red(),
                };
                if current && index == self.tag {
                    label.patch_style(Style::new().reversed())
                } else {
                    label
                }
            })
            .collect();
        let rows = flow(&labels, room);
        let at = rows
            .iter()
            .position(|row| row.contains(&self.tag))
            .unwrap_or(0);
        let first = (at + 1).saturating_sub(TAG_LINES);
        for row in rows.iter().skip(first).take(TAG_LINES) {
            let mut spans = vec![indent()];
            for index in row.clone() {
                spans.push(labels[index].clone());
                spans.push(Span::raw("  "));
            }
            lines.push(Line::from(spans));
        }
        lines
    }
}

/// Moves the highlight in a row of `count` checkboxes; returns the one Space toggles.
fn pick(cursor: &mut usize, count: usize, code: KeyCode) -> Option<usize> {
    match code {
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(count - 1),
        KeyCode::Char(' ') => return Some(*cursor),
        _ => {}
    }
    None
}

/// The tags whose name contains `finder`, ignoring case.
fn matching<'a>(tags: &'a TagList, finder: &str) -> Vec<&'a Tag> {
    let TagList::Loaded(tags) = tags else {
        return Vec::new();
    };
    let finder = finder.to_lowercase();
    tags.iter()
        .filter(|tag| tag.name.to_lowercase().contains(&finder))
        .collect()
}

/// Splits `labels` into lines of at most `width` columns, two spaces apart.
fn flow(labels: &[Span], width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (index, label) in labels.iter().enumerate() {
        let needed = label.width() + 2;
        if used + needed > width && index > start {
            rows.push(start..index);
            (start, used) = (index, 0);
        }
        used += needed;
    }
    rows.push(start..labels.len());
    rows
}

/// The filters in a few words for the search box's border, or `None` when there are none.
pub fn summary(filters: &SearchFilters) -> Option<String> {
    let mut parts = Vec::new();
    if filters.sort != Sort::default() {
        parts.push(filters.sort.label().to_owned());
    }
    if !filters.statuses.is_empty() {
        let statuses: Vec<_> = filters.statuses.iter().map(|s| s.as_str()).collect();
        parts.push(statuses.join(", "));
    }
    if !filters.demographics.is_empty() {
        let demographics: Vec<_> = filters
            .demographics
            .iter()
            .map(|d| match d {
                Demographic::None => "no demographic",
                d => d.as_str(),
            })
            .collect();
        parts.push(demographics.join(", "));
    }
    if let Some(year) = filters.year {
        parts.push(year.to_string());
    }
    let tags: Vec<String> = filters
        .included_tags
        .iter()
        .map(|tag| format!("+{}", tag.name))
        .chain(
            filters
                .excluded_tags
                .iter()
                .map(|tag| format!("−{}", tag.name)),
        )
        .collect();
    if !tags.is_empty() {
        parts.push(tags.join(" "));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::buffer_text;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn tags() -> TagList {
        let names = [
            "Action", "Comedy", "Drama", "Dragons", "Fantasy", "Romance", "Sci-Fi",
        ];
        TagList::Loaded(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| Tag {
                    id: format!("t{index}"),
                    name: (*name).to_owned(),
                })
                .collect(),
        )
    }

    fn press(popup: &mut FilterPopup, tags: &TagList, codes: &[KeyCode]) {
        for code in codes {
            assert!(popup.handle_key(key(*code), tags).is_none());
        }
    }

    fn draw(popup: &FilterPopup, tags: &TagList) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| popup.render(frame, frame.area(), tags))
            .unwrap();
        buffer_text(&terminal)
    }

    fn searched(popup: &mut FilterPopup, tags: &TagList) -> SearchFilters {
        match popup.handle_key(key(KeyCode::Enter), tags) {
            Some(Closed::Search(filters)) => filters,
            _ => panic!("expected a search"),
        }
    }

    #[test]
    fn edits_every_filter_and_searches_with_them() {
        let tags = tags();
        let mut popup = FilterPopup::new(&SearchFilters::default());
        use KeyCode::{Backspace, Char, Down, Right, Up};
        // Sort: two to the right; status: completed; demographic: seinen and none.
        press(&mut popup, &tags, &[Right, Right, Down]);
        press(&mut popup, &tags, &[Right, Char(' '), Down]);
        press(
            &mut popup,
            &tags,
            &[Right, Right, Char(' '), Right, Right, Right],
        );
        press(&mut popup, &tags, &[Char(' '), Down]);
        // Year: letters are ignored, and a fifth digit too.
        press(
            &mut popup,
            &tags,
            &[Char('1'), Char('x'), Char('9'), Char('8')],
        );
        press(&mut popup, &tags, &[Char('9'), Char('0'), Down]);
        // Tags: "dra" finds Drama and Dragons; include Dragons, exclude Romance.
        press(&mut popup, &tags, &[Char('d'), Char('r'), Char('a')]);
        let text = draw(&popup, &tags);
        assert!(text.contains("find: dra▏   2 of 7 tags"), "{text}");
        assert!(text.contains("Drama  Dragons"));
        assert!(!text.contains("Action"));
        press(&mut popup, &tags, &[Right, Char(' ')]);
        press(
            &mut popup,
            &tags,
            &[Backspace, Backspace, Backspace, Char('r'), Char('o')],
        );
        press(&mut popup, &tags, &[Char(' '), Char(' ')]);

        let text = draw(&popup, &tags);
        assert!(text.contains("▶ Tags         +Dragons  −Romance"), "{text}");
        assert!(text.contains("  Sort         ◂ Highest rated ▸"));
        assert!(text.contains("[x] completed"));
        assert!(text.contains("  Year         1989"));

        let filters = searched(&mut popup, &tags);
        assert_eq!(filters.sort, Sort::HighestRated);
        assert_eq!(filters.statuses, [Status::Completed]);
        assert_eq!(
            filters.demographics,
            [Demographic::Seinen, Demographic::None]
        );
        assert_eq!(filters.year, Some(1989));
        assert_eq!(filters.included_tags[0].name, "Dragons");
        assert_eq!(filters.excluded_tags[0].name, "Romance");
        assert_eq!(
            summary(&filters).as_deref(),
            Some("Highest rated · completed · seinen, no demographic · 1989 · +Dragons −Romance")
        );

        // Up wraps from the first row to Tags; Delete resets everything.
        let mut popup = FilterPopup::new(&filters);
        press(&mut popup, &tags, &[Up, KeyCode::Delete]);
        assert_eq!(popup.row, Row::Tags);
        assert_eq!(searched(&mut popup, &tags), SearchFilters::default());
        assert_eq!(summary(&SearchFilters::default()), None);
    }

    #[test]
    fn cancelling_keeps_nothing_and_a_partial_year_is_any() {
        let tags = TagList::Failed("rate limited".to_owned());
        let mut popup = FilterPopup::new(&SearchFilters::default());
        press(&mut popup, &tags, &[KeyCode::Up, KeyCode::Char('x')]);
        let text = draw(&popup, &tags);
        assert!(text.contains("Couldn't load tags: rate limited"));
        assert!(matches!(
            popup.handle_key(key(KeyCode::Esc), &tags),
            Some(Closed::Cancel)
        ));

        press(
            &mut popup,
            &tags,
            &[KeyCode::Up, KeyCode::Char('2'), KeyCode::Char('0')],
        );
        assert_eq!(searched(&mut popup, &tags).year, None);
    }

    #[test]
    fn shows_a_few_lines_of_tags_around_the_highlighted_one() {
        let many = TagList::Loaded(
            (0..60)
                .map(|index| Tag {
                    id: format!("t{index}"),
                    name: format!("Tag number {index:02}"),
                })
                .collect(),
        );
        let mut popup = FilterPopup::new(&SearchFilters::default());
        press(&mut popup, &many, &[KeyCode::Up]);
        let text = draw(&popup, &many);
        assert!(text.contains("Tag number 00"));
        assert!(!text.contains("Tag number 59"));
        press(&mut popup, &many, &[KeyCode::End]);
        let text = draw(&popup, &many);
        assert!(text.contains("Tag number 59"));
        assert!(!text.contains("Tag number 00"));
    }
}
