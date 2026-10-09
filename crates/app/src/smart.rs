//! Smart preview jobs: build the saved editing copy of a raw in the
//! background (see `edit_render::smart_preview`).
//!
//! Three things enqueue the same `smart_preview` job: the first edit of a
//! raw, the neighbours of the open photo, and a slow sweep over the opened
//! folder. The sweep keeps at most one job in flight and only enqueues while
//! no thumbnail or scan work is waiting, so it never delays them; opening
//! another folder bumps a generation that stops it and makes its queued job
//! a no-op.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use edit_catalog::{Catalog, JobId, JobKind, JobState};
use edit_core::cancel::CancelToken;
use edit_render::RenderService;
use serde::{Deserialize, Serialize};

/// The sweep stops adding photos once the store is this full, so it never
/// evicts the previews of photos the user already worked on.
const SWEEP_FILL_LIMIT: f64 = 0.8;

/// Pause between checks while the sweep waits for a quiet queue.
const SWEEP_POLL: Duration = Duration::from_millis(250);

/// Generation of the running sweep; a job or thread from an older one stops.
static SWEEP_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Deserialize)]
pub struct SmartPreviewPayload {
    pub path: String,
    /// Also keep the working copy in memory (the photo is open or next to it).
    #[serde(default)]
    pub bank: bool,
    /// The sweep generation that queued this job, if it came from the sweep.
    #[serde(default)]
    pub sweep: Option<u64>,
}

/// Queue a smart preview job unless the same one is already pending.
pub fn enqueue(catalog: &Catalog, path: &str, bank: bool) -> Option<JobId> {
    enqueue_payload(
        catalog,
        &SmartPreviewPayload {
            path: path.to_string(),
            bank,
            sweep: None,
        },
    )
}

fn enqueue_payload(catalog: &Catalog, payload: &SmartPreviewPayload) -> Option<JobId> {
    let json = serde_json::to_string(payload).ok()?;
    catalog
        .enqueue_job_if_absent(JobKind::SmartPreview, &json)
        .ok()
        .flatten()
}

/// Worker body of a `smart_preview` job. Idempotent: an existing preview is
/// left alone, and a repeat after a crash rebuilds nothing twice.
pub fn run_job(renderer: &RenderService, payload_json: &str) -> Result<(), String> {
    let payload: SmartPreviewPayload = serde_json::from_str(payload_json)
        .map_err(|e| format!("bad smart preview payload: {e}"))?;
    if payload
        .sweep
        .is_some_and(|g| g != SWEEP_GENERATION.load(Ordering::Relaxed))
    {
        return Ok(());
    }
    renderer
        .ensure_smart_preview(Path::new(&payload.path), payload.bank, &CancelToken::new())
        .map(|_| ())
        .map_err(|e| format!("smart preview failed for {}: {e}", payload.path))
}

/// Stop the running sweep (another folder opened).
pub fn cancel_sweep() {
    SWEEP_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Start a sweep over `paths` (raws of the opened folder, most useful first)
/// and remove smart previews whose photo rows are gone. Returns immediately.
pub fn start_sweep(catalog: Arc<Catalog>, renderer: Arc<RenderService>, paths: Vec<String>) {
    let generation = SWEEP_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    std::thread::spawn(move || sweep(&catalog, &renderer, paths, generation));
}

fn sweep(catalog: &Catalog, renderer: &RenderService, paths: Vec<String>, generation: u64) {
    let current = || SWEEP_GENERATION.load(Ordering::Relaxed) == generation;
    drop_orphans(catalog, renderer);
    let busy = [
        JobKind::CatalogScan,
        JobKind::Thumb,
        JobKind::SmartPreview,
        JobKind::IngestCopy,
    ];
    for path in paths {
        if renderer.editing_source(Path::new(&path)) != edit_render::EditingSource::Raw {
            continue;
        }
        loop {
            if !current() || !store_has_room(renderer) {
                return;
            }
            if catalog.count_queued_jobs(&busy).unwrap_or(1) == 0 {
                break;
            }
            std::thread::sleep(SWEEP_POLL);
        }
        let payload = SmartPreviewPayload {
            path,
            bank: false,
            sweep: Some(generation),
        };
        let Some(id) = enqueue_payload(catalog, &payload) else {
            continue;
        };
        // One in flight: wait for it to settle before the next.
        while current()
            && matches!(
                catalog.job(id).map(|j| j.state),
                Ok(JobState::Queued | JobState::Running)
            )
        {
            std::thread::sleep(SWEEP_POLL);
        }
    }
}

fn store_has_room(renderer: &RenderService) -> bool {
    renderer
        .smart_preview_usage()
        .is_some_and(|(used, budget)| (used as f64) < budget as f64 * SWEEP_FILL_LIMIT)
}

/// Delete smart previews of photos that no longer have a catalog row.
fn drop_orphans(catalog: &Catalog, renderer: &RenderService) {
    let Ok(roots) = catalog.list_roots() else {
        return;
    };
    let mut live: Vec<PathBuf> = Vec::new();
    for root in roots {
        match catalog.list_photos_by_root(root.id) {
            Ok(photos) => live.extend(photos.into_iter().map(|p| PathBuf::from(p.path))),
            // A failed read must not look like "no photos".
            Err(_) => return,
        }
    }
    renderer.retain_smart_previews(live.iter().map(PathBuf::as_path));
}
