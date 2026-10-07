//! R0 retouch spike (scratch binaries only — no app crate depends on this).
//!
//! Modules:
//! - `lightcraft_bridge`: the raw decode → develop-source path, adapted from
//!   lightcraft's engine (`crates/engine/src/files.rs` + `camera_preview.rs`)
//!   at the pinned rev so in-process renders are pixel-identical to the CLI.
//! - `photocraft_bridge`: Rgba8 ↔ photocraft Surface conversion (the only
//!   place the two ArtCraft families meet).
//! - `yunet`: tract + YuNet ONNX runner with box decode (feature `tract`).
//! - `detect_common`: box math, NMS, IoU matching, WIDER-Face parsing, and
//!   the face-size bucket / recall reporting shared by the bake-off.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

pub mod detect_common;
pub mod lightcraft_bridge;
pub mod photocraft_bridge;
#[cfg(feature = "tract")]
pub mod yunet;
