//! Background chapter downloads: one chapter at a time, a few pages at once.
//!
//! Pages go into a `<chapter>.<quality>.part` folder first, so an interrupted, failed or
//! paused chapter picks up where it stopped. Cancelling a chapter deletes that folder.

mod files;
mod saved;

pub use files::{released_by, sanitize};
pub use saved::{SavedChapter, SavedQueue};

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::{future, stream};
use tokio::fs;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, watch};

use crate::api::{Chapter, Client, Manga, PageSource, Quality};
use crate::library::link::MangaLink;

const PAGE_CONCURRENCY: usize = 4;
/// Each attempt asks MangaDex@Home for a fresh server, since the last one may be failing.
const ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    shared: Arc<Shared>,
    root: PathBuf,
    /// Folders linked to a MangaDex manga, by manga id.
    folders: HashMap<String, PathBuf>,
}

impl Downloader {
    pub fn spawn(client: Client, root: PathBuf, updates: UnboundedSender<Update>) -> Self {
        Self::spawn_with(client, root, updates)
    }

    fn spawn_with(source: impl Source, root: PathBuf, updates: UnboundedSender<Update>) -> Self {
        let shared = Arc::new(Shared::default());
        tokio::spawn(worker(source, shared.clone(), updates));
        Downloader {
            shared,
            root,
            folders: HashMap::new(),
        }
    }

    pub fn enqueue(&self, job: Job) {
        self.shared.lock().jobs.push_back(job);
        self.shared.wake.notify_one();
    }

    /// Drops `job`, whether it's queued, downloading or failed, and deletes the pages it saved.
    pub fn cancel(&self, job: &Job) {
        let mut queue = self.shared.lock();
        if let Some((running, stop)) = &queue.running
            && running.id == job.id
        {
            // The worker deletes them once the pages in flight are saved.
            stop.send_replace(Some(Stop::Cancel));
            return;
        }
        queue.jobs.retain(|queued| queued.id != job.id);
        if !queue.shares_files(job) {
            tokio::spawn(delete_partial_files(job.clone()));
        }
    }

    /// While paused nothing new starts, and the running chapter stops once its pages in flight
    /// are saved, keeping them; it's first in line again when downloads resume.
    pub fn set_paused(&self, paused: bool) {
        let mut queue = self.shared.lock();
        queue.paused = paused;
        if let Some((_, stop)) = &queue.running
            && *stop.borrow() != Some(Stop::Cancel)
        {
            // Resuming before the chapter stopped lets it carry on.
            stop.send_replace(paused.then_some(Stop::Pause));
        }
        drop(queue);
        if !paused {
            self.shared.wake.notify_one();
        }
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

/// The queue, shared by the [`Downloader`] and its worker.
#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    /// Wakes the worker when a job is queued or downloads resume.
    wake: Notify,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    paused: bool,
    /// The job downloading now, and how to stop it.
    running: Option<(Job, watch::Sender<Option<Stop>>)>,
}

impl Queue {
    /// Whether another queued or running job saves into the same partial files as `job`:
    /// cancelling one copy of a chapter mustn't delete the pages of the other.
    fn shares_files(&self, job: &Job) -> bool {
        let (pages, cbz) = partial_files(job);
        self.jobs
            .iter()
            .chain(self.running.as_ref().map(|(running, _)| running))
            .filter(|other| other.id != job.id)
            .any(|other| {
                let (other_pages, other_cbz) = partial_files(other);
                other_pages == pages || other_cbz == cbz
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Back to the front of the queue, keeping the pages saved so far.
    Pause,
    /// Gone, along with the pages saved so far.
    Cancel,
}

/// Tells a running download whether it's been asked to stop.
struct StopSignal(watch::Receiver<Option<Stop>>);

impl StopSignal {
    fn get(&self) -> Option<Stop> {
        *self.0.borrow()
    }

    /// Waits for `future`, unless asked to stop first.
    async fn unless_stopped<T>(&self, future: impl Future<Output = T>) -> Result<T, Interrupt> {
        let mut stop = self.0.clone();
        tokio::select! {
            output = future => Ok(output),
            _ = stop.wait_for(Option::is_some) => Err(Interrupt::Stopped),
        }
    }
}

/// Why a download ended without the chapter.
enum Interrupt {
    /// Asked to stop; the worker decides what that means from the latest [`Stop`].
    Stopped,
    Failed(String),
}

impl From<String> for Interrupt {
    fn from(err: String) -> Self {
        Interrupt::Failed(err)
    }
}

/// Where chapters are downloaded from: MangaDex, or a stand-in in tests.
trait Source: Send + Sync + 'static {
    fn page_source(
        &self,
        chapter_id: &str,
        quality: Quality,
    ) -> impl Future<Output = Result<PageSource, String>> + Send;

    fn fetch_page(&self, url: &str) -> impl Future<Output = Result<Bytes, String>> + Send;

    fn fetch_cover(
        &self,
        manga_id: &str,
        file_name: &str,
    ) -> impl Future<Output = Result<Bytes, String>> + Send;
}

impl Source for Client {
    async fn page_source(&self, chapter_id: &str, quality: Quality) -> Result<PageSource, String> {
        Client::page_source(self, chapter_id, quality)
            .await
            .map_err(|err| err.to_string())
    }

    async fn fetch_page(&self, url: &str) -> Result<Bytes, String> {
        Client::fetch_page(self, url)
            .await
            .map_err(|err| err.to_string())
    }

    async fn fetch_cover(&self, manga_id: &str, file_name: &str) -> Result<Bytes, String> {
        Client::fetch_cover(self, manga_id, file_name)
            .await
            .map_err(|err| err.to_string())
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

async fn worker(source: impl Source, shared: Arc<Shared>, updates: UnboundedSender<Update>) {
    loop {
        let (job, stop) = next_job(&shared).await;
        let notify = |event| {
            let _ = updates.send(Update {
                job_id: job.id,
                event,
            });
        };
        let result = download(&source, &job, &notify, &stop).await;
        let delete = {
            let mut queue = shared.lock();
            queue.running = None;
            match result {
                // Even when cancelled at the last moment: the chapter is saved either way.
                Ok(event) => {
                    notify(event);
                    false
                }
                // The latest request counts, e.g. a cancel after a pause.
                Err(_) if stop.get() == Some(Stop::Cancel) => !queue.shares_files(&job),
                // Paused, or resumed too late to carry on.
                Err(Interrupt::Stopped) => {
                    queue.jobs.push_front(job.clone());
                    false
                }
                Err(Interrupt::Failed(err)) => {
                    notify(Event::Failed(err));
                    false
                }
            }
        };
        if delete {
            delete_partial_files(job).await;
        }
    }
}

/// Waits until a job is queued and downloads aren't paused, and marks it running.
async fn next_job(shared: &Shared) -> (Job, StopSignal) {
    loop {
        {
            let mut queue = shared.lock();
            if !queue.paused
                && let Some(job) = queue.jobs.pop_front()
            {
                let (stop, signal) = watch::channel(None);
                queue.running = Some((job.clone(), stop));
                return (job, StopSignal(signal));
            }
        }
        shared.wake.notified().await;
    }
}

async fn download(
    source: &impl Source,
    job: &Job,
    notify: &impl Fn(Event),
    stop: &StopSignal,
) -> Result<Event, Interrupt> {
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
    let (page_dir, partial_cbz) = partial_files(job);
    fs::create_dir_all(&page_dir)
        .await
        .map_err(|err| format!("couldn't create {}: {err}", page_dir.display()))?;
    save_cover(source, manga_dir, &job.manga).await;

    let pages = fetch_pages(source, job, &page_dir, notify, stop).await?;
    // The pages in flight when it was cancelled may have been the last ones.
    if stop.get() == Some(Stop::Cancel) {
        return Err(Interrupt::Stopped);
    }

    match job.format {
        OutputFormat::Cbz => {
            let comic_info = files::comic_info(&job.manga, &job.chapter, pages.len());
            let (pages_from, partial_dest) = (page_dir.clone(), partial_cbz.clone());
            tokio::task::spawn_blocking(move || {
                files::write_cbz(&pages_from, &pages, &comic_info, &partial_dest)
            })
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| format!("couldn't write the CBZ: {err}"))?;
            if stop.get() == Some(Stop::Cancel) {
                return Err(Interrupt::Stopped);
            }
            fs::rename(&partial_cbz, &final_path)
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
    source: &impl Source,
    job: &Job,
    page_dir: &Path,
    notify: &impl Fn(Event),
    stop: &StopSignal,
) -> Result<Vec<String>, Interrupt> {
    let mut failure = String::new();
    for attempt in 1..=ATTEMPTS {
        let at_home = stop
            .unless_stopped(source.page_source(&job.chapter.id, job.quality))
            .await??;
        let total = at_home.files.len();
        if total == 0 {
            return Err(Interrupt::Failed(
                "MangaDex returned no pages for this chapter".to_owned(),
            ));
        }
        let names: Vec<String> = at_home
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| files::page_name(index, total, file))
            .collect();
        let missing: Vec<(PathBuf, String)> = names
            .iter()
            .map(|name| page_dir.join(name))
            .zip(at_home.page_urls())
            .filter(|(path, _)| !path.exists())
            .collect();

        let mut done = total - missing.len();
        notify(Event::Progress { done, total });
        // Once asked to stop, the pages in flight are still saved (and reported to
        // MangaDex@Home), but no more start.
        let mut saves = stream::iter(missing)
            .take_while(|_| future::ready(stop.get().is_none()))
            .map(|(path, url)| async move { save_page(source, &url, &path).await })
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
        if done == total {
            return Ok(names);
        }
        if done + failed < total || stop.get().is_some() {
            return Err(Interrupt::Stopped);
        }
        if attempt < ATTEMPTS {
            let delay = Duration::from_secs(2 * u64::from(attempt));
            stop.unless_stopped(tokio::time::sleep(delay)).await?;
        }
    }
    Err(Interrupt::Failed(failure))
}

async fn save_page(source: &impl Source, url: &str, path: &Path) -> Result<(), String> {
    let bytes = source.fetch_page(url).await?;
    // Write under a temporary name so a half-written page never counts as done.
    let partial = path.with_extension("tmp");
    fs::write(&partial, &bytes)
        .await
        .map_err(|err| err.to_string())?;
    fs::rename(&partial, path)
        .await
        .map_err(|err| err.to_string())
}

/// The pages saved so far and the CBZ being written, which cancelling deletes. The page
/// folder is named per quality so a resumed download never mixes image qualities.
fn partial_files(job: &Job) -> (PathBuf, PathBuf) {
    let stem = files::chapter_stem(&job.chapter);
    let pages = job
        .dir
        .join(format!("{stem}.{}.part", job.quality.as_str()));
    (pages, job.dir.join(format!("{stem}.cbz.part")))
}

async fn delete_partial_files(job: Job) {
    let (pages, cbz) = partial_files(&job);
    let _ = fs::remove_dir_all(pages).await;
    let _ = fs::remove_file(cbz).await;
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
async fn save_cover(source: &impl Source, manga_dir: &Path, manga: &Manga) {
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
    if let Ok(bytes) = source.fetch_cover(&manga.id, file_name).await {
        let _ = fs::write(&path, &bytes).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    /// Serves chapters of `pages` pages. Each page waits for a permit, so tests decide when
    /// pages arrive.
    #[derive(Clone)]
    struct FakeSource {
        pages: usize,
        permits: Arc<Semaphore>,
        /// Page sources asked for, and page fetches started.
        sources: Arc<AtomicUsize>,
        fetches: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn new(pages: usize) -> Self {
            FakeSource {
                pages,
                permits: Arc::new(Semaphore::new(0)),
                sources: Arc::default(),
                fetches: Arc::default(),
            }
        }

        fn release(&self, pages: usize) {
            self.permits.add_permits(pages);
        }

        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    impl Source for FakeSource {
        async fn page_source(
            &self,
            chapter_id: &str,
            quality: Quality,
        ) -> Result<PageSource, String> {
            self.sources.fetch_add(1, Ordering::SeqCst);
            Ok(PageSource {
                base_url: "https://fake.test".to_owned(),
                hash: chapter_id.to_owned(),
                quality,
                files: (1..=self.pages).map(|page| format!("{page}.png")).collect(),
            })
        }

        async fn fetch_page(&self, _url: &str) -> Result<Bytes, String> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.permits.acquire().await.unwrap().forget();
            Ok(Bytes::from_static(b"page"))
        }

        async fn fetch_cover(&self, _manga_id: &str, _file_name: &str) -> Result<Bytes, String> {
            Err("no cover".to_owned())
        }
    }

    fn manga() -> Manga {
        Manga {
            id: "m1".to_owned(),
            title: "Berserk".to_owned(),
            description: None,
            status: None,
            year: None,
            content_rating: ContentRating::Erotica,
            languages: vec![],
            authors: vec![],
            cover_file: None,
        }
    }

    fn job(id: u64, dir: &Path, number: &str, format: OutputFormat) -> Job {
        Job {
            id,
            dir: dir.join("Berserk"),
            manga: manga(),
            chapter: Chapter {
                id: format!("c{number}"),
                volume: None,
                number: Some(number.to_owned()),
                title: None,
                language: "en".to_owned(),
                pages: 6,
                groups: vec!["TAMT".to_owned()],
                publish_at: String::new(),
            },
            quality: Quality::DataSaver,
            format,
        }
    }

    fn start(source: &FakeSource, root: &Path) -> (Downloader, UnboundedReceiver<Update>) {
        let (updates, events) = mpsc::unbounded_channel();
        let downloader = Downloader::spawn_with(source.clone(), root.to_owned(), updates);
        (downloader, events)
    }

    /// The queued job ids and the running one.
    fn queue(downloader: &Downloader) -> (Vec<u64>, Option<u64>) {
        let queue = downloader.shared.lock();
        let jobs = queue.jobs.iter().map(|job| job.id).collect();
        (jobs, queue.running.as_ref().map(|(job, _)| job.id))
    }

    async fn eventually(what: &str, check: impl Fn() -> bool) {
        for _ in 0..500 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting until {what}");
    }

    /// Every event until `job_id` ends, which must be by finishing.
    async fn until_finished(events: &mut UnboundedReceiver<Update>, job_id: u64) -> Vec<Update> {
        let mut seen = Vec::new();
        loop {
            let update = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("timed out waiting for the download")
                .expect("the worker is running");
            let done = update.job_id == job_id
                && match &update.event {
                    Event::Finished(_) => true,
                    Event::Progress { .. } => false,
                    other => panic!("job {job_id} ended with {other:?}"),
                };
            seen.push(update);
            if done {
                return seen;
            }
        }
    }

    fn files_in(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| entries.count())
    }

    #[tokio::test]
    async fn pausing_keeps_the_pages_and_carries_on_later() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(6);
        let (downloader, mut events) = start(&source, library.path());
        let job = job(1, library.path(), "1", OutputFormat::Folder);
        let (page_dir, _) = partial_files(&job);
        downloader.enqueue(job);
        eventually("4 pages are in flight", || source.fetches() == 4).await;

        downloader.set_paused(true);
        source.release(4);
        eventually("it's back in the queue", || {
            queue(&downloader) == (vec![1], None)
        })
        .await;
        assert_eq!(files_in(&page_dir), 4, "the pages in flight are saved");
        assert_eq!(source.fetches(), 4, "no page starts once paused");

        downloader.set_paused(false);
        source.release(2);
        let updates = until_finished(&mut events, 1).await;
        let Some(Update {
            event: Event::Finished(path),
            ..
        }) = updates.last()
        else {
            unreachable!()
        };
        assert_eq!(files_in(path), 6);
        assert!(!page_dir.exists());
        assert_eq!(
            source.fetches(),
            6,
            "only the missing pages are fetched again"
        );
    }

    #[tokio::test]
    async fn resuming_before_the_chapter_stopped_lets_it_carry_on() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(6);
        let (downloader, mut events) = start(&source, library.path());
        downloader.enqueue(job(1, library.path(), "1", OutputFormat::Folder));
        eventually("4 pages are in flight", || source.fetches() == 4).await;

        downloader.set_paused(true);
        downloader.set_paused(false);
        source.release(6);
        until_finished(&mut events, 1).await;
        assert_eq!(
            source.sources.load(Ordering::SeqCst),
            1,
            "it never restarted"
        );
        assert_eq!(source.fetches(), 6);
    }

    #[tokio::test]
    async fn cancelling_a_running_chapter_deletes_its_pages_and_moves_on() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(6);
        let (downloader, mut events) = start(&source, library.path());
        let first = job(1, library.path(), "1", OutputFormat::Cbz);
        let (page_dir, _) = partial_files(&first);
        downloader.enqueue(first.clone());
        downloader.enqueue(job(2, library.path(), "2", OutputFormat::Cbz));
        eventually("4 pages are in flight", || source.fetches() == 4).await;

        downloader.cancel(&first);
        source.release(4);
        eventually("the next chapter starts", || source.fetches() == 8).await;
        eventually("the pages are deleted", || !page_dir.exists()).await;

        source.release(6);
        let updates = until_finished(&mut events, 2).await;
        assert!(
            updates
                .iter()
                .filter(|update| update.job_id == 1)
                .all(|update| matches!(update.event, Event::Progress { .. })),
            "a cancelled chapter neither finishes nor fails"
        );
        let names: Vec<String> = std::fs::read_dir(library.path().join("Berserk"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect();
        assert_eq!(names, ["Ch. 0002 [TAMT].cbz"]);
    }

    #[tokio::test]
    async fn a_chapter_cancelled_on_its_last_pages_is_not_saved() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(4);
        let (downloader, mut events) = start(&source, library.path());
        // As a folder, so nothing after the pages could catch the cancel instead.
        let job = job(1, library.path(), "1", OutputFormat::Folder);
        let (page_dir, partial_cbz) = partial_files(&job);
        downloader.enqueue(job.clone());
        eventually("every page is in flight", || source.fetches() == 4).await;

        downloader.cancel(&job);
        source.release(4);
        eventually("it's stopped and its pages deleted", || {
            queue(&downloader) == (vec![], None) && !page_dir.exists()
        })
        .await;
        assert!(!partial_cbz.exists());
        assert!(!job.dir.join("Ch. 0001 [TAMT]").exists());
        while let Ok(update) = events.try_recv() {
            assert!(matches!(update.event, Event::Progress { .. }));
        }
    }

    #[tokio::test]
    async fn a_cancel_after_a_pause_still_deletes_the_pages() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(6);
        let (downloader, _events) = start(&source, library.path());
        let job = job(1, library.path(), "1", OutputFormat::Folder);
        let (page_dir, _) = partial_files(&job);
        downloader.enqueue(job.clone());
        eventually("4 pages are in flight", || source.fetches() == 4).await;

        downloader.set_paused(true);
        downloader.cancel(&job);
        source.release(4);
        eventually("the pages are deleted", || !page_dir.exists()).await;
        assert_eq!(queue(&downloader), (vec![], None), "it isn't queued again");
    }

    #[tokio::test]
    async fn cancelling_a_queued_chapter_deletes_pages_from_an_earlier_run() {
        let library = tempfile::tempdir().unwrap();
        let source = FakeSource::new(6);
        let (downloader, _events) = start(&source, library.path());
        downloader.set_paused(true);
        let first = job(1, library.path(), "1", OutputFormat::Folder);
        let copy = Job {
            id: 2,
            ..first.clone()
        };
        let other = job(3, library.path(), "3", OutputFormat::Cbz);
        for job in [&first, &other] {
            let (page_dir, _) = partial_files(job);
            std::fs::create_dir_all(&page_dir).unwrap();
            std::fs::write(page_dir.join("001.png"), b"page").unwrap();
        }
        let partial_cbz = partial_files(&other).1;
        std::fs::write(&partial_cbz, b"zip").unwrap();
        for job in [&first, &copy, &other] {
            downloader.enqueue(job.clone());
        }

        downloader.cancel(&first);
        downloader.cancel(&other);
        // Deletions run in order, so the first chapter's pages would be gone by now too.
        eventually("the other chapter's files are deleted", || {
            !partial_files(&other).0.exists() && !partial_cbz.exists()
        })
        .await;
        assert!(
            partial_files(&first).0.exists(),
            "the queued copy still needs its pages"
        );
        assert_eq!(queue(&downloader), (vec![2], None));
        assert_eq!(source.fetches(), 0);
    }

    #[tokio::test]
    async fn keeps_a_linked_manga_in_its_folder() {
        let (updates, _) = mpsc::unbounded_channel();
        let library = tempfile::tempdir().unwrap();
        let root = library.path().to_owned();
        let mut downloader = Downloader::spawn(Client::new().unwrap(), root.clone(), updates);
        let manga = Manga {
            title: "Kimetsu: no Yaiba".to_owned(),
            ..manga()
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
