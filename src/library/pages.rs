//! Listing and decoding the page images of a downloaded chapter.

use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::Path;

use image::{DynamicImage, ImageReader};
use zip::ZipArchive;

use super::{ChapterKind, LocalChapter, is_image, natural_cmp};

/// The chapter's page names in reading order: zip entry names for a `.cbz`, file names for
/// a folder.
pub fn list(chapter: &LocalChapter) -> Result<Vec<String>, String> {
    let mut pages: Vec<String> = match chapter.kind {
        ChapterKind::Cbz => {
            let archive = open_archive(&chapter.path)?;
            archive
                .file_names()
                // Entries with names that aren't valid UTF-8 can't be looked up by name later.
                .filter_map(|name| name.ok().map(|name| name.into_owned()))
                // Archives made on macOS carry resource-fork copies of every image.
                .filter(|name| !name.ends_with('/') && !name.starts_with("__MACOSX/"))
                .filter(|name| is_image(Path::new(name)))
                .collect()
        }
        ChapterKind::Folder => fs::read_dir(&chapter.path)
            .map_err(|err| err.to_string())?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && is_image(path))
            .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .collect(),
    };
    pages.sort_by(|a, b| natural_cmp(a, b));
    if pages.is_empty() {
        return Err("no page images in this chapter".to_owned());
    }
    Ok(pages)
}

/// Decodes one page named by [`list`].
pub fn load(chapter: &LocalChapter, page: &str) -> Result<DynamicImage, String> {
    let image = match chapter.kind {
        ChapterKind::Cbz => {
            let mut archive = open_archive(&chapter.path)?;
            let mut entry = archive.by_name(page).map_err(|err| err.to_string())?;
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut bytes)
                .map_err(|err| err.to_string())?;
            image::load_from_memory(&bytes)
        }
        ChapterKind::Folder => ImageReader::open(chapter.path.join(page))
            .and_then(|reader| reader.with_guessed_format())
            .map_err(|err| err.to_string())?
            .decode(),
    };
    image.map_err(|err| format!("couldn't decode {page}: {err}"))
}

/// How much of a page to read for its size; headers are almost always at the start.
const HEADER_BYTES: u64 = 64 * 1024;

/// The pixel sizes of `pages` of `chapter`, read from their headers without decoding them.
/// `None` for a page that can't be read.
pub fn measure(chapter: &LocalChapter, pages: &[String]) -> Vec<Option<(u32, u32)>> {
    match chapter.kind {
        ChapterKind::Cbz => {
            let Ok(mut archive) = open_archive(&chapter.path) else {
                return vec![None; pages.len()];
            };
            pages
                .iter()
                .map(|page| {
                    let mut entry = archive.by_name(page).ok()?;
                    let mut bytes = Vec::new();
                    (&mut entry)
                        .take(HEADER_BYTES)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    if let Some(size) = size_from(&bytes) {
                        return Some(size);
                    }
                    // The header is further in, e.g. after a big colour profile.
                    entry.read_to_end(&mut bytes).ok()?;
                    size_from(&bytes)
                })
                .collect()
        }
        ChapterKind::Folder => pages
            .iter()
            .map(|page| {
                ImageReader::open(chapter.path.join(page))
                    .and_then(|reader| reader.with_guessed_format())
                    .ok()?
                    .into_dimensions()
                    .ok()
            })
            .collect(),
    }
}

fn size_from(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn open_archive(path: &Path) -> Result<ZipArchive<BufReader<File>>, String> {
    let file = File::open(path).map_err(|err| err.to_string())?;
    ZipArchive::new(BufReader::new(file)).map_err(|err| format!("not a readable CBZ: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        DynamicImage::new_rgb8(width, height)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn chapter(path: &Path, kind: ChapterKind) -> LocalChapter {
        LocalChapter {
            name: "test".to_owned(),
            file_name: "test".to_owned(),
            path: path.to_owned(),
            kind,
        }
    }

    #[test]
    fn reads_compressed_cbz_pages_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ch.cbz");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let deflated =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for name in [
            "10.png",
            "2.png",
            "__MACOSX/._2.png",
            "ComicInfo.xml",
            "1.png",
        ] {
            zip.start_file(name, deflated).unwrap();
            zip.write_all(&png(4, 6)).unwrap();
        }
        zip.finish().unwrap();

        let chapter = chapter(&path, ChapterKind::Cbz);
        assert_eq!(list(&chapter).unwrap(), ["1.png", "2.png", "10.png"]);
        let names = ["1.png".to_owned(), "nope".to_owned()];
        assert_eq!(measure(&chapter, &names), [Some((4, 6)), None]);
        let page = load(&chapter, "10.png").unwrap();
        assert_eq!((page.width(), page.height()), (4, 6));
    }

    #[test]
    fn reads_folder_pages_and_reports_empty_chapters() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("002.png"), png(3, 5)).unwrap();
        fs::write(dir.path().join("001.png"), png(3, 5)).unwrap();
        fs::write(dir.path().join("ComicInfo.xml"), b"<ComicInfo/>").unwrap();

        let chapter = chapter(dir.path(), ChapterKind::Folder);
        assert_eq!(list(&chapter).unwrap(), ["001.png", "002.png"]);
        assert_eq!(measure(&chapter, &["002.png".to_owned()]), [Some((3, 5))]);
        assert_eq!(load(&chapter, "002.png").unwrap().height(), 5);

        let empty = tempfile::tempdir().unwrap();
        assert!(
            list(&super::super::LocalChapter {
                path: empty.path().to_owned(),
                ..chapter
            })
            .is_err()
        );
    }
}
