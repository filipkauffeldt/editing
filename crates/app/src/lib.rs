//! Browse helpers for M1 (folder scan, thumbnails).
//!
//! [`browse`] holds the UI-agnostic state (scan, pairing, filter/sort,
//! selection, zoom) covered by `cargo test`; [`preview`] decodes thumbnails
//! through `lightcraft-raw` with a JPEG fallback.

pub mod browse;
pub mod preview;
