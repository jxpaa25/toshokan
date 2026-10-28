//! Rate-limited client for the MangaDex REST API and its MangaDex@Home image servers.

mod models;

pub use models::{
    Chapter, ChapterSlot, ContentRating, Manga, PageSource, Quality, SearchPage, group_chapters,
};

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;

use models::{
    AtHomeResponse, ChapterAttributes, Collection, Entity, EntityResponse, MangaAttributes,
};

const API_BASE: &str = "https://api.mangadex.org";
const REPORT_URL: &str = "https://api.mangadex.network/report";
const COVERS_BASE: &str = "https://uploads.mangadex.org/covers";
const USER_AGENT: &str = concat!("toshokan/", env!("CARGO_PKG_VERSION"));

pub const SEARCH_PAGE_SIZE: u32 = 50;
const FEED_PAGE_SIZE: u32 = 500;
/// MangaDex rejects any request where `offset + limit` exceeds this.
const RESULT_WINDOW: u32 = 10_000;
const MAX_RETRIES: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MangaDex returned {status}: {detail}")]
    Api { status: StatusCode, detail: String },
    #[error("MangaDex is blocking requests from this IP (HTTP 403); wait a while before retrying")]
    Forbidden,
    #[error("rate limited by MangaDex; try again in a minute")]
    RateLimited,
}

pub type Result<T> = std::result::Result<T, Error>;

type Query = Vec<(&'static str, String)>;

/// Cheap to clone; clones share the same connection pool and rate limiters.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_limit: Arc<DefaultDirectRateLimiter>,
    at_home_limit: Arc<DefaultDirectRateLimiter>,
}

impl Client {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        // The documented global limit is ~5 req/s per IP; stay below it with no bursts.
        let api_quota = Quota::with_period(Duration::from_millis(250)).expect("non-zero period");
        // GET /at-home/server allows 40 requests per minute.
        let at_home_quota =
            Quota::with_period(Duration::from_millis(1500)).expect("non-zero period");
        Ok(Client {
            http,
            api_limit: Arc::new(RateLimiter::direct(api_quota)),
            at_home_limit: Arc::new(RateLimiter::direct(at_home_quota)),
        })
    }

    pub async fn search_manga(
        &self,
        title: &str,
        offset: u32,
        include_adult: bool,
    ) -> Result<SearchPage> {
        let mut query: Query = vec![
            ("title", title.to_owned()),
            ("limit", SEARCH_PAGE_SIZE.to_string()),
            ("offset", offset.to_string()),
            ("order[relevance]", "desc".to_owned()),
            ("includes[]", "cover_art".to_owned()),
            ("includes[]", "author".to_owned()),
            ("includes[]", "artist".to_owned()),
        ];
        for rating in ContentRating::allowed(include_adult) {
            query.push(("contentRating[]", rating.as_str().to_owned()));
        }
        let page: Collection<Entity<MangaAttributes>> = self.get("/manga", &query).await?;
        Ok(SearchPage {
            items: page.data.into_iter().map(Manga::from).collect(),
            offset: page.offset,
            total: page.total,
        })
    }

    pub async fn manga(&self, id: &str) -> Result<Manga> {
        let query: Query = vec![
            ("includes[]", "cover_art".to_owned()),
            ("includes[]", "author".to_owned()),
            ("includes[]", "artist".to_owned()),
        ];
        let response: EntityResponse<Entity<MangaAttributes>> =
            self.get(&format!("/manga/{id}"), &query).await?;
        Ok(response.data.into())
    }

    /// The id of the manga a chapter belongs to.
    pub async fn manga_of_chapter(&self, chapter_id: &str) -> Result<Option<String>> {
        let response: EntityResponse<Entity<serde::de::IgnoredAny>> = self
            .get(&format!("/chapter/{chapter_id}"), &Query::new())
            .await?;
        Ok(response
            .data
            .relationships
            .into_iter()
            .find(|rel| rel.kind == "manga")
            .map(|rel| rel.id))
    }

    /// Every downloadable chapter of a manga in one language, grouped by chapter number.
    ///
    /// Chapters hosted on external sites, with no pages, or marked unavailable are left out.
    pub async fn chapters(&self, manga_id: &str, language: &str) -> Result<Vec<ChapterSlot>> {
        let path = format!("/manga/{manga_id}/feed");
        let mut chapters = Vec::new();
        let mut offset = 0;
        loop {
            let mut query: Query = vec![
                ("translatedLanguage[]", language.to_owned()),
                ("limit", FEED_PAGE_SIZE.to_string()),
                ("offset", offset.to_string()),
                ("order[volume]", "asc".to_owned()),
                ("order[chapter]", "asc".to_owned()),
                ("includes[]", "scanlation_group".to_owned()),
                ("includeExternalUrl", "0".to_owned()),
                ("includeEmptyPages", "0".to_owned()),
                ("includeUnavailable", "0".to_owned()),
            ];
            // The user already chose this manga, so don't let the default rating filter hide chapters.
            for rating in ContentRating::ALL {
                query.push(("contentRating[]", rating.as_str().to_owned()));
            }
            let page: Collection<Entity<ChapterAttributes>> = self.get(&path, &query).await?;
            let fetched = page.data.len() as u32;
            chapters.extend(page.data.into_iter().map(Chapter::from));
            offset += fetched;
            if fetched == 0 || offset >= page.total || offset + FEED_PAGE_SIZE > RESULT_WINDOW {
                break;
            }
        }
        Ok(group_chapters(chapters))
    }

    /// Asks MangaDex@Home where to download a chapter's pages from.
    pub async fn page_source(&self, chapter_id: &str, quality: Quality) -> Result<PageSource> {
        self.at_home_limit.until_ready().await;
        let response: AtHomeResponse = self
            .get(&format!("/at-home/server/{chapter_id}"), &Query::new())
            .await?;
        Ok(PageSource::new(response, quality))
    }

    /// Downloads one page image and reports the result to MangaDex@Home as its docs ask.
    ///
    /// An HTTP 403 here usually means the page source expired; request a new one.
    pub async fn fetch_page(&self, url: &str) -> Result<Bytes> {
        let started = Instant::now();
        let result = self.download_image(url).await;
        if !is_mangadex_org(url) {
            let (success, cached, bytes) = match &result {
                Ok((bytes, cached)) => (true, *cached, bytes.len()),
                Err(_) => (false, false, 0),
            };
            self.report(url, success, cached, bytes, started.elapsed());
        }
        result.map(|(bytes, _)| bytes)
    }

    pub async fn fetch_cover(&self, manga_id: &str, file_name: &str) -> Result<Bytes> {
        let response = self
            .http
            .get(cover_url(manga_id, file_name, None))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.bytes().await?)
    }

    async fn download_image(&self, url: &str) -> Result<(Bytes, bool)> {
        let response = self.http.get(url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Api {
                status,
                detail: "image server error".to_owned(),
            });
        }
        let cached = response
            .headers()
            .get("x-cache")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("HIT"));
        Ok((response.bytes().await?, cached))
    }

    fn report(&self, url: &str, success: bool, cached: bool, bytes: usize, duration: Duration) {
        let request = self.http.post(REPORT_URL).json(&serde_json::json!({
            "url": url,
            "success": success,
            "cached": cached,
            "bytes": bytes,
            "duration": duration.as_millis() as u64,
        }));
        // Reports are best effort and must not slow down or fail the download.
        tokio::spawn(async move {
            let _ = request.send().await;
        });
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &Query) -> Result<T> {
        let url = format!("{API_BASE}{path}");
        let mut attempt = 0;
        loop {
            self.api_limit.until_ready().await;
            let response = self.http.get(&url).query(query).send().await?;
            match response.status() {
                status if status.is_success() => return Ok(response.json().await?),
                StatusCode::TOO_MANY_REQUESTS if attempt < MAX_RETRIES => {
                    tokio::time::sleep(retry_delay(response.headers(), attempt)).await;
                    attempt += 1;
                }
                StatusCode::TOO_MANY_REQUESTS => return Err(Error::RateLimited),
                // Retrying while blocked extends the block, so give up immediately.
                StatusCode::FORBIDDEN => return Err(Error::Forbidden),
                status => {
                    let detail = error_detail(response).await;
                    return Err(Error::Api { status, detail });
                }
            }
        }
    }
}

/// Cover image URL; `thumbnail` may be 256 or 512 for a downscaled JPEG.
pub fn cover_url(manga_id: &str, file_name: &str, thumbnail: Option<u16>) -> String {
    match thumbnail {
        Some(size) => format!("{COVERS_BASE}/{manga_id}/{file_name}.{size}.jpg"),
        None => format!("{COVERS_BASE}/{manga_id}/{file_name}"),
    }
}

fn is_mangadex_org(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(|host| host.ends_with("mangadex.org")))
        .unwrap_or(false)
}

/// Waits until `X-RateLimit-Retry-After` (a UNIX timestamp) when present, else backs off exponentially.
fn retry_delay(headers: &HeaderMap, attempt: u32) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    headers
        .get("x-ratelimit-retry-after")
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .map(|retry_at| Duration::from_secs(retry_at.saturating_sub(now).clamp(1, 60)))
        .unwrap_or_else(|| Duration::from_secs(1 << attempt))
}

async fn error_detail(response: reqwest::Response) -> String {
    let status = response.status();
    let body: Option<serde_json::Value> = response.json().await.ok();
    body.as_ref()
        .and_then(|body| body["errors"][0]["detail"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("unknown error")
                .to_owned()
        })
}
