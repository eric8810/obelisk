// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Design tokens, ported 1:1 from the original Electron renderer's
//! `app/src/renderer/styles/base.css` (the visual spec for the GPUI rewrite,
//! ADR-0013 "parity" includes the look, not just the interactions).
//!
//! CSS variables map to GPUI as follows: `rgba(255,255,255,x)` alpha tokens
//! become precomputed `rgba(0xffffff, x)` gpui colors; font stacks become the
//! `.SystemUIFont` sans / `.SystemMono` mono family names.

#![allow(dead_code)]

use gpui::{px, Pixels, Rgba};

/// Const-friendly Rgba constructor (gpui's `rgba()` is not const): takes
/// `0xRRGGBBAA`.
pub const fn rgba(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 24) & 0xff) as f32 / 255.0,
        g: ((hex >> 16) & 0xff) as f32 / 255.0,
        b: ((hex >> 8) & 0xff) as f32 / 255.0,
        a: (hex & 0xff) as f32 / 255.0,
    }
}

// ---- surfaces / background -------------------------------------------------

/// Root background (`--bg`).
pub const BG: Rgba = rgba(0x0a0b14ff);
/// Lower background lift (`--bg-2`).
pub const BG_2: Rgba = rgba(0x11131fff);
/// Sidebar wash (sidebar.css `.sidebar` background over the page).
pub const SIDEBAR_BG: Rgba = rgba(0x00000033);
/// Popover / floating surface (sidebar.css `.sources-popover`).
pub const POPOVER_BG: Rgba = rgba(0x141626fa);

/// `--surface`: rgba(255,255,255,0.03)
pub const SURFACE: Rgba = rgba(0xffffff08);
/// `--surface-strong`: rgba(255,255,255,0.06)
pub const SURFACE_STRONG: Rgba = rgba(0xffffff0f);
/// `--surface-hi`: rgba(255,255,255,0.09)
pub const SURFACE_HI: Rgba = rgba(0xffffff17);

// ---- text ------------------------------------------------------------------

/// `--fg`: rgba(255,255,255,0.92)
pub const FG: Rgba = rgba(0xfffffdeb);
/// `--fg-2`: rgba(255,255,255,0.72)
pub const FG_2: Rgba = rgba(0xffffffb8);
/// `--muted`: rgba(255,255,255,0.48)
pub const MUTED: Rgba = rgba(0xffffff7a);
/// `--muted-2`: rgba(255,255,255,0.28)
pub const MUTED_2: Rgba = rgba(0xffffff47);

// ---- lines -----------------------------------------------------------------

/// `--hairline`: rgba(255,255,255,0.05)
pub const HAIRLINE: Rgba = rgba(0xffffff0d);
/// `--hairline-strong`: rgba(255,255,255,0.08)
pub const HAIRLINE_STRONG: Rgba = rgba(0xffffff14);
/// `--edge-hi` / `--edge-lo` (card inner edges).
pub const EDGE_HI: Rgba = rgba(0xffffff14);
pub const EDGE_LO: Rgba = rgba(0x00000059);

// ---- accent ----------------------------------------------------------------

/// `--accent` (violet-400).
pub const ACCENT: Rgba = rgba(0xa78bfaff);
/// `--accent-2` (violet-300).
pub const ACCENT_2: Rgba = rgba(0xc4b5fdff);
/// `--accent-glow`.
pub const ACCENT_GLOW: Rgba = rgba(0xa78bfa59);
/// `--accent-soft`: rgba(167,139,250,0.12)
pub const ACCENT_SOFT: Rgba = rgba(0xa78bfa1f);

// ---- status ----------------------------------------------------------------

pub const DANGER: Rgba = rgba(0xf87171ff);
pub const DANGER_SOFT: Rgba = rgba(0xf871711f);
pub const WARN: Rgba = rgba(0xfbbf24ff);
pub const WARN_SOFT: Rgba = rgba(0xfbbf2424);
pub const WORKFLOW: Rgba = rgba(0xf59e0bff);
pub const WORKFLOW_SOFT: Rgba = rgba(0xf59e0b1f);
pub const WORKFLOW_STRONG: Rgba = rgba(0xf59e0b47);

// ---- message bubbles -------------------------------------------------------

/// `--user-bubble`: rgba(167,139,250,0.08)
pub const USER_BUBBLE: Rgba = rgba(0xa78bfa14);
/// `--user-bubble-border`: rgba(167,139,250,0.18)
pub const USER_BUBBLE_BORDER: Rgba = rgba(0xa78bfa2e);
/// `--asst-bubble`: rgba(255,255,255,0.025)
pub const ASST_BUBBLE: Rgba = rgba(0xffffff06);
/// `--asst-bubble-border`: rgba(255,255,255,0.06)
pub const ASST_BUBBLE_BORDER: Rgba = rgba(0xffffff0f);

// ---- typography ------------------------------------------------------------

/// `--text-xs`
pub const TEXT_XS: Pixels = px(11.0);
/// `--text-sm`
pub const TEXT_SM: Pixels = px(12.0);
/// `--text-base`
pub const TEXT_BASE: Pixels = px(13.0);
/// `--text-md`
pub const TEXT_MD: Pixels = px(14.0);

/// Sans family for prose and labels (CSS `--font-sans`).
pub const SANS: &str = ".SystemUIFont";
/// Mono family for technical text — paths, counts, timestamps (CSS
/// `--font-mono`).
pub const MONO: &str = ".SystemMono";

// ---- Activity heatmap (Vue .heatmap-cell level-0..4) ------------------------

/// level-0: `--surface-strong`
pub const HEAT_L0: Rgba = rgba(0xffffff0f);
/// level-1: rgba(99, 102, 241, 0.3)
pub const HEAT_L1: Rgba = rgba(0x6366f14d);
/// level-2: rgba(99, 102, 241, 0.5)
pub const HEAT_L2: Rgba = rgba(0x6366f180);
/// level-3: rgba(139, 92, 246, 0.7)
pub const HEAT_L3: Rgba = rgba(0x8b5cf6b3);
/// level-4: rgba(168, 85, 247, 0.9)
pub const HEAT_L4: Rgba = rgba(0xa855f7e6);

pub const HEAT_LEVELS: [Rgba; 5] = [HEAT_L0, HEAT_L1, HEAT_L2, HEAT_L3, HEAT_L4];

// ---- Recap archetype palettes (Vue src/archetypes.js PALETTES) --------------

pub struct ArchetypePalette {
    pub key: &'static str,
    pub name: &'static str,
    /// `tc` — primary text/border color.
    pub tc: Rgba,
    /// `tc2` — light variant (secondary text).
    pub tc2: Rgba,
    /// `glow` — strong wash (headers, focus rings).
    pub glow: Rgba,
    /// `mid` — medium wash (card backgrounds).
    pub mid: Rgba,
    /// `soft` — faint wash (hover, dividers).
    pub soft: Rgba,
}

/// Vue rgba(...,0.40) glow / 0.22 mid / 0.10 soft alphas.
pub const ARCHETYPES: [ArchetypePalette; 7] = [
    ArchetypePalette {
        key: "architect",
        name: "The Architect",
        tc: rgba(0xa78bfaff),
        tc2: rgba(0xc4b5fdff),
        glow: rgba(0xa78bfa66),
        mid: rgba(0xa78bfa38),
        soft: rgba(0xa78bfa1a),
    },
    ArchetypePalette {
        key: "debugger",
        name: "The Debugger",
        tc: rgba(0xfbbf24ff),
        tc2: rgba(0xfde68aff),
        glow: rgba(0xfbbf2466),
        mid: rgba(0xfbbf2438),
        soft: rgba(0xfbbf241a),
    },
    ArchetypePalette {
        key: "shipper",
        name: "The Shipper",
        tc: rgba(0xf472b6ff),
        tc2: rgba(0xfda4afff),
        glow: rgba(0xf472b666),
        mid: rgba(0xf472b638),
        soft: rgba(0xf472b61a),
    },
    ArchetypePalette {
        key: "curator",
        name: "The Curator",
        tc: rgba(0x67e8f9ff),
        tc2: rgba(0xa5f3fcff),
        glow: rgba(0x67e8f966),
        mid: rgba(0x67e8f938),
        soft: rgba(0x67e8f91a),
    },
    ArchetypePalette {
        key: "director",
        name: "The Director",
        tc: rgba(0xfcd34dff),
        tc2: rgba(0xfde68aff),
        glow: rgba(0xfcd34d66),
        mid: rgba(0xfcd34d38),
        soft: rgba(0xfcd34d1a),
    },
    ArchetypePalette {
        key: "cartographer",
        name: "The Cartographer",
        tc: rgba(0x34d399ff),
        tc2: rgba(0x6ee7b7ff),
        glow: rgba(0x34d39966),
        mid: rgba(0x34d39938),
        soft: rgba(0x34d3991a),
    },
    ArchetypePalette {
        key: "wanderer",
        name: "The Wanderer",
        tc: rgba(0x64748bff),
        tc2: rgba(0x94a3b8ff),
        glow: rgba(0x64748b73),
        mid: rgba(0x64748b40),
        soft: rgba(0x64748b1f),
    },
];

/// Look up a palette by archetype key (case-insensitive; falls back to
/// wanderer, matching the Vue default).
pub fn archetype(key: &str) -> &'static ArchetypePalette {
    let key = key.trim().to_ascii_lowercase();
    ARCHETYPES
        .iter()
        .find(|p| p.key == key)
        .unwrap_or(&ARCHETYPES[6])
}

// ---- metrics ---------------------------------------------------------------

/// Sidebar column width (`--col-sidebar`).
pub const SIDEBAR_W: Pixels = px(220.0);
/// Compact sidebar row height (`--row-h-compact`).
pub const ROW_H_COMPACT: Pixels = px(28.0);
/// Sub-row (nested) sidebar row height.
pub const ROW_H_SUB: Pixels = px(26.0);
/// Sidebar row corner radius (`.sidebar-item` border-radius).
pub const ROW_RADIUS: Pixels = px(5.0);
