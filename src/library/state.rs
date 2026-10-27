//! Reading state per manga (direction, last page, read chapters), stored as JSON in the app
//! data folder (`~/.local/share/toshokan/library.json` on Linux).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::write_atomically;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Japanese order: in spreads the later page is on the left.
    #[default]
    Rtl,
    Ltr,
}

impl Direction {
    pub fn toggled(self) -> Self {
        match self {
            Direction::Rtl => Direction::Ltr,
            Direction::Ltr => Direction::Rtl,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Direction::Rtl => "RTL",
            Direction::Ltr => "LTR",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    /// Index of the first page on screen when the chapter was last left.
    pub page: usize,
    pub read: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct MangaState {
    #[serde(default)]
    direction: Direction,
    /// Keyed by chapter file name.
    #[serde(default)]
    chapters: BTreeMap<String, Progress>,
}

pub struct ReadingState {
    path: PathBuf,
    manga: BTreeMap<String, MangaState>,
}

impl ReadingState {
    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("toshokan")
            .join("library.json")
    }

    /// Loads the state at `path`. An unreadable file is moved aside to `library.json.bak`
    /// and replaced with an empty state; the returned warning says so.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        let parsed = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|err| err.to_string()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(err.to_string()),
        };
        match parsed {
            Ok(manga) => (ReadingState { path, manga }, None),
            Err(err) => {
                let backup = path.with_extension("json.bak");
                let _ = fs::rename(&path, &backup);
                let warning = format!(
                    "Reading progress couldn't be loaded ({err}); the old file was moved to {}",
                    backup.display()
                );
                (
                    ReadingState {
                        path,
                        manga: BTreeMap::new(),
                    },
                    Some(warning),
                )
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn direction(&self, manga: &str) -> Direction {
        self.manga
            .get(manga)
            .map(|state| state.direction)
            .unwrap_or_default()
    }

    pub fn set_direction(&mut self, manga: &str, direction: Direction) {
        self.manga.entry(manga.to_owned()).or_default().direction = direction;
    }

    pub fn progress(&self, manga: &str, chapter: &str) -> Option<Progress> {
        self.manga.get(manga)?.chapters.get(chapter).copied()
    }

    /// Records the page being read. Once a chapter is read it stays read.
    pub fn record(&mut self, manga: &str, chapter: &str, page: usize, finished: bool) {
        let progress = self
            .manga
            .entry(manga.to_owned())
            .or_default()
            .chapters
            .entry(chapter.to_owned())
            .or_default();
        progress.page = page;
        progress.read |= finished;
    }

    pub fn save(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.manga)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        write_atomically(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_progress_and_direction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toshokan/library.json");
        let (mut state, warning) = ReadingState::load(path.clone());
        assert!(warning.is_none());
        assert_eq!(state.direction("Berserk"), Direction::Rtl);

        state.set_direction("Berserk", Direction::Ltr);
        state.record("Berserk", "Ch. 0001.cbz", 24, true);
        state.record("Berserk", "Ch. 0001.cbz", 3, false);
        state.save().unwrap();

        let (state, _) = ReadingState::load(path);
        assert_eq!(state.direction("Berserk"), Direction::Ltr);
        assert_eq!(
            state.progress("Berserk", "Ch. 0001.cbz"),
            Some(Progress {
                page: 3,
                read: true
            }),
            "a read chapter stays read"
        );
        assert_eq!(state.progress("Berserk", "Ch. 0002.cbz"), None);
    }

    #[test]
    fn moves_a_corrupt_file_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.json");
        fs::write(&path, "{not json").unwrap();
        let (state, warning) = ReadingState::load(path.clone());
        assert!(warning.unwrap().contains("library.json.bak"));
        assert_eq!(state.direction("x"), Direction::Rtl);
        assert!(dir.path().join("library.json.bak").exists());
    }
}
