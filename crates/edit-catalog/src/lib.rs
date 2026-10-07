//! SQLite catalog: schema, typed stores, and a durable job queue.
//!
//! Open with [`Catalog::open`], which migrates the database and recovers
//! jobs left `running` by a crash. Background threads claim jobs through
//! [`spawn_workers`] and report progress on a [`flume`] channel.

pub mod db;
pub mod error;
pub mod jobs;
pub mod models;
pub mod stores;

pub use db::Catalog;
pub use error::{Error, Result};
pub use jobs::{
    ClaimedJob, Job, JobEvent, JobEventKind, JobHandler, JobId, JobKind, JobState, WorkerPool,
    spawn_workers,
};
