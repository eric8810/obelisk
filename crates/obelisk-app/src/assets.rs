// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Embedded icon assets.
//!
//! The icons are the original renderer's inline SVGs (App.vue / sidebar.css),
//! checked in under `assets/icons/` and compiled into the binary, so the app
//! never depends on the process cwd or an installed asset tree. GPUI resolves
//! `svg().path(...)` through [`gpui::AssetSource`], which this module
//! implements over a static path→content table.

use std::borrow::Cow;

/// One static asset: path key and file content.
const ICONS: &[(&str, &str)] = &[
    ("icons/brand.svg", include_str!("../assets/icons/brand.svg")),
    (
        "icons/sessions.svg",
        include_str!("../assets/icons/sessions.svg"),
    ),
    (
        "icons/memory.svg",
        include_str!("../assets/icons/memory.svg"),
    ),
    (
        "icons/dot-filled.svg",
        include_str!("../assets/icons/dot-filled.svg"),
    ),
    (
        "icons/dot-outline.svg",
        include_str!("../assets/icons/dot-outline.svg"),
    ),
    (
        "icons/activity.svg",
        include_str!("../assets/icons/activity.svg"),
    ),
    ("icons/recap.svg", include_str!("../assets/icons/recap.svg")),
    (
        "icons/settings.svg",
        include_str!("../assets/icons/settings.svg"),
    ),
    (
        "icons/search.svg",
        include_str!("../assets/icons/search.svg"),
    ),
    (
        "icons/folder.svg",
        include_str!("../assets/icons/folder.svg"),
    ),
    (
        "icons/chevron.svg",
        include_str!("../assets/icons/chevron.svg"),
    ),
];

/// Static asset source serving the embedded icons.
pub struct IconAssets;

impl gpui::AssetSource for IconAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(key, _)| *key == path)
            .map(|(_, content)| Cow::Borrowed(content.as_bytes())))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(key, _)| key.starts_with(path))
            .map(|(key, _)| gpui::SharedString::from(key.to_string()))
            .collect())
    }
}
