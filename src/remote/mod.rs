//! `br remote` — mirroring a beads workspace into an external tracker.
//!
//! The CLI surface is backend-neutral; there is exactly one backend
//! (`youtrack`). See the `bds-4r2` epic's `design` field for the full spec.

pub mod config;
pub mod error;
pub mod http;
pub mod youtrack;
