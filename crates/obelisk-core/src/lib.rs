// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Obelisk Core — the single shared Rust implementation behind every
//! transport (CLI today, the GPUI desktop app in Stage 2).
//!
//! Ported from the TypeScript core (packages/core, see
//! docs/adr/0013-tech-selection-rust-cli-sandbox-and-desktop.md). The 78-file
//! TS test suite is the behavioral spec; providers, persist, schema,
//! writer lease, watcher, and the CodeAct sandbox all live here.

pub mod core;
pub mod db;
pub mod index_finalize;
pub mod indexer;
#[cfg(test)]
mod invoking_tests;
pub mod parsing;
pub mod persist;
pub mod provider_indexing;
pub mod provider_settings;
pub mod providers;
pub mod query;
#[cfg(test)]
mod query_tests;
pub mod sandbox;
#[cfg(test)]
mod sandbox_tests;
pub mod schema;
pub mod session_detail;
#[cfg(test)]
mod session_detail_tests;
pub mod tx;
pub mod watcher;
pub mod writer_lease;

pub const OBELISK_DIR_NAME: &str = ".obelisk";
pub const DB_FILE_NAME: &str = "obelisk.sqlite";
