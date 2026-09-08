// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Read-side data loading for the desktop app. The app links obelisk-core
//! directly (no IPC) and reads the shared index; write ownership stays with
//! the resident Rust daemon (ADR-0013 Stage 3).

use chrono::Datelike;
use std::path::Path;

use obelisk_core::db;

#[derive(Debug, Clone)]
pub struct ProjectSummary {
    pub slug: String,
    pub session_count: usize,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub project: String,
    pub source: String,
    pub started_at: String,
    pub ended_at: String,
    pub message_count: i64,
    /// Git branch at session start (Vue branch filter, M4.5).
    pub git_branch: String,
}

#[derive(Debug, Clone, Default)]
pub struct AppData {
    pub projects: Vec<ProjectSummary>,
    pub sessions: Vec<SessionSummary>,
    /// Memory registry counts for the sidebar badges.
    pub memory_active: usize,
    pub memory_archived: usize,
    /// Whether an index exists and has completed at least one build —
    /// drives the "building vs. genuinely empty" empty-state message.
    pub index_ready: bool,
}

impl AppData {
    /// Load projects + all session summaries from the shared index. Fails
    /// soft to empty data (fresh machine, index not built yet).
    pub fn load(home: &Path, _cwd: &Path) -> Self {
        let conn = match db::open_read_db(home) {
            Ok(conn) => conn,
            Err(_) => return Self::default(),
        };
        let projects = read_projects(&conn);
        let sessions = read_sessions(&conn);
        let (memory_active, memory_archived) = memory_counts(&conn);
        let index_ready = conn
            .query_row(
                "SELECT COUNT(*) FROM index_state WHERE jsonl_path = '__last_build__'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count > 0)
            .unwrap_or(false);
        Self {
            projects,
            sessions,
            memory_active,
            memory_archived,
            index_ready,
        }
    }

    /// Sessions for one project, or all sessions when None.
    pub fn sessions_for(&self, project: Option<&str>) -> Vec<SessionSummary> {
        match project {
            None => self.sessions.clone(),
            Some(project) => self
                .sessions
                .iter()
                .filter(|s| s.project == project)
                .cloned()
                .collect(),
        }
    }
}

/// Active/archived memory counts for the sidebar badges (one indexed read;
/// the memory list itself stays lazily loaded per view switch).
fn memory_counts(conn: &rusqlite::Connection) -> (usize, usize) {
    let count = |archived: bool| {
        conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE archived = ?",
            [archived],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    };
    (count(false), count(true))
}

// ---- Memories (Vue db:getMemories / db:readMemoryFile parity) ----

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: String,
    pub session_id: String,
    pub project: String,
    pub path: String,
    pub summary: String,
    pub created_at: String,
    pub deleted_at: Option<String>,
    #[allow(dead_code)] // shown in a future detail affordance
    pub deleted_reason: Option<String>,
    /// First/last message uuids the memory was distilled from (detail view).
    pub message_start: Option<String>,
    pub message_end: Option<String>,
    /// Anchor `path:line` buttons (detail view; may be empty).
    pub anchors: Vec<MemoryAnchor>,
}

/// One memory anchor: a file/line provenance button.
#[derive(Debug, Clone)]
pub struct MemoryAnchor {
    pub path: String,
    pub line: Option<i64>,
    pub exists: bool,
}

impl MemoryEntry {
    pub fn archived(&self) -> bool {
        self.deleted_at.is_some()
    }
}

pub fn load_memories(home: &Path) -> Vec<MemoryEntry> {
    let Ok(conn) = db::open_read_db(home) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, session_id, project, path, COALESCE(summary, ''),
                COALESCE(created_at, ''), deleted_at, deleted_reason,
                message_start, message_end, COALESCE(anchors, '[]')
         FROM memories ORDER BY created_at DESC",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        let anchors_raw: Option<String> = row.get(10)?;
        let anchors = parse_memory_anchors(anchors_raw.as_deref().unwrap_or("[]"));
        Ok(MemoryEntry {
            id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            project: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            path: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            summary: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            created_at: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            deleted_at: row.get(6)?,
            deleted_reason: row.get(7)?,
            message_start: row.get(8)?,
            message_end: row.get(9)?,
            anchors,
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

/// Parse the memories table's anchors JSON (`[{path, line?, exists?}]`,
/// written by the indexer's remember() flow). Malformed input yields an
/// empty list rather than failing the whole view.
fn parse_memory_anchors(raw: &str) -> Vec<MemoryAnchor> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some(MemoryAnchor {
                            path: item.get("path")?.as_str()?.to_string(),
                            line: item.get("line").and_then(|v| v.as_i64()),
                            exists: item.get("exists").and_then(|v| v.as_bool()).unwrap_or(true),
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Archive a memory (Vue parity: deleted_at = now, deleted_reason default).
/// The write goes through the writer lease — the app daemon owns index
/// writes (ADR-0013 Stage 3), and memories are no exception.
pub fn archive_memory(home: &Path, id: &str) -> Result<(), String> {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    mutate_memory(home, |conn| {
        conn.execute(
            "UPDATE memories SET deleted_at = ?2, deleted_reason = 'Archived via panel' WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id, now],
        )
    })
    .map(|_| ())
}

/// Restore an archived memory (deleted_at/deleted_reason cleared).
pub fn restore_memory(home: &Path, id: &str) -> Result<(), String> {
    mutate_memory(home, |conn| {
        conn.execute(
            "UPDATE memories SET deleted_at = NULL, deleted_reason = NULL WHERE id = ?1 AND deleted_at IS NOT NULL",
            rusqlite::params![id],
        )
    })
    .map(|_| ())
}

/// One memory mutation under the writer lease.
fn mutate_memory(
    home: &Path,
    work: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<usize>,
) -> Result<usize, String> {
    let db_path = obelisk_core::db::db_path(home);
    if !db_path.exists() {
        return Err("index does not exist yet".to_string());
    }
    let lease = obelisk_core::writer_lease::acquire_writer_lease(
        &obelisk_core::writer_lease::writer_lock_path_for(&db_path),
        obelisk_core::writer_lease::AcquireOptions {
            wait_ms: 1000,
            retry_delay_ms: 25,
        },
    )
    .ok_or_else(|| "another writer holds the index (writer_busy)".to_string())?;
    let result = db::open_db(home)
        .map_err(|e| e.to_string())
        .and_then(|conn| work(&conn).map_err(|e| e.to_string()));
    lease.release();
    result
}

/// Read one memory's markdown file. The path comes from the indexer-written
/// memories table (not renderer input), so it is read as-is like the TS side.
pub fn read_memory_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Sessions indexed under one provider root (Settings status cards,
/// parity #2): counted by jsonl_path prefix.
pub fn count_sessions_under(home: &Path, root: &Path) -> Option<i64> {
    let conn = db::open_read_db(home).ok()?;
    let prefix = root.to_string_lossy();
    let pattern = format!("{prefix}%");
    conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE jsonl_path LIKE ?1",
        [pattern],
        |row| row.get(0),
    )
    .ok()
}

// ---- Usage stats (Vue db:getUsageStats parity) ----

#[derive(Debug, Clone, Default)]
pub struct UsageDay {
    pub day: String,
    pub tokens: i64,
}

#[derive(Debug, Clone, Default)]
pub struct LongestTurn {
    pub turn_duration_ms: i64,
    /// Message identity, kept for the jump-to-message link (polish pass).
    #[allow(dead_code)]
    pub uuid: String,
    /// Kept for the future jump-to-message link.
    #[allow(dead_code)]
    pub session_id: String,
    /// Kept for the future jump-to-message link.
    #[allow(dead_code)]
    pub timestamp: String,
}

#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub daily: Vec<UsageDay>,
    pub total_tokens: i64,
    pub peak_day: Option<UsageDay>,
    pub longest_turn: Option<LongestTurn>,
}

pub fn load_usage_stats(home: &Path) -> UsageStats {
    let Ok(conn) = db::open_read_db(home) else {
        return UsageStats::default();
    };
    // Visibility controls evidence display, not accounting: abandoned model
    // calls still consumed tokens, so usage aggregates include them.
    let mut stats = UsageStats::default();
    let Ok(mut stmt) = conn.prepare(
        "WITH usage_events AS (
             SELECT timestamp, input_tokens, output_tokens FROM messages
             WHERE input_tokens IS NOT NULL OR output_tokens IS NOT NULL
             UNION ALL
             SELECT su.timestamp, su.input_tokens, su.output_tokens FROM summaries su
             WHERE su.input_tokens IS NOT NULL OR su.output_tokens IS NOT NULL
         )
         SELECT DATE(timestamp) AS day,
                SUM(COALESCE(input_tokens, 0) + COALESCE(output_tokens, 0)) AS tokens
         FROM usage_events GROUP BY DATE(timestamp) ORDER BY day",
    ) else {
        return stats;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok(UsageDay {
            day: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            tokens: row.get::<_, Option<i64>>(1)?.unwrap_or_default(),
        })
    }) else {
        return stats;
    };
    for day in rows.flatten() {
        stats.total_tokens += day.tokens;
        if stats
            .peak_day
            .as_ref()
            .is_none_or(|peak| day.tokens > peak.tokens)
        {
            stats.peak_day = Some(day.clone());
        }
        stats.daily.push(day);
    }
    if let Ok(mut stmt) = conn.prepare(
        "SELECT turn_duration_ms, uuid, session_id, COALESCE(timestamp, '')
         FROM messages WHERE turn_duration_ms IS NOT NULL
         ORDER BY turn_duration_ms DESC LIMIT 1",
    ) {
        if let Ok(turn) = stmt.query_row([], |row| {
            Ok(LongestTurn {
                turn_duration_ms: row.get(0)?,
                uuid: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                session_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                timestamp: row.get(3)?,
            })
        }) {
            stats.longest_turn = Some(turn);
        }
    }
    stats
}

// ---- Activity ledger + heatmap (Vue Activity.vue parity) --------------------

/// One bare session row for the activity ledger (query output).
#[derive(Debug, Clone, Default)]
pub struct ActivitySession {
    pub id: String,
    pub title: String,
    pub project: String,
    /// Project display label (shortest project_path basename, or the slug).
    pub label: String,
    pub source: String,
    pub started_at: String,
    pub ended_at: String,
    pub message_count: i64,
}

/// Ledger kind (Vue `kind` classification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerKind {
    NewWorkspace,
    NewSession,
    Continued,
}

/// Noise vs normal split (Vue `splitNoise`).
#[derive(Debug, Clone, Default)]
pub struct NoiseSplit {
    pub normal: Vec<ActivitySession>,
    pub noise: Vec<ActivitySession>,
    pub total: usize,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerBlock {
    /// "September 2026".
    pub header: String,
    /// "SEP 8" event chip label (day blocks only; empty for months).
    pub event_date: String,
    pub session_total: usize,
    pub new_workspaces: NoiseSplit,
    pub new_sessions: NoiseSplit,
    pub continued: NoiseSplit,
    pub is_empty: bool,
}

#[derive(Debug, Clone)]
pub struct ActivityCell {
    pub day: String,
    /// Daily token count — kept for tooltips and evidence assertions.
    #[allow(dead_code)]
    pub tokens: i64,
    pub level: u8,
    pub col: usize,
    pub row: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ActivityHeatmap {
    pub cells: Vec<ActivityCell>,
    /// (col, label) month marks on the top row.
    pub month_labels: Vec<(usize, &'static str)>,
    pub cols: usize,
}

const MONTHS_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Vue `NOISE_PROJECT_RE = /^(od-conn-test|[0-9a-f]{6,})/i` plus the
/// untitled rule (`!s.title`).
pub fn is_noise_session(title: &str, label: &str) -> bool {
    if title.is_empty() {
        return true;
    }
    let lower = label.to_ascii_lowercase();
    if lower.starts_with("od-conn-test") {
        return true;
    }
    // [0-9a-f]{6,} at the start of the label.
    let hex_prefix = lower.chars().take_while(|c| c.is_ascii_hexdigit()).count();
    hex_prefix >= 6
}

pub fn split_noise(sessions: Vec<ActivitySession>) -> NoiseSplit {
    let mut split = NoiseSplit::default();
    for session in sessions {
        if is_noise_session(&session.title, &session.label) {
            split.noise.push(session);
        } else {
            split.normal.push(session);
        }
    }
    split.total = split.normal.len() + split.noise.len();
    split
}

/// All sessions ordered by start time — the ledger's raw material. Source
/// labels come from the jsonl_path provider prefix (provider id before the
/// first `/`), falling back to "claude" like the Vue side.
pub fn load_activity_sessions(home: &Path) -> Vec<ActivitySession> {
    let Ok(conn) = db::open_read_db(home) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, COALESCE(title, ''), COALESCE(project, ''), COALESCE(started_at, ''),          COALESCE(ended_at, ''), COALESCE(message_count, 0), COALESCE(jsonl_path, '')          FROM sessions ORDER BY started_at",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok(ActivitySession {
            id: row.get(0)?,
            title: row.get(1)?,
            project: row.get(2)?,
            label: String::new(),
            source: String::new(),
            started_at: row.get(3)?,
            ended_at: row.get(4)?,
            message_count: row.get(5)?,
        })
    });
    let mut sessions: Vec<ActivitySession> = match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => return Vec::new(),
    };
    // Project label: shortest project_path basename per slug (Vue
    // formatProjectLabel), else the slug minus a leading dash.
    let mut labels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT project, project_path FROM sessions          WHERE project IS NOT NULL AND project_path IS NOT NULL",
    ) {
        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        for row in rows {
            let (slug, path) = match row {
                Ok(value) => value,
                Err(_) => continue,
            };
            if path.is_empty() {
                continue;
            }
            match labels.get_mut(&slug) {
                Some(existing) if existing.len() <= path.len() => {}
                _ => {
                    labels.insert(slug, path);
                }
            }
        }
    }
    for session in &mut sessions {
        let slug = session.project.clone();
        let label = match labels.get(&slug) {
            Some(path) => path.rsplit('/').next().unwrap_or(path).to_string(),
            None => slug.trim_start_matches('-').to_string(),
        };
        session.label = label;
    }
    // jsonl_path is not in the row above; re-derive source per session via a
    // second pass keyed on id (kept simple: one extra query).
    if let Ok(mut stmt) = conn.prepare("SELECT id, jsonl_path FROM sessions") {
        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        }) else {
            return sessions;
        };
        let mut sources: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for row in rows {
            let Ok((id, path)) = row else {
                continue;
            };
            let source = path
                .split('/')
                .find(|part| !part.is_empty())
                .unwrap_or("claude")
                .to_string();
            sources.insert(id, source);
        }
        for session in &mut sessions {
            session.source = sources
                .get(&session.id)
                .cloned()
                .unwrap_or_else(|| "claude".to_string());
        }
    }
    sessions
}

/// Vue `heatmapGrid`: 53-week grid starting at the Sunday on/after
/// (today - 364 days); levels 0..4 = ceil(tokens/max*4) capped at 4.
/// Day keys are LOCAL dates (the Vue toISOString quirk shifts a day in
/// UTC+8; we intentionally use local formatting so keys match the SQL
/// DATE(timestamp) output).
pub fn heatmap_grid(daily: &[UsageDay], today: chrono::NaiveDate) -> ActivityHeatmap {
    let mut map = std::collections::HashMap::new();
    for day in daily {
        map.insert(day.day.clone(), day.tokens);
    }
    let max_tokens = daily
        .iter()
        .map(|d| d.tokens)
        .filter(|t| *t > 0)
        .max()
        .unwrap_or(1)
        .max(1);
    let start_base = today - chrono::Duration::days(364);
    // JS getDay(): 0=Sunday. chrono: num_days_from_sunday().
    // %u: 1=Monday..7=Sunday → JS getDay (0=Sunday).
    let js_day = (start_base
        .format("%u")
        .to_string()
        .parse::<u32>()
        .unwrap_or(1))
        % 7;
    let days_until_sunday = (7 - js_day) % 7;
    let start = start_base + chrono::Duration::days(days_until_sunday as i64);

    let mut cells = Vec::new();
    let mut month_labels = Vec::new();
    let mut last_month: Option<u32> = None;
    let mut i: usize = 0;
    loop {
        let date = start + chrono::Duration::days(i as i64);
        if date > today {
            break;
        }
        let key = date.format("%Y-%m-%d").to_string();
        let tokens = map.get(&key).copied().unwrap_or(0);
        let level = if tokens == 0 {
            0
        } else {
            (((tokens as f64 / max_tokens as f64) * 4.0).ceil() as u8).min(4)
        };
        let col = i / 7;
        let row = i % 7;
        let month = date.month();
        if row == 0 && last_month != Some(month) {
            month_labels.push((col, MONTHS_SHORT[(month - 1) as usize]));
            last_month = Some(month);
        }
        cells.push(ActivityCell {
            day: key,
            tokens,
            level,
            col,
            row,
        });
        i += 1;
        if i >= 371 {
            break;
        }
    }
    let cols = cells.last().map(|c| c.col + 1).unwrap_or(0);
    ActivityHeatmap {
        cells,
        month_labels,
        cols,
    }
}

/// Vue currentStreak/longestStreak.
pub fn streaks(daily: &[UsageDay], today: chrono::NaiveDate) -> (u32, u32) {
    let map: std::collections::HashMap<&str, i64> =
        daily.iter().map(|d| (d.day.as_str(), d.tokens)).collect();
    let mut current = 0u32;
    let mut started = false;
    for i in 0..=365 {
        let key = (today - chrono::Duration::days(i))
            .format("%Y-%m-%d")
            .to_string();
        if map.get(key.as_str()).is_some_and(|t| *t > 0) {
            started = true;
            current += 1;
        } else if started {
            break;
        }
    }
    let mut days: Vec<&str> = daily
        .iter()
        .filter(|d| d.tokens > 0)
        .map(|d| d.day.as_str())
        .collect();
    days.sort_unstable();
    let mut longest = 0u32;
    let mut streak = 0u32;
    let mut prev: Option<chrono::NaiveDate> = None;
    for day in days {
        let date = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok();
        streak = match (date, prev) {
            (Some(d), Some(p)) if (d - p).num_days() == 1 => streak + 1,
            _ => 1,
        };
        prev = date;
        longest = longest.max(streak);
    }
    (current, longest)
}

/// Classify + split sessions into a ledger block for a date range.
/// Sessions overlap the range if started_at <= end 23:59:59 and
/// (ended_at || started_at) >= start 00:00:00 (Vue daySessions filter).
fn ledger_block_for(
    sessions: &[ActivitySession],
    range_start: &str,
    range_end: &str,
    header: String,
    event_date: String,
) -> LedgerBlock {
    let day_start = format!("{range_start}T00:00:00");
    let day_end = format!("{range_end}T23:59:59");
    let mut new_workspaces = Vec::new();
    let mut new_sessions = Vec::new();
    let mut continued = Vec::new();
    for session in sessions {
        if session.started_at.is_empty() {
            continue;
        }
        let end = if session.ended_at.is_empty() {
            session.started_at.clone()
        } else {
            session.ended_at.clone()
        };
        if session.started_at > day_end || end < day_start {
            continue;
        }
        let started_in_range = session.started_at >= day_start && session.started_at <= day_end;
        let kind = if started_in_range {
            let has_earlier = sessions.iter().any(|other| {
                other.project == session.project
                    && other.id != session.id
                    && other.started_at < session.started_at
            });
            if has_earlier {
                LedgerKind::NewSession
            } else {
                LedgerKind::NewWorkspace
            }
        } else {
            LedgerKind::Continued
        };
        match kind {
            LedgerKind::NewWorkspace => new_workspaces.push(session.clone()),
            LedgerKind::NewSession => new_sessions.push(session.clone()),
            LedgerKind::Continued => continued.push(session.clone()),
        }
    }
    let session_total = new_workspaces.len() + new_sessions.len() + continued.len();
    LedgerBlock {
        header,
        event_date,
        session_total,
        new_workspaces: split_noise(new_workspaces),
        new_sessions: split_noise(new_sessions),
        continued: split_noise(continued),
        is_empty: session_total == 0,
    }
}

/// Day ledger (Vue daySessions): selects one day for the heatmap click.
pub fn day_ledger(sessions: &[ActivitySession], date_key: &str) -> LedgerBlock {
    let month = date_key
        .get(5..7)
        .and_then(|m| m.parse::<usize>().ok())
        .unwrap_or(1);
    let year = date_key.get(0..4).unwrap_or_default();
    let header = format!("{} {}", MONTHS_FULL[(month - 1).min(11)], year);
    let day = date_key
        .get(8..10)
        .and_then(|d| d.parse::<u32>().ok())
        .unwrap_or(1);
    let event_date = format!(
        "{} {}",
        MONTHS_SHORT[(month - 1).min(11)].to_uppercase(),
        day
    );
    ledger_block_for(sessions, date_key, date_key, header, event_date)
}

/// Month ledger (Vue buildMonthBlock).
pub fn month_ledger(sessions: &[ActivitySession], year: i32, month0: u32) -> LedgerBlock {
    let month = month0 + 1;
    let range_start = format!("{year:04}-{month:02}-01");
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let range_end_last = chrono::NaiveDate::from_ymd_opt(ny, nm, 1)
        .map(|d| d - chrono::Duration::days(1))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| range_start.clone());
    let header = format!("{} {}", MONTHS_FULL[(month - 1) as usize % 12], year);
    ledger_block_for(
        sessions,
        &range_start,
        &range_end_last,
        header,
        String::new(),
    )
}

// ---- Overview stats (Vue db:getStats parity) ----

#[derive(Debug, Clone, Default)]
pub struct OverviewStats {
    pub sessions: i64,
    pub memories: i64,
    pub memories_archived: i64,
}

pub fn load_stats(home: &Path) -> OverviewStats {
    let Ok(conn) = db::open_read_db(home) else {
        return OverviewStats::default();
    };
    OverviewStats {
        sessions: conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap_or(0),
        memories: conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0),
        memories_archived: conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE deleted_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0),
    }
}

// ---- Recap (Vue recap:list / recap:read parity) ----

/// List weekly-recap JSON files from `~/.obelisk/recap`, newest first.
pub fn list_recaps(home: &Path) -> Vec<String> {
    let dir = home.join(".obelisk").join("recap");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    names.reverse();
    names
}

/// Read one recap file. The name is basename-checked like the TS side, so a
/// crafted name cannot escape the recap directory.
pub fn read_recap(home: &Path, filename: &str) -> Option<serde_json::Value> {
    let dir = home.join(".obelisk").join("recap");
    let base = std::path::Path::new(filename);
    if base.file_name()? != base.as_os_str() {
        return None;
    }
    let path = dir.join(base);
    if !path.starts_with(&dir) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Validate a provider root before saving (P0-9): absolute path or `~`,
/// and the directory must exist. Returns a user-facing error otherwise.
pub fn validate_provider_root(home: &Path, path: &str) -> Result<(), String> {
    let path = path.trim();
    if !(path.starts_with('/') || path.starts_with('~')) {
        return Err("Path must be absolute (start with / or ~)".to_string());
    }
    let expanded = if let Some(rest) = path.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else {
        std::path::PathBuf::from(path)
    };
    if !expanded.is_dir() {
        return Err(format!("Folder not found: {}", expanded.display()));
    }
    Ok(())
}

/// Persist one provider root override (`providerRoots[id]` in settings.json).
/// Plain-file write like the editor scheme: it never touches the SQLite
/// index, so it stays legal regardless of which process owns DB writes.
pub fn save_provider_root(home: &Path, provider_id: &str, root: &str) -> Result<(), String> {
    let settings_path = obelisk_core::provider_settings::settings_path(home);
    let mut value = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        obelisk_core::provider_settings::SettingsRead::Failed(error) => {
            return Err(error);
        }
    };
    if !value.is_object() {
        return Err("settings.json is not a JSON object".to_string());
    }
    let roots = value
        .as_object_mut()
        .unwrap()
        .entry("providerRoots")
        .or_insert_with(|| serde_json::json!({}));
    if !roots.is_object() {
        return Err("providerRoots in settings.json is not an object".to_string());
    }
    roots
        .as_object_mut()
        .unwrap()
        .insert(provider_id.to_string(), serde_json::json!(root));
    let serialized = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    let dir = settings_path
        .parent()
        .ok_or("settings path has no parent")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let temporary = settings_path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &settings_path).map_err(|e| e.to_string())?;
    Ok(())
}

// ---- Settings (Vue settings:get / settings:set parity; the settings file
// is plain JSON — writing it does not touch the SQLite index, so it stays
// legal regardless of which process owns the DB writes) ----

#[derive(Debug, Clone)]
pub struct SettingsSnapshot {
    pub raw: serde_json::Value,
}

impl SettingsSnapshot {
    pub fn provider_roots(&self) -> Vec<(String, String)> {
        self.raw
            .get("providerRoots")
            .and_then(|roots| roots.as_object())
            .map(|roots| {
                roots
                    .iter()
                    .filter_map(|(id, path)| {
                        path.as_str().map(|path| (id.clone(), path.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn editor_scheme(&self) -> String {
        self.raw
            .get("editorScheme")
            .and_then(|scheme| scheme.as_str())
            .unwrap_or(crate::file_reference::DEFAULT_EDITOR_SCHEME)
            .to_string()
    }
}

pub fn load_settings(home: &Path) -> SettingsSnapshot {
    let raw = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        _ => serde_json::json!({}),
    };
    SettingsSnapshot { raw }
}

/// Persist the settings file atomically (tmp + rename, Vue
/// `savePersistedSettings` pattern). Only `editorScheme` is editable in the
/// Stage-2 settings page.
pub fn save_editor_scheme(home: &Path, scheme: &str) -> Result<(), String> {
    let settings_path = obelisk_core::provider_settings::settings_path(home);
    let mut value = match obelisk_core::provider_settings::read_persisted_provider_settings(home) {
        obelisk_core::provider_settings::SettingsRead::Ok(value) => value,
        obelisk_core::provider_settings::SettingsRead::Failed(error) => {
            return Err(error);
        }
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("editorScheme".to_string(), serde_json::json!(scheme));
    }
    let serialized = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    let dir = settings_path
        .parent()
        .ok_or("settings path has no parent")?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temporary = settings_path.with_extension("json.tmp");
    std::fs::write(&temporary, serialized).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &settings_path).map_err(|error| error.to_string())
}

// ---- Full-text search (the agent-facing FTS contract, exposed in-app so
// the search box aligns with `obelisk --search`; read-only) ----

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub message_uuid: String,
    pub session_id: String,
    pub snippet: String,
    pub session_title: String,
}

pub fn search_messages(home: &Path, cwd: &Path, query: &str) -> Vec<SearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(conn) = obelisk_core::db::open_read_db(home) else {
        return Vec::new();
    };
    let obelisk_core::provider_settings::SettingsRead::Ok(persisted) =
        obelisk_core::provider_settings::read_persisted_provider_settings(home)
    else {
        return Vec::new();
    };
    let runtime = obelisk_core::provider_settings::create_configured_builtin_provider_runtime(
        home,
        cwd,
        &persisted,
        &Default::default(),
    );
    let api = obelisk_core::query::QueryApi {
        conn,
        registry: std::sync::Arc::new(runtime.registry),
        invoking_session_id: None,
        cwd: cwd.to_path_buf(),
    };
    let rows = api.search(
        &serde_json::json!(query),
        &serde_json::json!({ "limit": 50 }),
    );
    rows.iter()
        .filter_map(|row| {
            let session = row.get("session")?;
            Some(SearchHit {
                message_uuid: row
                    .get("uuid")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                session_id: session
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                snippet: row
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect(),
                session_title: session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(untitled)")
                    .to_string(),
            })
        })
        .collect()
}

fn read_projects(conn: &rusqlite::Connection) -> Vec<ProjectSummary> {
    let mut stmt = match conn.prepare(
        "SELECT project, COUNT(*) AS c
         FROM sessions
         WHERE project IS NOT NULL
         GROUP BY project
         ORDER BY MAX(COALESCE(ended_at, started_at)) DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(ProjectSummary {
                slug: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                session_count: row.get::<_, i64>(1)?.max(0) as usize,
            })
        })
        .unwrap_or_else(|_| panic!("projects query is valid"));
    rows.filter_map(Result::ok).collect()
}

fn read_sessions(conn: &rusqlite::Connection) -> Vec<SessionSummary> {
    let mut stmt = match conn.prepare(
        "SELECT id, COALESCE(title, ''), COALESCE(project, ''), COALESCE(source, 'claude'),
                COALESCE(started_at, ''), COALESCE(ended_at, ''),
                COALESCE(message_count, 0), COALESCE(git_branch, '')
         FROM sessions
         ORDER BY COALESCE(ended_at, started_at) DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = stmt
        .query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                project: row.get(2)?,
                source: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                // COALESCE(message_count, 0) is column 6.
                message_count: row.get(6)?,
                git_branch: row.get(7)?,
            })
        })
        .unwrap_or_else(|_| panic!("sessions query is valid"));
    rows.filter_map(Result::ok).collect()
}

// ---- session detail (timeline source) ----

#[derive(Debug, Clone)]
pub struct TimelineToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct TimelineToolCall {
    pub id: String,
    pub name: String,
    pub input_json: String,
    /// Kept for file-reference rendering (remaining M2.3 work).
    #[allow(dead_code)]
    pub file_path: Option<String>,
    pub result: Option<TimelineToolResult>,
    /// The workflow run record for Workflow tool calls.
    pub workflow: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SessionDetail {
    pub session_title: String,
    pub session_source: String,
    pub messages: Vec<crate::timeline::TimelineMessage>,
}

/// Load one session's timeline source: messages ordered by timestamp with
/// their tool calls joined to results and workflow runs. Reads only.
pub fn load_session_detail(conn: &rusqlite::Connection, session_id: &str) -> Option<SessionDetail> {
    let (session_title, session_source): (String, String) = conn
        .query_row(
            "SELECT COALESCE(title, '(untitled)'), COALESCE(source, 'claude') FROM sessions WHERE id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;

    // Messages in timeline order (visible first, then inactive/hidden for
    // disclosure; the timeline view groups them).
    let mut messages: Vec<crate::timeline::TimelineMessage> = Vec::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT uuid, session_id, type, timestamp, role, text, content_type,
                        COALESCE(is_meta, 0), COALESCE(visibility, 'visible'), model, agent_id,
                        input_tokens, output_tokens, cwd, COALESCE(source, 'claude')
                 FROM messages
                 WHERE session_id = ?1
                 ORDER BY (visibility = 'visible') DESC, timestamp",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(crate::timeline::TimelineMessage {
                    uuid: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    session_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r#type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    timestamp: row.get(3)?,
                    role: row.get(4)?,
                    text: row.get(5)?,
                    content_type: row.get(6)?,
                    is_meta: row.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
                    visibility: row
                        .get::<_, Option<String>>(8)?
                        .unwrap_or_else(|| "visible".to_string()),
                    model: row.get(9)?,
                    agent_id: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    cwd: row.get(13)?,
                    source: row
                        .get::<_, Option<String>>(14)?
                        .unwrap_or_else(|| "claude".to_string()),
                    tool_calls: Vec::new(),
                    workflow: None,
                    summary: None,
                    workflow_agents: Vec::new(),
                })
            })
            .ok()?;
        for message in rows.flatten() {
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return None;
    }

    // Summaries joined by message uuid (Vue message.summary).
    {
        let mut stmt = conn
            .prepare("SELECT id, content FROM summaries WHERE session_id = ?1")
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .ok()?;
        let summaries: std::collections::HashMap<String, String> = rows
            .flatten()
            .filter(|(id, content)| !id.is_empty() && !content.is_empty())
            .collect();
        for message in &mut messages {
            if let Some(content) = summaries.get(&message.uuid) {
                message.summary = Some(content.clone());
            }
        }
    }

    // Workflow agents joined by run id (grouped by phase in the card).
    {
        let mut stmt = conn
            .prepare("SELECT agent_id, COALESCE(agent_type,''), COALESCE(description,''), COALESCE(phase,''), COALESCE(label,''), COALESCE(state,''), COALESCE(duration_ms,0), COALESCE(tokens,0), COALESCE(tool_calls,0) FROM workflow_agents WHERE session_id = ?1")
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(crate::timeline::WorkflowAgentRow {
                    agent_id: row.get(0)?,
                    agent_type: row.get(1)?,
                    description: row.get(2)?,
                    phase: row.get(3)?,
                    label: row.get(4)?,
                    state: row.get(5)?,
                    duration_ms: row.get(6)?,
                    tokens: row.get(7)?,
                    tool_calls: row.get(8)?,
                })
            })
            .ok()?;
        let mut agents: Vec<crate::timeline::WorkflowAgentRow> = rows.flatten().collect();
        agents.sort_by(|a, b| a.phase.cmp(&b.phase).then(a.label.cmp(&b.label)));
        for message in &mut messages {
            if message.workflow.is_some() {
                message.workflow_agents = agents.clone();
            }
        }
    }

    // Tool calls keyed by message uuid.
    let mut calls_by_message: std::collections::HashMap<String, Vec<TimelineToolCall>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, message_uuid, name, input_json, file_path
                 FROM tool_calls
                 WHERE session_id = ?1
                 ORDER BY rowid",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                let call = TimelineToolCall {
                    id: row.get(0)?,
                    name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    input_json: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    file_path: row.get(4)?,
                    result: None,
                    workflow: None,
                };
                let message_uuid: Option<String> = row.get(1)?;
                Ok((message_uuid, call))
            })
            .ok()?;
        for row in rows.flatten() {
            let (message_uuid, call) = row;
            if let Some(message_uuid) = message_uuid {
                calls_by_message.entry(message_uuid).or_default().push(call);
            }
        }
    }

    // Tool results keyed by tool id.
    let mut results_by_tool: std::collections::HashMap<String, TimelineToolResult> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT tool_use_id, content, COALESCE(is_error, 0)
                 FROM tool_results
                 WHERE session_id = ?1",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    TimelineToolResult {
                        content: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        is_error: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
                    },
                ))
            })
            .ok()?;
        for row in rows.flatten() {
            results_by_tool.insert(row.0, row.1);
        }
    }

    // Workflow runs keyed by run_id (result text contains the run id).
    let mut workflows_by_run: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT run_id, session_id, parent_tool_use_id, workflow_name, status,
                        timestamp, agent_count, duration_ms, total_tokens
                 FROM workflows
                 WHERE session_id = ?1",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(serde_json::json!({
                    "run_id": row.get::<_, String>(0)?,
                    "session_id": row.get::<_, Option<String>>(1)?,
                    "parent_tool_use_id": row.get::<_, Option<String>>(2)?,
                    "workflow_name": row.get::<_, Option<String>>(3)?,
                    "status": row.get::<_, Option<String>>(4)?,
                    "timestamp": row.get::<_, Option<String>>(5)?,
                    "agent_count": row.get::<_, Option<i64>>(6)?,
                    "duration_ms": row.get::<_, Option<i64>>(7)?,
                    "total_tokens": row.get::<_, Option<i64>>(8)?,
                }))
            })
            .ok()?;
        for row in rows.flatten() {
            if let Some(run_id) = row.get("run_id").and_then(|v| v.as_str()) {
                workflows_by_run.insert(run_id.to_string(), row);
            }
        }
    }

    for message in &mut messages {
        if let Some(calls) = calls_by_message.remove(&message.uuid) {
            let mut attached = Vec::new();
            for mut call in calls {
                if let Some(result) = results_by_tool.remove(&call.id) {
                    call.result = Some(result);
                }
                // Link the workflow run when the result text names the run id.
                if call.name == "Workflow" {
                    if let Some(result) = &call.result {
                        for (run_id, workflow) in &workflows_by_run {
                            if result.content.contains(run_id) {
                                call.workflow = Some(workflow.clone());
                                break;
                            }
                        }
                    }
                }
                attached.push(call);
            }
            message.tool_calls = attached;
        }
    }

    Some(SessionDetail {
        session_title,
        session_source,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_provider_root;

    #[test]
    fn provider_root_validation() {
        let home = std::path::Path::new("/home/tester");
        // Relative paths are rejected outright.
        assert!(validate_provider_root(home, "relative/path").is_err());
        assert!(validate_provider_root(home, "").is_err());
        // Absolute but missing directories are rejected with "not found".
        let err = validate_provider_root(home, "/definitely/not/a/dir").unwrap_err();
        assert!(err.contains("not found") || err.contains("Folder"), "{err}");
        // Real directories pass (both plain-absolute and ~-expanded).
        assert!(validate_provider_root(home, "/tmp").is_ok());
        let err = validate_provider_root(home, "/definitely/not/a/dir");
        assert!(err.is_err());
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use chrono::NaiveDate;

    fn day(key: &str, tokens: i64) -> UsageDay {
        UsageDay {
            day: key.to_string(),
            tokens,
        }
    }

    #[test]
    fn noise_sessions_match_vue_rules() {
        // Untitled is always noise.
        assert!(is_noise_session("", "anything"));
        // od-conn-test prefix.
        assert!(is_noise_session("titled", "od-conn-test-1"));
        // 6+ leading hex digits.
        assert!(is_noise_session("titled", "abcdef"));
        assert!(is_noise_session("titled", "a1b2c3foo"));
        // Short hex or non-hex labels stay normal.
        assert!(!is_noise_session("titled", "abcde"));
        assert!(!is_noise_session("titled", "zzzzzz"));
        // Uppercase hex counts (the Vue regex is case-insensitive).
        assert!(is_noise_session("titled", "ABCDEF"));
    }

    #[test]
    fn heatmap_grid_levels_and_bounds() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();
        let daily = vec![
            day("2026-09-08", 400), // today: max -> level 4
            day("2026-09-07", 100), // ceil(100/400*4)=1
            day("2026-09-06", 200), // ceil(2)=2
            day("2026-09-05", 0),   // level 0
        ];
        let grid = heatmap_grid(&daily, today);
        assert!(!grid.cells.is_empty());
        assert!(grid.cells.len() <= 371);
        let today_cell = grid.cells.iter().find(|c| c.day == "2026-09-08").unwrap();
        assert_eq!(today_cell.level, 4);
        let d7 = grid.cells.iter().find(|c| c.day == "2026-09-07").unwrap();
        assert_eq!(d7.level, 1);
        let d6 = grid.cells.iter().find(|c| c.day == "2026-09-06").unwrap();
        assert_eq!(d6.level, 2);
        let d5 = grid.cells.iter().find(|c| c.day == "2026-09-05").unwrap();
        assert_eq!(d5.level, 0);
        // The grid starts on a Sunday (row 0, col 0).
        assert_eq!(grid.cells[0].row, 0);
        assert_eq!(grid.cells[0].col, 0);
    }

    #[test]
    fn streaks_match_vue_semantics() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 8).unwrap();
        // Current: 3-day run ending today; longest includes a 4-day run.
        let daily = vec![
            day("2026-09-08", 10),
            day("2026-09-07", 10),
            day("2026-09-06", 10),
            day("2026-06-01", 10),
            day("2026-06-02", 10),
            day("2026-06-03", 10),
            day("2026-06-04", 10),
        ];
        let (current, longest) = streaks(&daily, today);
        assert_eq!(current, 3);
        assert_eq!(longest, 4);
    }

    #[test]
    fn day_ledger_classifies_and_splits_noise() {
        let sessions = vec![
            ActivitySession {
                id: "first".into(),
                title: "First workspace".into(),
                project: "proj-a".into(),
                label: "proj-a".into(),
                source: "deepseek".into(),
                started_at: "2026-09-07T10:00:00".into(),
                ended_at: "2026-09-07T11:00:00".into(),
                message_count: 5,
            },
            ActivitySession {
                id: "second".into(),
                title: "Second in same workspace".into(),
                project: "proj-a".into(),
                label: "proj-a".into(),
                source: "deepseek".into(),
                started_at: "2026-09-07T12:00:00".into(),
                ended_at: String::new(),
                message_count: 3,
            },
            ActivitySession {
                id: "carried".into(),
                title: "Carried over".into(),
                project: "proj-b".into(),
                label: "proj-b".into(),
                source: "deepseek".into(),
                started_at: "2026-09-06T23:00:00".into(),
                ended_at: "2026-09-07T09:30:00".into(),
                message_count: 7,
            },
            ActivitySession {
                id: "throwaway".into(),
                title: String::new(),
                project: "hex123456".into(),
                label: "abcdef12".into(),
                source: "deepseek".into(),
                started_at: "2026-09-07T09:00:00".into(),
                ended_at: String::new(),
                message_count: 1,
            },
        ];
        let block = day_ledger(&sessions, "2026-09-07");
        assert_eq!(block.session_total, 4);
        // first: started that day, no earlier proj-a session -> workspace.
        assert_eq!(block.new_workspaces.normal.len(), 1);
        assert_eq!(block.new_workspaces.normal[0].id, "first");
        // second: earlier proj-a session exists -> new-session.
        assert_eq!(block.new_sessions.normal.len(), 1);
        assert_eq!(block.new_sessions.normal[0].id, "second");
        // carried: started before the day -> continued.
        assert_eq!(block.continued.normal.len(), 1);
        assert_eq!(block.continued.normal[0].id, "carried");
        // throwaway: untitled -> noise, still counted in its group.
        assert_eq!(block.new_workspaces.noise.len(), 1);
        assert_eq!(block.new_workspaces.total, 2);
        // Header formats.
        assert_eq!(block.header, "September 2026");
        assert_eq!(block.event_date, "SEP 7");
    }

    #[test]
    fn month_ledger_range_is_calendar_month() {
        let sessions = vec![ActivitySession {
            id: "s".into(),
            title: "t".into(),
            project: "p".into(),
            label: "p".into(),
            source: "deepseek".into(),
            started_at: "2026-08-15T10:00:00".into(),
            ended_at: String::new(),
            message_count: 1,
        }];
        let block = month_ledger(&sessions, 2026, 7); // August (month0=7)
        assert_eq!(block.header, "August 2026");
        assert_eq!(block.session_total, 1);
        let off = month_ledger(&sessions, 2026, 8); // September: empty
        assert!(off.is_empty);
    }
}
