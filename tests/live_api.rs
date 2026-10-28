//! Hits the real MangaDex API. Run with `cargo test -- --ignored`.

use std::fs::{self, File};
use std::io::Write;
use std::sync::Mutex;

use toshokan::api::{Client, Quality};
use toshokan::library::link::MangaLink;
use toshokan::library::updates::{CheckEvent, Outcome, check_library};

/// Berserk, which MangaDex rates "erotica".
const BERSERK: &str = "801513ba-a712-498c-8f57-cae55b38cc92";

#[tokio::test]
#[ignore = "requires network access to MangaDex"]
async fn search_respects_adult_setting() {
    let client = Client::new().unwrap();

    let safe = client.search_manga("berserk", 0, false).await.unwrap();
    assert!(safe.items.iter().all(|m| !m.content_rating.is_adult()));
    assert!(safe.items.iter().all(|m| m.id != BERSERK));

    let all = client.search_manga("berserk", 0, true).await.unwrap();
    let berserk = all
        .items
        .iter()
        .find(|m| m.id == BERSERK)
        .expect("Berserk in adult-enabled results");
    assert_eq!(berserk.title, "Berserk");
    assert!(berserk.cover_file.is_some());
    assert!(!berserk.authors.is_empty());
}

#[tokio::test]
#[ignore = "requires network access to MangaDex"]
async fn list_chapters_and_download_a_page() {
    let client = Client::new().unwrap();

    let slots = client.chapters(BERSERK, "en").await.unwrap();
    assert!(slots.len() > 300);
    assert!(slots.iter().any(|slot| slot.uploads.len() > 1));

    let chapter = &slots[0].uploads[0];
    let source = client
        .page_source(&chapter.id, Quality::DataSaver)
        .await
        .unwrap();
    assert_eq!(source.files.len(), chapter.pages as usize);

    let url = source.page_urls().next().unwrap();
    let image = client.fetch_page(&url).await.unwrap();
    let is_jpeg = image.starts_with(&[0xFF, 0xD8]);
    let is_png = image.starts_with(b"\x89PNG");
    assert!(is_jpeg || is_png, "page is not an image");
}

#[tokio::test]
#[ignore = "requires network access to MangaDex"]
async fn links_an_old_download_and_finds_new_chapters() {
    let client = Client::new().unwrap();
    let manga = client.manga(BERSERK).await.unwrap();
    assert_eq!(manga.title, "Berserk");
    assert!(!manga.authors.is_empty());

    // A chapter downloaded before folders were linked: only ComicInfo.xml knows its manga.
    let slots = client.chapters(BERSERK, "en").await.unwrap();
    let chapter = &slots[0].uploads[0];
    assert_eq!(
        client
            .manga_of_chapter(&chapter.id)
            .await
            .unwrap()
            .as_deref(),
        Some(BERSERK)
    );
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("Berserk");
    fs::create_dir_all(&dir).unwrap();
    let mut zip = zip::ZipWriter::new(File::create(dir.join("Ch. 0001.cbz")).unwrap());
    zip.start_file("ComicInfo.xml", zip::write::SimpleFileOptions::default())
        .unwrap();
    let web = format!("<Web>https://mangadex.org/chapter/{}</Web>", chapter.id);
    zip.write_all(format!("<ComicInfo>{web}<LanguageISO>en</LanguageISO></ComicInfo>").as_bytes())
        .unwrap();
    zip.finish().unwrap();

    let events = Mutex::new(Vec::new());
    check_library(&client, root.path().to_owned(), "fr", false, |event| {
        events.lock().unwrap().push(event)
    })
    .await;
    let events = events.into_inner().unwrap();
    assert!(matches!(events[0], CheckEvent::Started { total: 1, .. }));
    let CheckEvent::Checked {
        result: Ok(Outcome::Found(found)),
        ..
    } = &events[1]
    else {
        panic!("expected Berserk to be found, got {:?}", events[1]);
    };
    assert_eq!(found.link.id, BERSERK);
    assert_eq!(
        found.link.language, "en",
        "the chapter's language, not the default"
    );
    assert!(found.chapters.len() > 300);
    assert_eq!(MangaLink::load(&dir), Some(found.link.clone()));
}
