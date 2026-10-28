//! Which MangaDex manga a folder in the library holds, saved as a hidden `.toshokan.json` in
//! the folder so it moves with it.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::write_atomically;

pub const FILE_NAME: &str = ".toshokan.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MangaLink {
    #[serde(rename = "mangadex_id")]
    pub id: String,
    /// The translation of the latest download, which is the one checked for new chapters.
    pub language: String,
    /// The name of the newest chapter the folder has held, saved when chapters are deleted:
    /// new chapters are counted from it, and the manga stays in the library with none left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_chapter: Option<String>,
}

impl MangaLink {
    /// The link saved in `dir`. A missing or unreadable file means the folder isn't linked.
    pub fn load(dir: &Path) -> Option<MangaLink> {
        let text = fs::read_to_string(dir.join(FILE_NAME)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Saves the link in `dir` unless it's already there.
    pub fn save(&self, dir: &Path) -> io::Result<()> {
        if MangaLink::load(dir).as_ref() == Some(self) {
            return Ok(());
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        write_atomically(&dir.join(FILE_NAME), &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ignores_broken_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(MangaLink::load(dir.path()), None);

        let link = MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: None,
        };
        link.save(dir.path()).unwrap();
        assert_eq!(MangaLink::load(dir.path()), Some(link));
        let text = fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(text.contains("\"mangadex_id\": \"m1\""), "{text}");

        fs::write(dir.path().join(FILE_NAME), "{").unwrap();
        assert_eq!(MangaLink::load(dir.path()), None);
    }
}
