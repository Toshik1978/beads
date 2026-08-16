//! `br remote` — mirroring a beads workspace into an external tracker.
//!
//! The CLI surface is backend-neutral; there is exactly one backend
//! (`youtrack`). See the `bds-4r2` epic's `design` field for the full spec.

pub mod adopt;
pub mod comments;
pub mod config;
pub mod diff;
pub mod error;
pub mod execute;
pub mod http;
pub mod link_diff;
pub mod model;
pub mod plan;
pub mod reconcile;
pub mod tombstone;
pub mod youtrack;
