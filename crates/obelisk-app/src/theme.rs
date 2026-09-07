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

// ---- metrics ---------------------------------------------------------------

/// Sidebar column width (`--col-sidebar`).
pub const SIDEBAR_W: Pixels = px(220.0);
/// Compact sidebar row height (`--row-h-compact`).
pub const ROW_H_COMPACT: Pixels = px(28.0);
/// Sub-row (nested) sidebar row height.
pub const ROW_H_SUB: Pixels = px(26.0);
/// Sidebar row corner radius (`.sidebar-item` border-radius).
pub const ROW_RADIUS: Pixels = px(5.0);
