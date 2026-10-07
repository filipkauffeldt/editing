//! Browse demo: open a folder, list the pair items it contains.
//!
//! ```sh
//! cargo run -p app --example scan_demo -- /path/to/photos
//! ```
//!
//! Opens (creating) a scratch catalog next to the folder, runs the same
//! [`edit`] scan the `catalog_scan` worker runs, and prints one line per
//! contact-sheet cell.

use std::path::PathBuf;
use std::sync::Arc;

use edit_catalog::Catalog;

fn main() {
    let folder = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let db_path = folder.join(".editing-scan-demo.sqlite");
    let catalog = Catalog::open(&db_path).expect("open scratch catalog");
    catalog.recover_running().expect("recover queue");
    let catalog = Arc::new(catalog);

    let summary = app::browse::run_catalog_scan(&catalog, &folder, &|done, total| {
        eprintln!("scan: {done}/{total}");
    })
    .expect("scan folder");
    let items = app::browse::list_browse_items(&catalog, summary.root_id).expect("list items");
    println!(
        "folder: {} ({} files seen, {} new photos, {} cells)",
        folder.display(),
        summary.files_seen,
        summary.photos_added,
        items.len()
    );
    for item in &items {
        println!("  {:>6}  {}", item.item_id, item.label());
    }
    std::fs::remove_file(&db_path).ok();
    for ext in ["-wal", "-shm"] {
        std::fs::remove_file(db_path.with_extension(format!("sqlite{ext}"))).ok();
    }
}
