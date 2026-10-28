//! Checking library manga for chapters released on MangaDex after the newest one on disk.
//!
//! A manga folder is linked to MangaDex by its `.toshokan.json`. Folders downloaded before
//! that file existed are looked up once, through the chapter link in a CBZ's ComicInfo.xml or
//! else by their title, and the link is saved for next time.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use super::link::MangaLink;
use super::{ChapterKind, LibraryManga, LocalChapter, chapter_number};
use crate::api::{self, Chapter, ChapterSlot, Client};
use crate::download;

/// How many CBZ files to look in for a ComicInfo.xml chapter link before trying the title.
const COMIC_INFO_TRIES: usize = 3;

#[derive(Debug, Clone)]
pub enum Outcome {
    /// No manga on MangaDex could be matched to the folder.
    NotFound,
    Found(Found),
}

#[derive(Debug, Clone)]
pub struct Found {
    pub link: MangaLink,
    /// The groups behind the newest chapter on disk, when its name says so.
    pub groups: Option<Vec<String>>,
    /// Chapters numbered after the newest one on disk, in order.
    pub chapters: Vec<NewChapter>,
}

#[derive(Debug, Clone)]
pub struct NewChapter {
    pub slot: ChapterSlot,
    /// The upload by [`Found::groups`], if they released this chapter.
    upload: Option<usize>,
}

impl NewChapter {
    pub fn upload(&self) -> Option<&Chapter> {
        Some(&self.slot.uploads[self.upload?])
    }
}

impl Found {
    /// The new chapters that still aren't on disk.
    pub fn missing(&self, local: &[LocalChapter]) -> Vec<&NewChapter> {
        if self.chapters.is_empty() {
            return Vec::new();
        }
        let on_disk: Vec<f64> = local
            .iter()
            .filter_map(|c| chapter_number(&c.name))
            .collect();
        self.chapters
            .iter()
            .filter(|new| !new.slot.numeric().is_some_and(|n| on_disk.contains(&n)))
            .collect()
    }
}

/// What a library check reports as it goes.
#[derive(Debug)]
pub enum CheckEvent {
    /// The library was scanned: this many manga are about to be checked, and these folders
    /// are already linked (manga id, folder).
    Started {
        total: usize,
        links: Vec<(String, PathBuf)>,
    },
    Checked {
        key: String,
        dir: PathBuf,
        result: Result<Outcome, String>,
    },
    /// Gave up before the end, e.g. because MangaDex is blocking requests.
    Stopped(String),
}

/// Checks every manga folder in `root`, one at a time. `language` and `include_adult` are
/// used to find folders that aren't linked yet by their title.
pub async fn check_library(
    client: &Client,
    root: PathBuf,
    language: &str,
    include_adult: bool,
    report: impl Fn(CheckEvent),
) {
    let scanned = tokio::task::spawn_blocking(move || super::scan(&root)).await;
    let library: Vec<LibraryManga> = match scanned {
        Ok(Ok(library)) => library.into_iter().filter(|m| m.dir.is_some()).collect(),
        Ok(Err(err)) => return report(CheckEvent::Stopped(err.to_string())),
        Err(err) => return report(CheckEvent::Stopped(err.to_string())),
    };
    let links = library
        .iter()
        .filter_map(|m| Some((m.link.as_ref()?.id.clone(), m.dir.clone()?)))
        .collect();
    report(CheckEvent::Started {
        total: library.len(),
        links,
    });
    for manga in library {
        let result = check(client, &manga, language, include_adult).await;
        // Carrying on while blocked or rate limited would only make it worse.
        if let Err(err @ (api::Error::Forbidden | api::Error::RateLimited)) = &result {
            return report(CheckEvent::Stopped(err.to_string()));
        }
        report(CheckEvent::Checked {
            key: manga.key,
            dir: manga.dir.unwrap_or_default(),
            result: result.map_err(|err| err.to_string()),
        });
    }
}

async fn check(
    client: &Client,
    manga: &LibraryManga,
    language: &str,
    include_adult: bool,
) -> api::Result<Outcome> {
    let Some(dir) = &manga.dir else {
        return Ok(Outcome::NotFound);
    };
    let link = match &manga.link {
        Some(link) => link.clone(),
        None => match find_link(client, manga, language, include_adult).await? {
            Some(link) => {
                let (saved, dir) = (link.clone(), dir.clone());
                // Without the file the folder is just looked up again next time.
                let _ = tokio::task::spawn_blocking(move || saved.save(&dir)).await;
                link
            }
            None => return Ok(Outcome::NotFound),
        },
    };
    let slots = client.chapters(&link.id, &link.language).await?;
    Ok(Outcome::Found(compare(&manga.chapters, slots, link)))
}

/// Finds the MangaDex manga of a folder without a link: through a CBZ's chapter link, or
/// else the one search result whose title gives the folder's name.
async fn find_link(
    client: &Client,
    manga: &LibraryManga,
    language: &str,
    include_adult: bool,
) -> api::Result<Option<MangaLink>> {
    let chapters = manga.chapters.clone();
    let comic_info = tokio::task::spawn_blocking(move || comic_info_chapter(&chapters))
        .await
        .ok()
        .flatten();
    if let Some((chapter_id, chapter_language)) = comic_info {
        match client.manga_of_chapter(&chapter_id).await {
            Ok(Some(id)) => {
                return Ok(Some(MangaLink {
                    id,
                    language: chapter_language.unwrap_or_else(|| language.to_owned()),
                    newest_chapter: None,
                }));
            }
            // The chapter may have been taken down since; the title may still work.
            Ok(None) | Err(api::Error::Api { .. }) => {}
            Err(err) => return Err(err),
        }
    }
    let results = client.search_manga(&manga.title, 0, include_adult).await?;
    let mut matches = results
        .items
        .into_iter()
        .filter(|found| download::sanitize(&found.title) == manga.key);
    Ok(match (matches.next(), matches.next()) {
        (Some(found), None) => Some(MangaLink {
            id: found.id,
            language: language.to_owned(),
            newest_chapter: None,
        }),
        _ => None,
    })
}

/// Compares the chapters on MangaDex with the ones on disk, and the newest one the folder
/// held if chapters were deleted since.
pub fn compare(local: &[LocalChapter], slots: Vec<ChapterSlot>, link: MangaLink) -> Found {
    let mut numbered: Vec<(f64, &str)> = local
        .iter()
        .map(|c| c.name.as_str())
        .chain(link.newest_chapter.as_deref())
        .filter_map(|name| Some((chapter_number(name)?, name)))
        .collect();
    numbered.sort_by(|a, b| b.0.total_cmp(&a.0));
    // The newest chapter whose name tells which groups released it.
    let groups = numbered.iter().find_map(|(_, name)| {
        slots
            .iter()
            .flat_map(|slot| &slot.uploads)
            .find(|upload| download::released_by(name, &upload.groups))
            .map(|upload| upload.groups.clone())
    });
    let chapters = match numbered.first() {
        Some(&(latest, _)) => slots
            .into_iter()
            .filter(|slot| slot.numeric().is_some_and(|n| n > latest))
            .map(|slot| NewChapter {
                upload: groups
                    .as_ref()
                    .and_then(|groups| slot.uploads.iter().position(|u| &u.groups == groups)),
                slot,
            })
            .collect(),
        // Without chapter numbers there's no telling what's new.
        None => Vec::new(),
    };
    Found {
        link,
        groups,
        chapters,
    }
}

/// The MangaDex chapter id, and language, in the ComicInfo.xml of one of the newest CBZs.
fn comic_info_chapter(chapters: &[LocalChapter]) -> Option<(String, Option<String>)> {
    chapters
        .iter()
        .rev()
        .filter(|chapter| chapter.kind == ChapterKind::Cbz)
        .take(COMIC_INFO_TRIES)
        .find_map(|chapter| {
            let xml = read_comic_info(&chapter.path)?;
            Some((chapter_id_in(&xml)?, element(&xml, "LanguageISO")))
        })
}

fn read_comic_info(path: &Path) -> Option<String> {
    let mut archive = ZipArchive::new(BufReader::new(File::open(path).ok()?)).ok()?;
    let mut xml = String::new();
    archive
        .by_name("ComicInfo.xml")
        .ok()?
        .read_to_string(&mut xml)
        .ok()?;
    Some(xml)
}

/// The id in a `mangadex.org/chapter/<id>` link.
fn chapter_id_in(text: &str) -> Option<String> {
    const PREFIX: &str = "mangadex.org/chapter/";
    let start = text.find(PREFIX)? + PREFIX.len();
    let id: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    (id.len() == 36).then_some(id)
}

/// The text of the first `<name>` element.
fn element(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{name}>"))?;
    let text = xml[start..end].trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn local(name: &str) -> LocalChapter {
        LocalChapter {
            name: name.to_owned(),
            file_name: format!("{name}.cbz"),
            path: PathBuf::from(format!("/library/Berserk/{name}.cbz")),
            kind: ChapterKind::Cbz,
        }
    }

    fn upload(id: &str, number: &str, group: &str) -> Chapter {
        Chapter {
            id: id.to_owned(),
            volume: None,
            number: Some(number.to_owned()),
            title: None,
            language: "en".to_owned(),
            pages: 20,
            groups: vec![group.to_owned()],
            publish_at: String::new(),
        }
    }

    fn slot(number: &str, uploads: Vec<Chapter>) -> ChapterSlot {
        ChapterSlot {
            volume: None,
            number: Some(number.to_owned()),
            uploads,
        }
    }

    fn link() -> MangaLink {
        MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: None,
        }
    }

    #[test]
    fn finds_chapters_after_the_newest_one_by_the_same_group() {
        let on_disk = [local("Ch. 0001 [A]"), local("Ch. 0002 [B]")];
        let slots = vec![
            slot("1", vec![upload("1a", "1", "A")]),
            slot("2", vec![upload("2a", "2", "A"), upload("2b", "2", "B")]),
            slot("2.5", vec![upload("2.5a", "2.5", "A")]),
            slot("3", vec![upload("3a", "3", "A"), upload("3b", "3", "B")]),
        ];
        let found = compare(&on_disk, slots, link());
        assert_eq!(found.groups.as_deref(), Some(&["B".to_owned()][..]));
        let new: Vec<_> = found
            .chapters
            .iter()
            .map(|c| (c.slot.label(), c.upload().map(|u| u.id.as_str())))
            .collect();
        assert_eq!(
            new,
            [
                ("Ch. 2.5".to_owned(), None),
                ("Ch. 3".to_owned(), Some("3b"))
            ]
        );

        // Once chapter 3 is downloaded only 2.5 is left.
        let on_disk = [local("Ch. 0002 [B]"), local("Ch. 0003 [B]")];
        let missing: Vec<_> = found
            .missing(&on_disk)
            .iter()
            .map(|c| c.slot.label())
            .collect();
        assert_eq!(missing, ["Ch. 2.5"]);
    }

    #[test]
    fn counts_from_the_newest_chapter_even_once_deleted() {
        let slots = || {
            vec![
                slot("1", vec![upload("1a", "1", "A")]),
                slot("2", vec![upload("2a", "2", "A"), upload("2b", "2", "B")]),
                slot("3", vec![upload("3a", "3", "A"), upload("3b", "3", "B")]),
            ]
        };
        let link = MangaLink {
            newest_chapter: Some("Ch. 0002 [B]".to_owned()),
            ..link()
        };
        // Chapter 2 was read and deleted; chapter 1 is still there.
        let found = compare(&[local("Ch. 0001 [A]")], slots(), link.clone());
        assert_eq!(found.groups.as_deref(), Some(&["B".to_owned()][..]));
        let new: Vec<_> = found.chapters.iter().map(|c| c.slot.label()).collect();
        assert_eq!(new, ["Ch. 3"]);
        assert_eq!(found.chapters[0].upload().unwrap().id, "3b");

        // Every chapter deleted.
        let found = compare(&[], slots(), link.clone());
        assert_eq!(found.chapters.len(), 1);

        // Chapters downloaded since count once they're newer.
        let found = compare(&[local("Ch. 0003 [A]")], slots(), link);
        assert!(found.chapters.is_empty());
        assert_eq!(found.groups.as_deref(), Some(&["A".to_owned()][..]));
    }

    #[test]
    fn skips_names_without_a_known_group() {
        let on_disk = [local("Ch. 0001 [A]"), local("Chapter 2 (my scan)")];
        let slots = vec![
            slot("1", vec![upload("1a", "1", "A")]),
            slot("3", vec![upload("3a", "3", "A")]),
        ];
        let found = compare(&on_disk, slots, link());
        assert_eq!(found.groups.as_deref(), Some(&["A".to_owned()][..]));
        assert_eq!(found.chapters.len(), 1);
        assert_eq!(found.chapters[0].upload().unwrap().id, "3a");

        let found = compare(&[local("Oneshot")], vec![slot("1", vec![])], link());
        assert!(
            found.chapters.is_empty(),
            "nothing numbered to compare with"
        );
    }

    #[test]
    fn reads_the_chapter_link_from_comic_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Ch. 0001.cbz");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        zip.start_file("ComicInfo.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(
            b"<ComicInfo>\n  <Web>https://mangadex.org/chapter/0a1b2c3d-0000-4000-8000-123456789abc</Web>\n  <LanguageISO>es-la</LanguageISO>\n</ComicInfo>",
        )
        .unwrap();
        zip.finish().unwrap();

        let chapter = LocalChapter {
            path,
            ..local("Ch. 0001")
        };
        assert_eq!(
            comic_info_chapter(&[chapter]),
            Some((
                "0a1b2c3d-0000-4000-8000-123456789abc".to_owned(),
                Some("es-la".to_owned())
            ))
        );
        assert_eq!(chapter_id_in("https://mangadex.org/chapter/abc"), None);
    }
}
