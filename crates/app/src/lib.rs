//! Browse surfaces for M1 (contact sheet, single view, scan jobs) and the
//! ingest copy worker for M4/C2.
//!
//! [`browse`] holds the UI-agnostic state (scan, pairing, filter/sort,
//! selection, zoom) covered by `cargo test`; [`preview`] decodes thumbnails
//! through `lightcraft-raw` with a JPEG fallback; [`crop`] holds the crop
//! mode drag and aspect logic; [`views`] renders them;
//! [`ingest_job`] runs the durable ingest copy job.

pub mod browse;
pub mod crop;
pub mod develop;
pub mod ingest;
pub mod ingest_job;
pub mod preview;
pub mod templates;
pub mod views;
