//! Downloaded manga on disk: what's in the download folder, the pages of a chapter, and
//! reading progress.
//!
//! Each folder in the download folder is a manga whose `.cbz` files and image folders are its
//! chapters. A `.cbz` or image folder placed directly in the download folder is listed as a
//! one-chapter entry of its own.

pub mod delete;
pub mod link;
pub mod pages;
pub mod prepare;
pub mod relocate;
pub mod state;
pub mod updates;

use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use link::MangaLink;

const IMAGE_EXTENSIONS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "gif"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChapterKind {
    Cbz,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalChapter {
    /// Shown in lists: the file name without `.cbz`.
    pub name: String,
    /// The file or folder name, used as the key for reading progress.
    pub file_name: String,
    pub path: PathBuf,
    pub kind: ChapterKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryManga {
    /// The folder name (or the loose file's name), used as the key for reading state.
    pub key: String,
    pub title: String,
    pub chapters: Vec<LocalChapter>,
    /// The manga's folder; `None` for a loose chapter.
    pub dir: Option<PathBuf>,
    pub link: Option<MangaLink>,
}

impl LibraryManga {
    /// Whether its chapters were deleted to make room while it's still checked for new ones,
    /// which keeps it in the library with none left.
    pub fn is_followed(&self) -> bool {
        self.link
            .as_ref()
            .is_some_and(|link| link.newest_chapter.is_some())
    }
}

/// Everything readable in `root`, sorted by title. A missing folder is an empty library.
pub fn scan(root: &Path) -> io::Result<Vec<LibraryManga>> {
    let entries = match sorted_entries(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut library = Vec::new();
    for path in entries {
        // A folder holding chapters is a manga, even if it also has loose images like a cover.
        if path.is_dir() {
            let manga = scan_manga(&path)?;
            if !manga.chapters.is_empty() || manga.is_followed() {
                library.push(manga);
                continue;
            }
        }
        if let Some(chapter) = as_chapter(&path) {
            library.push(LibraryManga {
                key: chapter.file_name.clone(),
                title: chapter.name.clone(),
                chapters: vec![chapter],
                dir: None,
                link: None,
            });
        }
    }
    Ok(library)
}

/// The chapters in one manga folder.
pub fn scan_manga(dir: &Path) -> io::Result<LibraryManga> {
    let mut chapters: Vec<LocalChapter> = sorted_entries(dir)?
        .iter()
        .filter_map(|path| as_chapter(path))
        .collect();
    // Names alone put "Vol. 01 Ch. 0001" after "Ch. 0002", so chapters with a number in
    // their name are ordered by it, ahead of any without one.
    chapters.sort_by(
        |a, b| match (chapter_number(&a.name), chapter_number(&b.name)) {
            (Some(x), Some(y)) => x.total_cmp(&y).then_with(|| natural_cmp(&a.name, &b.name)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => natural_cmp(&a.name, &b.name),
        },
    );
    let key = file_name(dir);
    Ok(LibraryManga {
        title: key.clone(),
        key,
        chapters,
        dir: Some(dir.to_owned()),
        link: MangaLink::load(dir),
    })
}

/// The number after "Ch", "Ch." or "Chapter" in any case, e.g. 10.5 in
/// "Vol. 02 Ch. 0010.5 - Title".
pub(crate) fn chapter_number(name: &str) -> Option<f64> {
    let lower = name.to_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("ch") {
        let start = from + found;
        from = start + 2;
        // "ch" has to start a word, so "Rich 2" doesn't count.
        if lower[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let rest = &lower[start + 2..];
        let rest = rest.strip_prefix("apter").unwrap_or(rest);
        let rest = rest.strip_prefix('.').unwrap_or(rest).trim_start();
        let number: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if let Ok(number) = number.trim_end_matches('.').parse() {
            return Some(number);
        }
    }
    None
}

/// A `.cbz` file or a folder with images directly inside, unless it's hidden or a
/// download in progress.
pub(crate) fn as_chapter(path: &Path) -> Option<LocalChapter> {
    let file_name = file_name(path);
    if file_name.starts_with('.') || file_name.ends_with(".part") {
        return None;
    }
    let (name, kind) = if path.is_file() && has_extension(path, &["cbz"]) {
        (
            file_name.trim_end_matches(".cbz").to_owned(),
            ChapterKind::Cbz,
        )
    } else if path.is_dir() && contains_images(path) {
        (file_name.clone(), ChapterKind::Folder)
    } else {
        return None;
    };
    Some(LocalChapter {
        name,
        file_name,
        path: path.to_owned(),
        kind,
    })
}

/// Whether `dir` has page images directly inside. A series cover doesn't count.
fn contains_images(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let path = entry.path();
                let is_cover = path.file_stem().is_some_and(|stem| stem == "cover");
                is_image(&path) && !is_cover && path.is_file()
            })
        })
        .unwrap_or(false)
}

pub(crate) fn is_image(path: &Path) -> bool {
    has_extension(path, &IMAGE_EXTENSIONS)
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sorted_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    paths.sort_by(|a, b| natural_cmp(&file_name(a), &file_name(b)));
    Ok(paths)
}

/// Orders names the way people expect: "Ch 2" before "Ch 10", ignoring case.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let a_len = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
                let b_len = b.find(|c: char| !c.is_ascii_digit()).unwrap_or(b.len());
                let (a_num, b_num) = (
                    a[..a_len].trim_start_matches('0'),
                    b[..b_len].trim_start_matches('0'),
                );
                let order = a_num.len().cmp(&b_num.len()).then_with(|| a_num.cmp(b_num));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[a_len..];
                b = &b[b_len..];
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[x.len_utf8()..];
                b = &b[y.len_utf8()..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn orders_numbers_naturally() {
        let mut names = vec!["Ch 10", "ch 2", "Ch 1.5", "Ch 0001", "Extra"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["Ch 0001", "Ch 1.5", "ch 2", "Ch 10", "Extra"]);
    }

    #[test]
    fn scans_manga_folders_and_loose_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Berserk/Ch. 0010 [A].cbz"));
        touch(&root.join("Berserk/Ch. 0002 [A].cbz"));
        touch(&root.join("Berserk/Vol. 01 Ch. 0001/001.jpg"));
        touch(&root.join("Berserk/Ch. 0011 [A].data.part/001.jpg"));
        touch(&root.join("Berserk/Ch. 0012 [A].cbz.part"));
        touch(&root.join("Berserk/cover.jpg"));
        touch(&root.join("Berserk/.hidden.cbz"));
        touch(&root.join("Akira 01.cbz"));
        touch(&root.join("Oneshot scans/page1.png"));
        touch(&root.join("Empty manga/notes.txt"));
        // Linked by a first download that was cancelled: nothing to follow yet.
        let linked = MangaLink {
            id: "m1".to_owned(),
            language: "en".to_owned(),
            newest_chapter: None,
        };
        linked.save(&root.join("Empty manga")).unwrap();
        let followed = MangaLink {
            id: "m2".to_owned(),
            language: "en".to_owned(),
            newest_chapter: Some("Ch. 0040".to_owned()),
        };
        fs::create_dir(root.join("Read and deleted")).unwrap();
        followed.save(&root.join("Read and deleted")).unwrap();

        let library = scan(root).unwrap();
        let titles: Vec<_> = library.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Akira 01", "Berserk", "Oneshot scans", "Read and deleted"]
        );
        assert!(library[3].chapters.is_empty());

        let berserk = &library[1];
        let chapters: Vec<_> = berserk
            .chapters
            .iter()
            .map(|c| (c.name.as_str(), c.kind))
            .collect();
        assert_eq!(
            chapters,
            [
                ("Vol. 01 Ch. 0001", ChapterKind::Folder),
                ("Ch. 0002 [A]", ChapterKind::Cbz),
                ("Ch. 0010 [A]", ChapterKind::Cbz),
            ]
        );
        assert_eq!(library[2].chapters[0].kind, ChapterKind::Folder);
        assert_eq!(library[0].key, "Akira 01.cbz");
    }

    #[test]
    fn finds_chapter_numbers() {
        assert_eq!(chapter_number("Vol. 02 Ch. 0010.5 - Title [G]"), Some(10.5));
        assert_eq!(chapter_number("Chapter 7"), Some(7.0));
        assert_eq!(chapter_number("berserk ch12."), Some(12.0));
        assert_eq!(chapter_number("Chainsaw Man Ch 3"), Some(3.0));
        assert_eq!(chapter_number("Rich 2"), None);
        assert_eq!(chapter_number("Oneshot"), None);
    }

    #[test]
    fn missing_folder_is_an_empty_library() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(&dir.path().join("nope")).unwrap().is_empty());
    }
}
