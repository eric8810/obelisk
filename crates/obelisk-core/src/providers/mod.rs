// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Provider adapters and the shared provider contract.

pub mod types;

pub mod claude;
pub mod codex;
pub mod deepseek;
pub mod kimi;
pub mod pi;

#[cfg(test)]
mod claude_tests;
#[cfg(test)]
mod codex_tests;
#[cfg(test)]
mod deepseek_tests;
#[cfg(test)]
mod kimi_tests;
#[cfg(test)]
mod pi_tests;

pub use types::*;
