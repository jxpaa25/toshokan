//! Deleting chapters and whole manga from the library.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::link::{self, MangaLink};
use super::{LibraryManga, LocalChapter, as_chapter, chapter_number, file_name, is_image};

/// What to delete, as picked on the library screen.
#[derive(Debug, Clone)]
pub enum Deletion {
    /// Its chapters, cover, link and unfinished downloads, and then the folder unless
    /// something else is in it.
    Manga(LibraryManga),
    Chapters {
        manga: LibraryManga,
        chapters: Vec<LocalChapter>,
    },
}

impl Deletion {
    pub fn manga(&self) -> &LibraryManga {
        match self {
            Deletion::Manga(manga) | Deletion::Chapters { manga, .. } => manga,
        }
    }

    /// The chapters that go.
    pub fn chapters(&self) -> &[LocalChapter] {
        match self {
            Deletion::Manga(manga) => &manga.chapters,
            Deletion::Chapters { chapters, .. } => chapters,
        }
    }

    /// Whether every chapter goes but the manga stays in the library, to keep getting new
    /// chapters.
    pub fn keeps_following(&self) -> bool {
        match self {
            Deletion::Manga(_) => false,
            Deletion::Chapters { manga, chapters } => {
                chapters.len() == manga.chapters.len()
                    && manga.dir.is_some()
                    && manga.link.is_some()
            }
        }
    }

    /// The files and folders it deletes.
    fn paths(&self) -> io::Result<Vec<PathBuf>> {
        match self {
            Deletion::Manga(LibraryManga { dir: Some(dir), .. }) => ours(dir),
            _ => Ok(self.chapters().iter().map(|c| c.path.clone()).collect()),
        }
    }

    /// How much space it frees.
    pub fn size(&self) -> u64 {
        self.paths()
            .unwrap_or_default()
            .iter()
            .map(|path| size_of(path))
            .sum()
    }
}

/// Deletes what `deletion` names, refusing anything outside `root`, the download folder.
/// Before chapters of a linked manga go, its link records the newest chapter it held.
pub fn delete(root: &Path, deletion: &Deletion) -> io::Result<()> {
    let paths = deletion.paths()?;
    if let Some(outside) = paths
        .iter()
        .find(|path| !path.starts_with(root) || *path == root)
    {
        let message = format!("{} isn't in the download folder", outside.display());
        return Err(io::Error::other(message));
    }
    if let Deletion::Chapters {
        manga:
            LibraryManga {
                dir: Some(dir),
                chapters,
                ..
            },
        ..
    } = deletion
    {
        remember_newest(dir, chapters)?;
    }
    for path in paths {
        remove(&path)?;
    }
    if let Deletion::Manga(LibraryManga { dir: Some(dir), .. }) = deletion
        && dir.starts_with(root)
        && dir != root
    {
        // Fails, keeping the folder, when the user keeps something else in it.
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// What the library and downloader put in a manga folder: chapters, the cover, the link and
/// unfinished downloads.
fn ours(dir: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = file_name(path);
            let is_cover = path.file_stem().is_some_and(|stem| stem == "cover") && is_image(path);
            as_chapter(path).is_some()
                || is_cover
                || name == link::FILE_NAME
                || name.ends_with(".part")
        })
        .collect())
}

/// Saves the name of the newest of `chapters` in the folder's link, unless it already has
/// a newer one, so chapters deleted now are never offered as new again.
fn remember_newest(dir: &Path, chapters: &[LocalChapter]) -> io::Result<()> {
    let Some(link) = MangaLink::load(dir) else {
        return Ok(());
    };
    let newest = chapters
        .iter()
        .map(|chapter| chapter.name.as_str())
        .chain(link.newest_chapter.as_deref())
        .filter_map(|name| Some((chapter_number(name)?, name)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    match newest {
        Some((_, name)) if link.newest_chapter.as_deref() != Some(name) => MangaLink {
            newest_chapter: Some(name.to_owned()),
            ..link
        }
        .save(dir),
        _ => Ok(()),
    }
}

fn remove(path: &Path) -> io::Result<()> {
    // Not following links, so a linked folder's contents are left alone.
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// A file's size, or everything in a folder.
fn size_of(path: &Path) -> u64 {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    fs::read_dir(path)
        .map(|entries| entries.flatten().map(|entry| size_of(&entry.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::scan;

    fn write(path: &Path, bytes: usize) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![0; bytes]).unwrap();
    }

    fn link(newest_chapter: Option<&str>) -> MangaLink {
        MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: newest_chapter.map(str::to_owned),
        }
    }

    /// Berserk with three chapters (100 bytes each, one a folder), a cover and a link.
    fn berserk(root: &Path) -> LibraryManga {
        let dir = root.join("Berserk");
        write(&dir.join("Ch. 0001 [A].cbz"), 100);
        write(&dir.join("Ch. 0002 [A]/001.png"), 100);
        write(&dir.join("Ch. 0003 [B].cbz"), 100);
        write(&dir.join("cover.jpg"), 10);
        link(None).save(&dir).unwrap();
        scan(root).unwrap().remove(0)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn deletes_chapters_and_remembers_the_newest() {
        let root = tempfile::tempdir().unwrap();
        let manga = berserk(root.path());
        let dir = manga.dir.clone().unwrap();
        let newest_two = Deletion::Chapters {
            chapters: manga.chapters[1..].to_vec(),
            manga: manga.clone(),
        };
        assert_eq!(newest_two.size(), 200);
        assert!(!newest_two.keeps_following());

        delete(root.path(), &newest_two).unwrap();
        assert_eq!(
            names(&dir),
            [".toshokan.json", "Ch. 0001 [A].cbz", "cover.jpg"]
        );
        let saved = MangaLink::load(&dir).unwrap();
        assert_eq!(saved.newest_chapter.as_deref(), Some("Ch. 0003 [B]"));

        // The last one: the manga stays, and the newest chapter isn't forgotten.
        let manga = scan(root.path()).unwrap().remove(0);
        let last = Deletion::Chapters {
            chapters: manga.chapters.clone(),
            manga,
        };
        assert!(last.keeps_following());
        delete(root.path(), &last).unwrap();
        let saved = MangaLink::load(&dir).unwrap();
        assert_eq!(saved.newest_chapter.as_deref(), Some("Ch. 0003 [B]"));
        let library = scan(root.path()).unwrap();
        assert_eq!(library.len(), 1);
        assert!(library[0].chapters.is_empty() && library[0].is_followed());
    }

    #[test]
    fn deletes_a_manga_but_not_what_else_is_in_its_folder() {
        let root = tempfile::tempdir().unwrap();
        let manga = berserk(root.path());
        let dir = manga.dir.clone().unwrap();
        write(&dir.join("Ch. 0004 [B].data.part/001.png"), 50);
        write(&dir.join("Ch. 0004 [B].cbz.part"), 50);
        let deletion = Deletion::Manga(manga.clone());
        let link_size = fs::metadata(dir.join(link::FILE_NAME)).unwrap().len();
        assert_eq!(deletion.size(), 300 + 10 + 100 + link_size);

        write(&dir.join("notes.txt"), 5);
        delete(root.path(), &deletion).unwrap();
        assert_eq!(names(&dir), ["notes.txt"], "the user's own file stays");
        assert!(scan(root.path()).unwrap().is_empty());

        // With nothing else in it, the folder goes too.
        let manga = berserk(root.path());
        fs::remove_file(dir.join("notes.txt")).unwrap();
        delete(root.path(), &Deletion::Manga(manga)).unwrap();
        assert!(!dir.exists());
        assert!(root.path().exists());
    }

    #[test]
    fn deletes_loose_chapters_and_nothing_outside_the_download_folder() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("Akira 01.cbz"), 100);
        let akira = scan(root.path()).unwrap().remove(0);
        delete(root.path(), &Deletion::Manga(akira)).unwrap();
        assert!(names(root.path()).is_empty());

        let elsewhere = tempfile::tempdir().unwrap();
        let manga = berserk(elsewhere.path());
        let err = delete(root.path(), &Deletion::Manga(manga)).unwrap_err();
        assert!(err.to_string().contains("isn't in the download folder"));
        assert_eq!(names(&elsewhere.path().join("Berserk")).len(), 5);
    }
}
