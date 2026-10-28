//! MangaDex wire types and the simplified domain types the rest of the app uses.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};

pub type LocalizedString = HashMap<String, String>;

/// Some localized fields come back as `[]` instead of `{}` when empty.
fn localized<'de, D: Deserializer<'de>>(de: D) -> Result<LocalizedString, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum MapOrList {
        Map(LocalizedString),
        #[allow(dead_code)]
        List(Vec<serde::de::IgnoredAny>),
    }
    Ok(match MapOrList::deserialize(de)? {
        MapOrList::Map(map) => map,
        MapOrList::List(_) => LocalizedString::new(),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct Collection<T> {
    pub data: Vec<T>,
    pub offset: u32,
    pub total: u32,
}

/// The body of a request for one entity, like `GET /manga/{id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct EntityResponse<T> {
    pub data: T,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Entity<A> {
    pub id: String,
    pub attributes: A,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Relationship {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Only present for relationship types requested via `includes[]`.
    pub attributes: Option<serde_json::Value>,
}

impl Relationship {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attributes.as_ref()?.get(key)?.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContentRating {
    Safe,
    Suggestive,
    Erotica,
    Pornographic,
}

impl ContentRating {
    pub const ALL: [ContentRating; 4] = [
        ContentRating::Safe,
        ContentRating::Suggestive,
        ContentRating::Erotica,
        ContentRating::Pornographic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ContentRating::Safe => "safe",
            ContentRating::Suggestive => "suggestive",
            ContentRating::Erotica => "erotica",
            ContentRating::Pornographic => "pornographic",
        }
    }

    pub fn is_adult(self) -> bool {
        matches!(self, ContentRating::Erotica | ContentRating::Pornographic)
    }

    /// Ratings to request when searching, depending on the adult content setting.
    pub fn allowed(include_adult: bool) -> impl Iterator<Item = ContentRating> {
        Self::ALL
            .into_iter()
            .filter(move |rating| include_adult || !rating.is_adult())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaAttributes {
    #[serde(deserialize_with = "localized")]
    pub title: LocalizedString,
    #[serde(default)]
    pub alt_titles: Vec<LocalizedString>,
    #[serde(default, deserialize_with = "localized")]
    pub description: LocalizedString,
    pub status: Option<String>,
    pub year: Option<u32>,
    pub content_rating: ContentRating,
    pub original_language: Option<String>,
    #[serde(default)]
    pub available_translated_languages: Vec<Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manga {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub year: Option<u32>,
    pub content_rating: ContentRating,
    pub languages: Vec<String>,
    pub authors: Vec<String>,
    pub cover_file: Option<String>,
}

impl From<Entity<MangaAttributes>> for Manga {
    fn from(entity: Entity<MangaAttributes>) -> Self {
        let attrs = entity.attributes;
        let title = pick_title(&attrs).unwrap_or_else(|| entity.id.clone());
        let description = attrs
            .description
            .get("en")
            .or_else(|| attrs.description.values().next())
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());

        let mut authors = Vec::new();
        let mut cover_file = None;
        for rel in &entity.relationships {
            match rel.kind.as_str() {
                "author" | "artist" => {
                    if let Some(name) = rel.attr("name")
                        && !authors.iter().any(|known| known == name)
                    {
                        authors.push(name.to_owned());
                    }
                }
                "cover_art" => cover_file = rel.attr("fileName").map(str::to_owned),
                _ => {}
            }
        }

        Manga {
            id: entity.id,
            title,
            description,
            status: attrs.status,
            year: attrs.year,
            content_rating: attrs.content_rating,
            languages: attrs
                .available_translated_languages
                .into_iter()
                .flatten()
                .collect(),
            authors,
            cover_file,
        }
    }
}

/// English title first, then an English alt title, then the romanized original, then anything.
fn pick_title(attrs: &MangaAttributes) -> Option<String> {
    let romanized = attrs
        .original_language
        .as_ref()
        .map(|lang| format!("{lang}-ro"));
    attrs
        .title
        .get("en")
        .or_else(|| attrs.alt_titles.iter().find_map(|alt| alt.get("en")))
        .or_else(|| romanized.and_then(|key| attrs.title.get(&key)))
        .or_else(|| attrs.title.values().next())
        .cloned()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChapterAttributes {
    pub volume: Option<String>,
    pub chapter: Option<String>,
    pub title: Option<String>,
    pub translated_language: String,
    pub pages: u32,
    pub publish_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chapter {
    pub id: String,
    pub volume: Option<String>,
    pub number: Option<String>,
    pub title: Option<String>,
    pub language: String,
    pub pages: u32,
    /// Scanlation group names; empty when the uploader set no group.
    pub groups: Vec<String>,
    pub publish_at: String,
}

impl From<Entity<ChapterAttributes>> for Chapter {
    fn from(entity: Entity<ChapterAttributes>) -> Self {
        let attrs = entity.attributes;
        let groups = entity
            .relationships
            .iter()
            .filter(|rel| rel.kind == "scanlation_group")
            .filter_map(|rel| rel.attr("name").map(str::to_owned))
            .collect();
        Chapter {
            id: entity.id,
            volume: attrs.volume.filter(|v| !v.is_empty()),
            number: attrs.chapter.filter(|c| !c.is_empty()),
            title: attrs.title.filter(|t| !t.is_empty()),
            language: attrs.translated_language,
            pages: attrs.pages,
            groups,
            publish_at: attrs.publish_at,
        }
    }
}

impl Chapter {
    /// e.g. "Vol. 1 Ch. 10", "Ch. 386", or "Oneshot".
    pub fn label(&self) -> String {
        match (&self.volume, &self.number) {
            (Some(volume), Some(number)) => format!("Vol. {volume} Ch. {number}"),
            (None, Some(number)) => format!("Ch. {number}"),
            (Some(volume), None) => format!("Vol. {volume} Oneshot"),
            (None, None) => "Oneshot".to_owned(),
        }
    }
}

/// All uploads of one chapter number, so the user can pick which group's release to download.
#[derive(Debug, Clone)]
pub struct ChapterSlot {
    pub volume: Option<String>,
    pub number: Option<String>,
    pub uploads: Vec<Chapter>,
}

impl ChapterSlot {
    /// Like [`Chapter::label`], with the volume any upload gave.
    pub fn label(&self) -> String {
        match (&self.volume, &self.number) {
            (Some(volume), Some(number)) => format!("Vol. {volume} Ch. {number}"),
            (None, Some(number)) => format!("Ch. {number}"),
            (Some(volume), None) => format!("Vol. {volume} Oneshot"),
            (None, None) => "Oneshot".to_owned(),
        }
    }

    /// The chapter number as a number, when it is one.
    pub fn numeric(&self) -> Option<f64> {
        self.number.as_deref()?.parse().ok()
    }
}

/// Groups uploads by chapter number and sorts them numerically ("10" after "9.5").
///
/// Chapters without a number (usually oneshots) are only merged when their titles match.
pub fn group_chapters(chapters: Vec<Chapter>) -> Vec<ChapterSlot> {
    let mut slots: Vec<ChapterSlot> = Vec::new();
    for chapter in chapters {
        let existing = slots
            .iter_mut()
            .find(|slot| match (&slot.number, &chapter.number) {
                (Some(a), Some(b)) => a == b,
                (None, None) => slot.uploads[0].title == chapter.title,
                _ => false,
            });
        match existing {
            Some(slot) => {
                if slot.volume.is_none() {
                    slot.volume = chapter.volume.clone();
                }
                slot.uploads.push(chapter);
            }
            None => slots.push(ChapterSlot {
                volume: chapter.volume.clone(),
                number: chapter.number.clone(),
                uploads: vec![chapter],
            }),
        }
    }
    slots.sort_by(|a, b| {
        sort_key(a.number.as_deref())
            .total_cmp(&sort_key(b.number.as_deref()))
            .then_with(|| a.number.cmp(&b.number))
    });
    slots
}

fn sort_key(number: Option<&str>) -> f64 {
    number
        .and_then(|n| n.parse::<f64>().ok())
        .unwrap_or(f64::INFINITY)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Quality {
    Data,
    DataSaver,
}

impl Quality {
    /// The URL path segment MangaDex@Home uses for this quality.
    pub fn as_str(self) -> &'static str {
        match self {
            Quality::Data => "data",
            Quality::DataSaver => "data-saver",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AtHomeResponse {
    pub base_url: String,
    pub chapter: AtHomeChapter,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AtHomeChapter {
    pub hash: String,
    pub data: Vec<String>,
    pub data_saver: Vec<String>,
}

/// Where to download a chapter's pages from. The base URL expires after roughly 15 minutes.
#[derive(Debug, Clone)]
pub struct PageSource {
    pub base_url: String,
    pub hash: String,
    pub quality: Quality,
    pub files: Vec<String>,
}

impl PageSource {
    pub(crate) fn new(response: AtHomeResponse, quality: Quality) -> Self {
        let files = match quality {
            Quality::Data => response.chapter.data,
            Quality::DataSaver => response.chapter.data_saver,
        };
        PageSource {
            base_url: response.base_url,
            hash: response.chapter.hash,
            quality,
            files,
        }
    }

    pub fn page_urls(&self) -> impl Iterator<Item = String> + '_ {
        self.files.iter().map(|file| {
            format!(
                "{}/{}/{}/{}",
                self.base_url,
                self.quality.as_str(),
                self.hash,
                file
            )
        })
    }
}

#[derive(Debug, Clone)]
pub struct SearchPage {
    pub items: Vec<Manga>,
    pub offset: u32,
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chapter(id: &str, volume: Option<&str>, number: Option<&str>, group: &str) -> Chapter {
        Chapter {
            id: id.to_owned(),
            volume: volume.map(str::to_owned),
            number: number.map(str::to_owned),
            title: None,
            language: "en".to_owned(),
            pages: 10,
            groups: vec![group.to_owned()],
            publish_at: String::new(),
        }
    }

    #[test]
    fn groups_duplicate_uploads_and_sorts_numerically() {
        let slots = group_chapters(vec![
            chapter("a", Some("2"), Some("10"), "A"),
            chapter("b", None, Some("9.5"), "A"),
            chapter("c", None, Some("10"), "B"),
            chapter("d", Some("1"), Some("2"), "A"),
            chapter("e", None, None, "A"),
        ]);
        let numbers: Vec<_> = slots.iter().map(|s| s.number.as_deref()).collect();
        assert_eq!(numbers, [Some("2"), Some("9.5"), Some("10"), None]);
        let ten = &slots[2];
        assert_eq!(ten.uploads.len(), 2);
        assert_eq!(ten.volume.as_deref(), Some("2"));
    }

    #[test]
    fn parses_manga_with_title_fallback_and_includes() {
        let json = r#"{
            "id": "m1",
            "type": "manga",
            "attributes": {
                "title": {"ja-ro": "Berserk"},
                "altTitles": [{"ja": "ベルセルク"}, {"en": "Berserk EN"}],
                "description": [],
                "status": "ongoing",
                "year": 1989,
                "contentRating": "suggestive",
                "originalLanguage": "ja",
                "availableTranslatedLanguages": ["en", null, "fr"]
            },
            "relationships": [
                {"id": "a1", "type": "author", "attributes": {"name": "Miura Kentarou"}},
                {"id": "a1", "type": "artist", "attributes": {"name": "Miura Kentarou"}},
                {"id": "c1", "type": "cover_art", "attributes": {"fileName": "cover.jpg"}},
                {"id": "m2", "type": "manga"}
            ]
        }"#;
        let manga: Manga = serde_json::from_str::<Entity<MangaAttributes>>(json)
            .unwrap()
            .into();
        assert_eq!(manga.title, "Berserk EN");
        assert_eq!(manga.description, None);
        assert_eq!(manga.authors, ["Miura Kentarou"]);
        assert_eq!(manga.cover_file.as_deref(), Some("cover.jpg"));
        assert_eq!(manga.languages, ["en", "fr"]);
    }

    #[test]
    fn builds_page_urls_for_chosen_quality() {
        let response = AtHomeResponse {
            base_url: "https://x.mangadex.network".to_owned(),
            chapter: AtHomeChapter {
                hash: "h".to_owned(),
                data: vec!["1-full.png".to_owned()],
                data_saver: vec!["1-small.jpg".to_owned()],
            },
        };
        let source = PageSource::new(response, Quality::DataSaver);
        let urls: Vec<_> = source.page_urls().collect();
        assert_eq!(
            urls,
            ["https://x.mangadex.network/data-saver/h/1-small.jpg"]
        );
    }
}
