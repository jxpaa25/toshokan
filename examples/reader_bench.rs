//! Times getting pages ready for the kitty protocol (decode, shrink, write for the terminal),
//! as single pages and as a long strip: `cargo run --release --example reader_bench -- <chapter.cbz>`

use std::time::Instant;

use toshokan::library::prepare::{Bounds, fill_cells, prepare};
use toshokan::library::{ChapterKind, LocalChapter, pages};
use toshokan::ui::reader::layout::{page_rows, strip_width};

fn main() {
    let path = std::env::args().nth(1).expect("pass a .cbz path");
    let chapter = LocalChapter {
        name: String::new(),
        file_name: String::new(),
        path: path.into(),
        kind: ChapterKind::Cbz,
    };
    let names = pages::list(&chapter).unwrap();
    // A 190x50-cell window with 10x20 and 20x40 pixel cells, fit page.
    for bounds in [
        Bounds {
            width: 1900,
            height: Some(1000),
        },
        Bounds {
            width: 3800,
            height: Some(2000),
        },
    ] {
        let start = Instant::now();
        let mut bytes = 0;
        for name in &names {
            let page = prepare(&chapter, name, bounds).unwrap();
            let file = format!("/dev/shm/tty-graphics-protocol-toshokan-bench-{name}");
            std::fs::write(&file, &page.rgb).unwrap();
            std::fs::remove_file(&file).unwrap();
            bytes += page.rgb.len();
        }
        let per_page = start.elapsed().as_secs_f64() * 1000.0 / names.len() as f64;
        println!(
            "box {}x{}: {per_page:.1} ms per page ({} pages, {:.1} MB avg written)",
            bounds.width,
            bounds.height.unwrap(),
            names.len(),
            bytes as f64 / names.len() as f64 / 1e6
        );
    }

    // A long strip: every page's size from its header, then each page scaled to fill its cells
    // at the pages' own width, with 10x20 and 20x40 pixel cells.
    let start = Instant::now();
    let sizes = pages::measure(&chapter, &names);
    println!(
        "strip: measuring {} pages took {:.1} ms",
        names.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    let width = strip_width(&sizes);
    for cell in [(10u16, 20u16), (20, 40)] {
        let cols = (width / u32::from(cell.0)) as u16;
        let start = Instant::now();
        let mut bytes = 0;
        for (name, size) in names.iter().zip(&sizes) {
            let rows = page_rows(size.unwrap_or((1, 1)), cols, cell);
            let image = pages::load(&chapter, name).unwrap();
            let target = (
                u32::from(cols) * u32::from(cell.0),
                rows * u32::from(cell.1),
            );
            let page = fill_cells(image, target);
            let file = format!("/dev/shm/tty-graphics-protocol-toshokan-bench-{name}");
            std::fs::write(&file, &page.rgb).unwrap();
            std::fs::remove_file(&file).unwrap();
            bytes += page.rgb.len();
        }
        let per_page = start.elapsed().as_secs_f64() * 1000.0 / names.len() as f64;
        println!(
            "strip with {}x{} cells, {cols} columns: {per_page:.1} ms per page ({:.1} MB avg written)",
            cell.0,
            cell.1,
            bytes as f64 / names.len() as f64 / 1e6
        );
    }
}
