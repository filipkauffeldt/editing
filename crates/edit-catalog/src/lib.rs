//! SQLite catalog: schema, typed stores, and a durable job queue.
//!
//! Open with [`Catalog::open`] (applies pending migrations), call
//! [`Catalog::recover_running`] once at startup to re-queue jobs left
//! `running` by a crash, then drain the queue with [`spawn_workers`].
//! Background threads report progress on a [`flume`] channel.

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
