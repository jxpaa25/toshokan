//! Hits the real MangaDex API. Run with `cargo test -- --ignored`.

use toshokan::api::{Client, Quality};

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
