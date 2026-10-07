//! Browse surfaces for M1 (contact sheet, single view, scan jobs).
//!
//! [`browse`] holds the UI-agnostic state (scan, pairing, filter/sort,
//! selection, zoom) covered by `cargo test`; [`preview`] decodes thumbnails
//! through `lightcraft-raw` with a JPEG fallback; [`views`] renders them.

pub mod browse;
pub mod preview;
pub mod views;
