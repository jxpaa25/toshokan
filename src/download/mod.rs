//! Background chapter downloads: one chapter at a time, a few pages at once.
//!
//! Pages go into a `<chapter>.<quality>.part` folder first, so an interrupted or failed
//! chapter picks up where it stopped the next time it's downloaded.

mod files;

pub use files::{released_by, sanitize};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use tokio::fs;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::api::{Chapter, Client, Manga, Quality};
use crate::library::link::MangaLink;

const PAGE_CONCURRENCY: usize = 4;
/// Each attempt asks MangaDex@Home for a fresh server, since the last one may be failing.
const ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Cbz,
    Folder,
}

impl OutputFormat {
    /// The name used in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Cbz => "cbz",
            OutputFormat::Folder => "folder",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Cbz => "CBZ archive",
            OutputFormat::Folder => "Image folder",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            OutputFormat::Cbz => OutputFormat::Folder,
            OutputFormat::Folder => OutputFormat::Cbz,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    /// The manga folder to save into: [`Downloader::manga_dir`], or the library folder the
    /// download was started from.
    pub dir: PathBuf,
    pub manga: Manga,
    pub chapter: Chapter,
    pub quality: Quality,
    pub format: OutputFormat,
}

#[derive(Debug, Clone)]
pub enum Event {
    Progress { done: usize, total: usize },
    Finished(PathBuf),
    AlreadyDownloaded(PathBuf),
    Failed(String),
}

#[derive(Debug)]
pub struct Update {
    pub job_id: u64,
    pub event: Event,
}

/// Handle to the download worker. Jobs run in the order they're queued.
pub struct Downloader {
    jobs: UnboundedSender<Job>,
    root: PathBuf,
    /// Folders linked to a MangaDex manga, by manga id.
    folders: HashMap<String, PathBuf>,
}

impl Downloader {
    pub fn spawn(client: Client, root: PathBuf, updates: UnboundedSender<Update>) -> Self {
        let (jobs, queue) = mpsc::unbounded_channel();
        tokio::spawn(worker(client, queue, updates));
        Downloader {
            jobs,
            root,
            folders: HashMap::new(),
        }
    }

    pub fn enqueue(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder named after the manga's title. When there's none, a folder linked to the
    /// manga instead, since its title on MangaDex may have changed since it was downloaded.
    pub fn manga_dir(&self, manga: &Manga) -> PathBuf {
        let named = self.root.join(files::sanitize(&manga.title));
        match self.folders.get(&manga.id) {
            Some(linked) if !named.exists() => linked.clone(),
            _ => named,
        }
    }

    /// Remembers that `dir` holds the manga with this id.
    pub fn link_folder(&mut self, manga_id: &str, dir: PathBuf) {
        self.folders.insert(manga_id.to_owned(), dir);
    }
}

/// Where `chapter` was saved in the manga folder `dir`, in either format, if it has been
/// downloaded.
pub fn existing_path(dir: &Path, chapter: &Chapter) -> Option<PathBuf> {
    let stem = files::chapter_stem(chapter);
    [dir.join(format!("{stem}.cbz")), dir.join(stem)]
        .into_iter()
        .find(|path| path.exists())
}

async fn worker(
    client: Client,
    mut queue: UnboundedReceiver<Job>,
    updates: UnboundedSender<Update>,
) {
    while let Some(job) = queue.recv().await {
        let notify = |event| {
            let _ = updates.send(Update {
                job_id: job.id,
                event,
            });
        };
        let event = download(&client, &job, &notify)
            .await
            .unwrap_or_else(Event::Failed);
        notify(event);
    }
}

async fn download(client: &Client, job: &Job, notify: &impl Fn(Event)) -> Result<Event, String> {
    let manga_dir = &job.dir;
    fs::create_dir_all(manga_dir)
        .await
        .map_err(|err| format!("couldn't create {}: {err}", manga_dir.display()))?;
    save_link(manga_dir, job).await;
    let stem = files::chapter_stem(&job.chapter);
    let final_path = match job.format {
        OutputFormat::Cbz => manga_dir.join(format!("{stem}.cbz")),
        OutputFormat::Folder => manga_dir.join(&stem),
    };
    if fs::try_exists(&final_path).await.unwrap_or(false) {
        return Ok(Event::AlreadyDownloaded(final_path));
    }
    // Named per quality so a resumed download never mixes image qualities.
    let page_dir = manga_dir.join(format!("{stem}.{}.part", job.quality.as_str()));
    fs::create_dir_all(&page_dir)
        .await
        .map_err(|err| format!("couldn't create {}: {err}", page_dir.display()))?;
    save_cover(client, manga_dir, &job.manga).await;

    let pages = fetch_pages(client, job, &page_dir, notify).await?;

    match job.format {
        OutputFormat::Cbz => {
            let comic_info = files::comic_info(&job.manga, &job.chapter, pages.len());
            let partial = manga_dir.join(format!("{stem}.cbz.part"));
            let (pages_from, partial_dest) = (page_dir.clone(), partial.clone());
            tokio::task::spawn_blocking(move || {
                files::write_cbz(&pages_from, &pages, &comic_info, &partial_dest)
            })
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| format!("couldn't write the CBZ: {err}"))?;
            fs::rename(&partial, &final_path)
                .await
                .map_err(|err| format!("couldn't save the CBZ: {err}"))?;
            let _ = fs::remove_dir_all(&page_dir).await;
        }
        OutputFormat::Folder => fs::rename(&page_dir, &final_path)
            .await
            .map_err(|err| format!("couldn't move pages into place: {err}"))?,
    }
    Ok(Event::Finished(final_path))
}

/// Downloads every page not already in `page_dir` and returns all page file names in order.
async fn fetch_pages(
    client: &Client,
    job: &Job,
    page_dir: &Path,
    notify: &impl Fn(Event),
) -> Result<Vec<String>, String> {
    let mut failure = String::new();
    for attempt in 1..=ATTEMPTS {
        let source = client
            .page_source(&job.chapter.id, job.quality)
            .await
            .map_err(|err| err.to_string())?;
        let total = source.files.len();
        if total == 0 {
            return Err("MangaDex returned no pages for this chapter".to_owned());
        }
        let names: Vec<String> = source
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| files::page_name(index, total, file))
            .collect();
        let missing: Vec<(PathBuf, String)> = names
            .iter()
            .map(|name| page_dir.join(name))
            .zip(source.page_urls())
            .filter(|(path, _)| !path.exists())
            .collect();

        let mut done = total - missing.len();
        notify(Event::Progress { done, total });
        let mut saves = stream::iter(missing)
            .map(|(path, url)| async move { save_page(client, &url, &path).await })
            .buffer_unordered(PAGE_CONCURRENCY);
        let mut failed = 0;
        while let Some(result) = saves.next().await {
            match result {
                Ok(()) => {
                    done += 1;
                    notify(Event::Progress { done, total });
                }
                Err(err) => {
                    failed += 1;
                    failure = format!("{failed} of {total} pages failed ({err})");
                }
            }
        }
        if failed == 0 {
            return Ok(names);
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
        }
    }
    Err(failure)
}

async fn save_page(client: &Client, url: &str, path: &Path) -> Result<(), String> {
    let bytes = client
        .fetch_page(url)
        .await
        .map_err(|err| err.to_string())?;
    // Write under a temporary name so a half-written page never counts as done.
    let partial = path.with_extension("tmp");
    fs::write(&partial, &bytes)
        .await
        .map_err(|err| err.to_string())?;
    fs::rename(&partial, path)
        .await
        .map_err(|err| err.to_string())
}

/// Records which MangaDex manga the folder holds, so the library can check it for new
/// chapters. Best effort, like the cover.
async fn save_link(manga_dir: &Path, job: &Job) {
    let link = MangaLink {
        id: job.manga.id.clone(),
        language: job.chapter.language.clone(),
    };
    let dir = manga_dir.to_owned();
    let _ = tokio::task::spawn_blocking(move || link.save(&dir)).await;
}

/// Saves the series cover as `cover.<ext>` next to the chapters, which readers like Mihon
/// and Kavita use as the series thumbnail. Best effort: a missing cover isn't worth failing for.
async fn save_cover(client: &Client, manga_dir: &Path, manga: &Manga) {
    let Some(file_name) = &manga.cover_file else {
        return;
    };
    let extension = Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("jpg");
    let path = manga_dir.join(format!("cover.{extension}"));
    if fs::try_exists(&path).await.unwrap_or(false) {
        return;
    }
    if let Ok(bytes) = client.fetch_cover(&manga.id, file_name).await {
        let _ = fs::write(&path, &bytes).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;

    #[tokio::test]
    async fn keeps_a_linked_manga_in_its_folder() {
        let (updates, _) = mpsc::unbounded_channel();
        let library = tempfile::tempdir().unwrap();
        let root = library.path().to_owned();
        let mut downloader = Downloader::spawn(Client::new().unwrap(), root.clone(), updates);
        let manga = Manga {
            id: "m1".to_owned(),
            title: "Kimetsu: no Yaiba".to_owned(),
            description: None,
            status: None,
            year: None,
            content_rating: ContentRating::Safe,
            languages: vec![],
            authors: vec![],
            cover_file: None,
        };
        assert_eq!(downloader.manga_dir(&manga), root.join("Kimetsu_ no Yaiba"));
        // Downloaded back when MangaDex had another title for it.
        downloader.link_folder("m1", root.join("Demon Slayer"));
        assert_eq!(downloader.manga_dir(&manga), root.join("Demon Slayer"));
        // A folder named after the current title still wins, as before folders were linked.
        std::fs::create_dir(root.join("Kimetsu_ no Yaiba")).unwrap();
        assert_eq!(downloader.manga_dir(&manga), root.join("Kimetsu_ no Yaiba"));
    }
}
