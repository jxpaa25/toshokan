//! The download queue kept between runs, as JSON in the app data folder
//! (`~/.local/share/toshokan/downloads.json` on Linux): chapters still to download or that
//! failed, and whether downloads are paused.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Job, OutputFormat};
use crate::api::{Chapter, Manga, Quality};
use crate::config::{data_path, load_json, write_atomically};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SavedQueue {
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub chapters: Vec<SavedChapter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedChapter {
    pub dir: PathBuf,
    pub manga: Manga,
    pub chapter: Chapter,
    pub quality: Quality,
    pub format: OutputFormat,
    /// Why it failed; failed chapters wait to be retried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SavedChapter {
    pub fn new(job: &Job, error: Option<String>) -> Self {
        SavedChapter {
            dir: job.dir.clone(),
            manga: job.manga.clone(),
            chapter: job.chapter.clone(),
            quality: job.quality,
            format: job.format,
            error,
        }
    }

    pub fn into_job(self, id: u64) -> (Job, Option<String>) {
        let job = Job {
            id,
            dir: self.dir,
            manga: self.manga,
            chapter: self.chapter,
            quality: self.quality,
            format: self.format,
        };
        (job, self.error)
    }
}

impl SavedQueue {
    pub fn default_path() -> PathBuf {
        data_path("downloads.json")
    }

    /// Loads the queue at `path`. An unreadable file is moved aside to `downloads.json.bak`
    /// and replaced with an empty queue; the returned warning says so.
    pub fn load(path: &Path) -> (Self, Option<String>) {
        load_json(path, "The download queue")
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        write_atomically(path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;

    fn job(id: u64) -> Job {
        Job {
            id,
            dir: PathBuf::from("/manga/Berserk"),
            manga: Manga {
                id: "m1".to_owned(),
                title: "Berserk".to_owned(),
                description: Some("Guts".to_owned()),
                status: Some("ongoing".to_owned()),
                year: Some(1989),
                content_rating: ContentRating::Erotica,
                languages: vec!["en".to_owned()],
                authors: vec!["Miura Kentarou".to_owned()],
                cover_file: Some("cover.jpg".to_owned()),
            },
            chapter: Chapter {
                id: "c1".to_owned(),
                volume: Some("1".to_owned()),
                number: Some("1".to_owned()),
                title: None,
                language: "en".to_owned(),
                pages: 30,
                groups: vec!["TAMT".to_owned()],
                publish_at: "2018-01-01T00:00:00+00:00".to_owned(),
            },
            quality: Quality::DataSaver,
            format: OutputFormat::Folder,
        }
    }

    #[test]
    fn round_trips_queued_and_failed_chapters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("toshokan/downloads.json");
        let (empty, warning) = SavedQueue::load(&path);
        assert!(warning.is_none());
        assert!(!empty.paused && empty.chapters.is_empty());

        let queue = SavedQueue {
            paused: true,
            chapters: vec![
                SavedChapter::new(&job(1), None),
                SavedChapter::new(&job(2), Some("3 of 30 pages failed".to_owned())),
            ],
        };
        queue.save(&path).unwrap();
        let (loaded, warning) = SavedQueue::load(&path);
        assert!(warning.is_none());
        assert!(loaded.paused);
        let jobs: Vec<_> = loaded
            .chapters
            .into_iter()
            .zip([7, 8])
            .map(|(chapter, id)| chapter.into_job(id))
            .collect();
        let (first, error) = &jobs[0];
        assert_eq!(first.id, 7);
        assert_eq!(first.dir, PathBuf::from("/manga/Berserk"));
        assert_eq!(first.manga.authors, ["Miura Kentarou"]);
        assert_eq!(first.chapter.label(), "Vol. 1 Ch. 1");
        assert_eq!(first.chapter.groups, ["TAMT"]);
        assert_eq!(
            (first.quality, first.format),
            (Quality::DataSaver, OutputFormat::Folder)
        );
        assert_eq!(*error, None);
        assert_eq!(jobs[1].1.as_deref(), Some("3 of 30 pages failed"));
    }

    #[test]
    fn moves_a_corrupt_queue_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("downloads.json");
        std::fs::write(&path, "[").unwrap();
        let (queue, warning) = SavedQueue::load(&path);
        assert!(
            warning
                .unwrap()
                .starts_with("The download queue couldn't be loaded")
        );
        assert!(queue.chapters.is_empty());
        assert!(dir.path().join("downloads.json.bak").exists());
    }
}
