//! On-disk layout: file names, ComicInfo.xml, and packing page images into a .cbz.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::api::{Chapter, Manga};

/// Leaves room under the common 255-byte file name limit for suffixes like ".data-saver.part".
const MAX_NAME_BYTES: usize = 200;

/// Makes `name` safe to use as one path component on Linux, macOS and Windows.
pub fn sanitize(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    for c in name.chars() {
        let c = match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => ' ',
            c => c,
        };
        if cleaned.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        cleaned.push(c);
    }
    // Leading dots hide files on Unix; trailing dots and spaces are invalid on Windows.
    let trimmed = cleaned
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "_".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// e.g. "Vol. 01 Ch. 0010.5 - The Brand [Band of the Hawks]". The zero padding keeps
/// chapters in order in file managers and readers that sort by name.
pub fn chapter_stem(chapter: &Chapter) -> String {
    let mut stem = String::new();
    if let Some(volume) = &chapter.volume {
        stem.push_str(&format!("Vol. {} ", pad_number(volume, 2)));
    }
    match &chapter.number {
        Some(number) => stem.push_str(&format!("Ch. {}", pad_number(number, 4))),
        None => stem.push_str("Oneshot"),
    }
    if let Some(title) = &chapter.title {
        stem.push_str(" - ");
        stem.push_str(title);
    }
    if !chapter.groups.is_empty() {
        stem.push_str(&format!(" [{}]", chapter.groups.join(" & ")));
    }
    sanitize(&stem)
}

fn pad_number(number: &str, width: usize) -> String {
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return number.to_owned();
    }
    match fraction {
        Some(fraction) => format!("{whole:0>width$}.{fraction}"),
        None => format!("{whole:0>width$}"),
    }
}

/// "001.jpg", "002.png", …, with at least three digits so pages sort correctly.
pub fn page_name(index: usize, total: usize, source_file: &str) -> String {
    let width = total.to_string().len().max(3);
    let extension = Path::new(source_file)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("jpg");
    format!("{:0width$}.{extension}", index + 1)
}

/// Metadata most comic readers use for titles and chapter ordering. Elements follow the
/// order the ComicInfo schema requires.
pub fn comic_info(manga: &Manga, chapter: &Chapter, pages: usize) -> String {
    let mut fields: Vec<(&str, String)> = Vec::new();
    if let Some(title) = &chapter.title {
        fields.push(("Title", title.clone()));
    }
    fields.push(("Series", manga.title.clone()));
    if let Some(number) = &chapter.number {
        fields.push(("Number", number.clone()));
    }
    // The schema only allows whole-number volumes.
    if let Some(volume) = chapter.volume.as_ref().filter(|v| v.parse::<u32>().is_ok()) {
        fields.push(("Volume", volume.clone()));
    }
    if !manga.authors.is_empty() {
        fields.push(("Writer", manga.authors.join(", ")));
    }
    if !chapter.groups.is_empty() {
        fields.push(("Translator", chapter.groups.join(", ")));
    }
    fields.push((
        "Web",
        format!("https://mangadex.org/chapter/{}", chapter.id),
    ));
    fields.push(("PageCount", pages.to_string()));
    fields.push(("LanguageISO", chapter.language.clone()));

    let mut xml = String::from(concat!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n",
        "<ComicInfo xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" ",
        "xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\">\n",
    ));
    for (name, value) in fields {
        xml.push_str(&format!("  <{name}>{}</{name}>\n", escape_xml(&value)));
    }
    xml.push_str("</ComicInfo>\n");
    xml
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Packs `pages` from `page_dir`, in order, plus ComicInfo.xml into a .cbz at `dest`.
///
/// Entries are stored uncompressed: the images are already compressed, and stored
/// entries are the quickest for readers to open.
pub fn write_cbz(
    page_dir: &Path,
    pages: &[String],
    comic_info: &str,
    dest: &Path,
) -> io::Result<()> {
    let mut zip = ZipWriter::new(BufWriter::new(File::create(dest)?));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for page in pages {
        zip.start_file(page.as_str(), options)?;
        io::copy(&mut File::open(page_dir.join(page))?, &mut zip)?;
    }
    zip.start_file("ComicInfo.xml", options)?;
    zip.write_all(comic_info.as_bytes())?;
    zip.finish()?.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ContentRating;

    fn chapter(volume: Option<&str>, number: Option<&str>, title: Option<&str>) -> Chapter {
        Chapter {
            id: "c1".to_owned(),
            volume: volume.map(str::to_owned),
            number: number.map(str::to_owned),
            title: title.map(str::to_owned),
            language: "en".to_owned(),
            pages: 2,
            groups: vec!["Band of the Hawks".to_owned()],
            publish_at: String::new(),
        }
    }

    #[test]
    fn sanitizes_path_components() {
        assert_eq!(sanitize(".hack//Sign"), "hack__Sign");
        assert_eq!(sanitize("What? Really. "), "What_ Really");
        assert_eq!(sanitize("..."), "_");
        let long = "ベ".repeat(100);
        assert!(sanitize(&long).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn names_chapters_so_they_sort() {
        let name = chapter_stem(&chapter(Some("1"), Some("0.5"), Some("The Brand")));
        assert_eq!(name, "Vol. 01 Ch. 0000.5 - The Brand [Band of the Hawks]");
        assert_eq!(
            chapter_stem(&chapter(None, Some("386"), None)),
            "Ch. 0386 [Band of the Hawks]"
        );
        assert_eq!(
            chapter_stem(&chapter(None, Some("Extra"), None)),
            "Ch. Extra [Band of the Hawks]"
        );
        assert_eq!(
            chapter_stem(&chapter(None, None, Some("A/B"))),
            "Oneshot - A_B [Band of the Hawks]"
        );
    }

    #[test]
    fn numbers_pages_with_their_extension() {
        assert_eq!(page_name(0, 25, "x1-abc.png"), "001.png");
        assert_eq!(page_name(1199, 1200, "x1200-abc.jpg"), "1200.jpg");
    }

    #[test]
    fn writes_escaped_comic_info() {
        let manga = Manga {
            id: "m1".to_owned(),
            title: "Tom & Jerry".to_owned(),
            description: None,
            status: None,
            year: None,
            content_rating: ContentRating::Safe,
            languages: vec![],
            authors: vec!["A".to_owned(), "B".to_owned()],
            cover_file: None,
        };
        let xml = comic_info(
            &manga,
            &chapter(Some("1.5"), Some("3"), Some("<Start>")),
            20,
        );
        assert!(xml.contains("<Title>&lt;Start&gt;</Title>"));
        assert!(xml.contains("<Series>Tom &amp; Jerry</Series>"));
        assert!(xml.contains("<Number>3</Number>"));
        assert!(!xml.contains("<Volume>"), "fractional volumes are left out");
        assert!(xml.contains("<Writer>A, B</Writer>"));
        assert!(xml.contains("<PageCount>20</PageCount>"));
    }
}
