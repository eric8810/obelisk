// Copyright (C) 2026 tommy0103 and contributors.
// SPDX-License-Identifier: AGPL-3.0-only

//! Persisted provider settings + configured runtime (port of
//! packages/core/src/provider-settings.ts and providers/builtins.ts).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::providers::claude::ClaudeProvider;
use crate::providers::codex::CodexProvider;
use crate::providers::deepseek::DeepseekProvider;
use crate::providers::kimi::KimiProvider;
use crate::providers::pi::PiProvider;
use crate::providers::types::{
    DiscoverContext, IndexUnit, ProviderAdapter, ProviderDescriptor, ProviderRegistry, WatchTarget,
};

pub fn settings_path(home: &Path) -> PathBuf {
    home.join(".obelisk").join("settings.json")
}

#[derive(Debug, Clone)]
pub enum SettingsRead {
    Ok(Value),
    Failed(String),
}

/// Read ~/.obelisk/settings.json; missing file is `Ok(empty object)`.
/// A malformed file (not an object / providerRoots not an object / unparsable)
/// fails honestly.
pub fn read_persisted_provider_settings(home: &Path) -> SettingsRead {
    let path = settings_path(home);
    if !path.exists() {
        return SettingsRead::Ok(serde_json::json!({}));
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            return SettingsRead::Failed(format!(
                "Unable to read Obelisk settings at {}: {}",
                path.display(),
                error
            ))
        }
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            return SettingsRead::Failed(format!(
                "Unable to read Obelisk settings at {}: {error}",
                path.display()
            ))
        }
    };
    if !parsed.is_object() {
        return SettingsRead::Failed(format!(
            "Obelisk settings are not an object: {}",
            path.display()
        ));
    }
    if let Some(roots) = parsed.get("providerRoots") {
        if !roots.is_null() && !roots.is_object() {
            return SettingsRead::Failed(format!(
                "Obelisk providerRoots are not an object: {}",
                path.display()
            ));
        }
    }
    SettingsRead::Ok(parsed)
}

/// `~`-expanding absolute-path validation (TS `configuredPath`).
fn configured_path(value: &Value, home_dir: &Path) -> Option<PathBuf> {
    let raw = value.as_str()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded: PathBuf = if trimmed == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        home_dir.join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if expanded.is_absolute() {
        Some(PathBuf::from(crate::parsing::normalize_path(
            &expanded.to_string_lossy(),
        )))
    } else {
        None
    }
}

/// Resolve configured provider roots from persisted settings. Modern key:
/// `providerRoots.<id>`; legacy key: `<id>Dir` at the top level.
pub fn resolve_provider_roots(
    registry: &ProviderRegistry,
    persisted: &Value,
    home_dir: &Path,
) -> HashMap<String, PathBuf> {
    let mut roots: HashMap<String, PathBuf> = HashMap::new();
    let configured = persisted
        .get("providerRoots")
        .filter(|v| v.is_object())
        .and_then(|v| v.as_object());
    for descriptor in registry.catalog() {
        let modern = configured
            .and_then(|map| map.get(descriptor.id))
            .filter(|v| !v.is_null());
        let legacy = persisted
            .get(format!("{}Dir", descriptor.id))
            .filter(|v| !v.is_null());
        if let Some(value) = modern.or(legacy) {
            if let Some(explicit) = configured_path(value, home_dir) {
                roots.insert(descriptor.id.to_string(), explicit);
            }
        } else if !descriptor.requires_explicit_root {
            roots.insert(
                descriptor.id.to_string(),
                PathBuf::from(&descriptor.default_root),
            );
        }
    }
    roots
}

/// A provider whose configured root failed to resolve: no discovery, no
/// watch targets, honest inventory reporting (TS `createConfiguredBuiltinProviderRuntime`
/// wrapper).
struct UnresolvedRootProvider {
    inner: Arc<dyn ProviderAdapter>,
    reason: String,
    descriptor: ProviderDescriptor,
}

impl ProviderAdapter for UnresolvedRootProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            requires_explicit_root: true,
            root_resolution_reason: Some(self.reason.clone()),
            ..self.descriptor.clone()
        }
    }
    fn index_version_marker(&self) -> Option<&'static str> {
        self.inner.index_version_marker()
    }
    fn session_unit_key(
        &self,
        session: &crate::providers::types::IndexedSession,
    ) -> Option<String> {
        self.inner.session_unit_key(session)
    }
    fn watch_targets(&self, _configured_root: &str) -> Vec<WatchTarget> {
        Vec::new()
    }
    fn discover<'a>(&'a self, ctx: &mut DiscoverContext<'a>) -> Vec<IndexUnit> {
        let indexed = ctx.indexed_sessions();
        if let Some(first) = indexed.first() {
            ctx.report_incomplete(crate::providers::types::InventoryIssue {
                path: first.jsonl_path.clone(),
                error: self.reason.clone(),
            });
        }
        Vec::new()
    }
    fn parse<'a>(
        &'a self,
        unit: &'a IndexUnit,
        _cursor: crate::providers::types::Cursor,
    ) -> crate::providers::types::ParseStream<'a> {
        self.inner.parse(unit, None)
    }
    fn raw(
        &self,
        input: &crate::providers::types::RawLookup,
    ) -> Option<crate::providers::types::RawRecord> {
        self.inner.raw(input)
    }
}

/// The builtin five-provider registry with optional root overrides.
/// `base_roots` mirror the TS `BuiltinProviderRoots`.
pub fn create_builtin_provider_registry(
    home: &Path,
    base_roots: &HashMap<String, PathBuf>,
    cwd: &Path,
) -> ProviderRegistry {
    let claude_root = base_roots
        .get("claude")
        .cloned()
        .unwrap_or_else(|| crate::parsing::claude_dir(home));
    let codex_root = base_roots
        .get("codex")
        .cloned()
        .unwrap_or_else(|| crate::parsing::codex_dir(home));
    // deepseek: explicit root override, else DSH_HOME / ~/.dsh/sessions
    // (resolved inside the provider, mirroring deepseekSessionsRoot()).
    let deepseek = match base_roots.get("deepseek") {
        Some(root) => DeepseekProvider::with_root(root.clone()),
        None => DeepseekProvider::new(),
    };
    // kimi: KIMI_CODE_HOME / ~/.kimi-code (mirrors defaultKimiRoot()).
    let kimi_root = base_roots.get("kimi").cloned().unwrap_or_else(|| {
        std::env::var_os("KIMI_CODE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".kimi-code"))
    });
    // pi: full createPiProvider resolution (env + launch cwd).
    let pi = PiProvider::create(
        base_roots
            .get("pi")
            .map(|p| p.to_string_lossy().into_owned()),
        Some(cwd.to_string_lossy().into_owned()),
    );
    let providers: Vec<Arc<dyn ProviderAdapter>> = vec![
        Arc::new(ClaudeProvider::new(claude_root)),
        Arc::new(CodexProvider::new(codex_root)),
        Arc::new(deepseek),
        Arc::new(KimiProvider::new(kimi_root)),
        Arc::new(pi),
    ];
    ProviderRegistry::new(providers).expect("builtin provider registry is consistent")
}

pub struct ConfiguredRuntime {
    pub roots: HashMap<String, PathBuf>,
    pub registry: ProviderRegistry,
}

/// Settings-aware runtime: providers whose roots fail to resolve become
/// honest no-discovery wrappers (TS `createConfiguredBuiltinProviderRuntime`).
pub fn create_configured_builtin_provider_runtime(
    home: &Path,
    cwd: &Path,
    persisted: &Value,
    base_roots: &HashMap<String, PathBuf>,
) -> ConfiguredRuntime {
    let defaults = create_builtin_provider_registry(home, base_roots, cwd);
    let roots = resolve_provider_roots(&defaults, persisted, home);
    let mut resolved_roots: HashMap<String, PathBuf> = base_roots.clone();
    for (id, path) in &roots {
        resolved_roots.insert(id.clone(), path.clone());
    }
    let configured = create_builtin_provider_registry(home, &resolved_roots, cwd);
    let wrapped: Vec<Arc<dyn ProviderAdapter>> = configured
        .list()
        .into_iter()
        .map(|provider| {
            if roots.contains_key(provider.name()) {
                return provider;
            }
            let descriptor = provider.descriptor();
            let reason = descriptor
                .root_resolution_reason
                .clone()
                .unwrap_or_else(|| {
                    format!(
                        "Configured {} root must be absolute or start with ~",
                        provider.name()
                    )
                });
            Arc::new(UnresolvedRootProvider {
                inner: provider,
                reason,
                descriptor,
            }) as Arc<dyn ProviderAdapter>
        })
        .collect();
    ConfiguredRuntime {
        roots,
        registry: ProviderRegistry::new(wrapped).expect("wrapped registry is consistent"),
    }
}
