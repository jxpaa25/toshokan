//! Hits the real MangaDex API. Run with `cargo test -- --ignored`.

use std::fs::{self, File};
use std::io::Write;
use std::sync::Mutex;

use toshokan::api::{Client, Demographic, Quality, SearchFilters, Sort, Status};
use toshokan::library::link::MangaLink;
use toshokan::library::updates::{CheckEvent, Outcome, check_library};
use toshokan::ui::cover::{self, CoverSource};

/// Berserk, which MangaDex rates "erotica".
const BERSERK: &str = "801513ba-a712-498c-8f57-cae55b38cc92";

#[tokio::test]
#[ignore = "requires network access to MangaDex"]
async fn search_respects_adult_setting() {
    let client = Client::new().unwrap();

    let none = SearchFilters::default();
    let safe = client
        .search_manga("berserk", &none, 0, false)
        .await
        .unwrap();
    assert!(safe.items.iter().all(|m| !m.content_rating.is_adult()));
    assert!(safe.items.iter().all(|m| m.id != BERSERK));

    let all = client
        .search_manga("berserk", &none, 0, true)
        .await
        .unwrap();
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
async fn browses_by_filters_and_tags() {
    let client = Client::new().unwrap();
    let tags = client.tags().await.unwrap();
    assert!(tags.len() > 50, "{} tags", tags.len());
    let tag = |name: &str| tags.iter().find(|t| t.name == name).unwrap().clone();

    let mut filters = SearchFilters {
        sort: Sort::MostFollowed,
        included_tags: vec![tag("Action")],
        excluded_tags: vec![tag("Romance")],
        ..SearchFilters::default()
    };
    filters.toggle_status(Status::Completed);
    filters.toggle_demographic(Demographic::Seinen);
    let page = client.search_manga("", &filters, 0, false).await.unwrap();
    assert!(page.total > 10, "{} results", page.total);
    assert!(
        page.items
            .iter()
            .all(|m| m.status.as_deref() == Some("completed"))
    );

    let year = SearchFilters {
        year: Some(1989),
        ..SearchFilters::default()
    };
    let page = client.search_manga("", &year, 0, false).await.unwrap();
    assert!(page.items.iter().all(|m| m.year == Some(1989)));
}

#[tokio::test]
#[ignore = "requires network access to MangaDex"]
async fn loads_covers_online_and_for_linked_folders() {
    let client = Client::new().unwrap();
    let berserk = client.manga(BERSERK).await.unwrap();
    let online = CoverSource::online(&berserk).expect("Berserk has a cover");
    let thumbnail = cover::load(&client, online).await.unwrap();
    assert_eq!(thumbnail.width(), 512);

    // A linked folder without a cover gets MangaDex's, saved next to its chapters.
    let library = tempfile::tempdir().unwrap();
    let dir = library.path().join("Berserk");
    fs::create_dir(&dir).unwrap();
    let source = CoverSource::Local {
        dir: Some(dir.clone()),
        manga_id: Some(BERSERK.to_owned()),
        first_chapter: None,
    };
    let image = cover::load(&client, source).await.unwrap();
    assert!(image.width() <= 512 && image.height() <= 768);
    let saved: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].starts_with("cover."), "{saved:?}");
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
