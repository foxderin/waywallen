use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::wallpaper_type::WallpaperType;

// ---------------------------------------------------------------------------
// Manifest (TOML on disk)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RendererManifest {
    pub renderer: RendererDef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RendererDef {
    pub name: String,
    pub bin: PathBuf,
    pub types: Vec<WallpaperType>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_version")]
    pub version: String,
    /// Wire-protocol `Init.spawn_version` the daemon should emit when
    /// spawning this renderer. `None` means "use the daemon's compile-
    /// time `SPAWN_VERSION`" (legacy manifests). Step 2 keeps this
    /// optional so old manifests behave identically; Step 3 will make
    /// it the source of truth.
    #[serde(default)]
    pub spawn_version: Option<u32>,
    /// Allow-listed extra metadata keys (beyond the canonical `path`).
    /// Forwarded verbatim as `Init.resource_extras`. The daemon warns
    /// when source-plugin metadata carries a key that's neither `path`
    /// nor in this list nor in `settings`; Step 3 will turn the warning
    /// into an error.
    ///
    /// Empty (the default) means "no extras". A manifest is considered
    /// "schema-bearing" iff it has a non-empty `extras` OR a non-empty
    /// `settings` table — the legacy "no schema" fall-through stays in
    /// place for old manifests until OWE migrates wescene.
    #[serde(default)]
    pub extras: Vec<String>,
    /// Optional schema for plugin-level settings (e.g. mpv's
    /// `loop_file`, wescene's `volume`). Each entry declares a type
    /// and a default; missing entries are filled from the default at
    /// validation time. Empty (the default) means "no schema, no
    /// validation".
    #[serde(default)]
    pub settings: HashMap<String, SettingDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingDef {
    #[serde(rename = "type")]
    pub ty: SettingType,
    pub default: toml::Value,
    /// When `true` (the default), the setting participates in the
    /// renderer's identity hash — changing it should respawn the
    /// renderer. When `false`, the setting is hot-applicable and
    /// changes should be dispatched as `ApplySettings` instead
    /// (Step 4 work).
    #[serde(default = "default_true")]
    pub identity: bool,
    /// i18n key the UI binds to for the field label
    /// (e.g. `"settings.video.loop_file"`). Optional — old manifests
    /// without this stay valid; the UI falls back to the raw key name.
    #[serde(default)]
    pub label_key: Option<String>,
    /// Optional i18n key for a short helper / tooltip line.
    #[serde(default)]
    pub description_key: Option<String>,
    /// Numeric lower bound (inclusive) for `U32`/`F32` settings.
    /// Ignored on string/bool. Out-of-range values from `SettingsSet`
    /// are rejected; out-of-range values found at startup fall back
    /// to `default` with a warning.
    #[serde(default)]
    pub min: Option<toml::Value>,
    /// Numeric upper bound (inclusive). Same semantics as `min`.
    #[serde(default)]
    pub max: Option<toml::Value>,
    /// Optional UI hint for slider/spinner step. Daemon does not
    /// enforce step alignment (would block legitimate fine-grained
    /// values); UIs can choose to snap.
    #[serde(default)]
    pub step: Option<toml::Value>,
    /// Allowed string values. Only valid for `String` settings;
    /// values outside the list are rejected by `SettingsSet` and
    /// reset to default at startup.
    #[serde(default)]
    pub choices: Option<Vec<String>>,
    /// Logical group key. UI groups settings sharing this name into
    /// the same panel section. `None` = ungrouped.
    #[serde(default)]
    pub group: Option<String>,
    /// Sort order within a group. Lower goes first. `0` for unspecified.
    #[serde(default)]
    pub order: Option<i32>,
}

impl SettingDef {
    /// Bare-minimum constructor — fills the optional schema metadata
    /// (`label_key`, `min`, `choices`, …) with `None`. Real manifests
    /// flow through `serde::Deserialize` and never touch this; tests
    /// and ad-hoc programmatic builds use it as a base.
    pub fn new(ty: SettingType, default: toml::Value, identity: bool) -> Self {
        Self {
            ty,
            default,
            identity,
            label_key: None,
            description_key: None,
            min: None,
            max: None,
            step: None,
            choices: None,
            group: None,
            order: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SettingType {
    U32,
    F32,
    String,
    Bool,
}

fn default_priority() -> u32 {
    100
}

fn default_version() -> String {
    "v0.0.0".into()
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Metadata validation
// ---------------------------------------------------------------------------

/// True iff the manifest carries any schema fields (`extras` or
/// `settings`). When false, validation falls through to the legacy
/// "no schema" pass-through used by unmigrated renderers (wescene
/// today).
fn has_schema(def: &RendererDef) -> bool {
    !def.extras.is_empty() || !def.settings.is_empty()
}

/// Outcome of `validate_metadata`: typecast string values ready for
/// the `Init` wire fields, plus a list of (non-fatal) warnings the
/// caller should log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ValidatedMetadata {
    /// Value at `metadata["path"]`. Empty string when the manifest
    /// has no schema.
    pub primary_value: String,
    /// Allow-listed extras (key → metadata value). Only populated when
    /// the manifest has a schema.
    pub extras: HashMap<String, String>,
    /// Plugin settings, already typecast to strings for the wire. When
    /// the manifest has no `settings` table this is the pass-through
    /// copy of the input metadata's plugin-shaped keys (Step 2 keeps
    /// "no schema" semantics broad — the legacy spawn path just
    /// forwards everything as argv).
    pub settings: HashMap<String, String>,
    /// Non-fatal observations: unknown metadata keys, defaulted
    /// settings, etc. Step 3 will tighten some of these into errors.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValidationError {
    /// Required `path` key was missing from metadata or had an empty
    /// value.
    MissingPath,
    /// Setting value couldn't be coerced into the schema's declared
    /// type.
    BadSettingType {
        key: String,
        expected: SettingType,
        got: String,
    },
    /// Numeric setting value fell outside the manifest's `[min, max]`
    /// envelope.
    OutOfRange {
        key: String,
        got: String,
        min: Option<String>,
        max: Option<String>,
    },
    /// String setting value didn't match any entry in the manifest's
    /// `choices` allowlist.
    BadChoice {
        key: String,
        got: String,
        choices: Vec<String>,
    },
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::MissingPath => {
                write!(f, "missing required metadata key 'path'")
            }
            ValidationError::BadSettingType { key, expected, got } => write!(
                f,
                "plugin setting '{key}' expected type {expected:?}, got {got:?}"
            ),
            ValidationError::OutOfRange { key, got, min, max } => write!(
                f,
                "plugin setting '{key}' value {got:?} out of range (min={min:?}, max={max:?})"
            ),
            ValidationError::BadChoice { key, got, choices } => write!(
                f,
                "plugin setting '{key}' value {got:?} not in allowed choices {choices:?}"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Validate a source-plugin's `metadata` map against a manifest's
/// optional `extras` / `settings` schema.
///
/// Behaviour matrix:
///
/// - schema-bearing manifest (`extras` and/or `settings` non-empty):
///   require `metadata["path"]` to exist and be non-empty; surface a
///   warning for any metadata key that's neither `"path"`, in
///   `extras`, nor declared in `settings` (Step 3 will turn into
///   error).
/// - `def.settings` non-empty: typecheck each known setting against
///   its schema; missing keys are filled from `default`. Bad types
///   accumulate into `Vec<ValidationError>`.
/// - schema-less (both empty): no schema → pass `metadata` through
///   verbatim as `settings` to preserve today's "argv fall-through"
///   behaviour. The caller logs an info line at the call site.
pub fn validate_metadata(
    def: &RendererDef,
    md: &HashMap<String, String>,
) -> std::result::Result<ValidatedMetadata, Vec<ValidationError>> {
    let mut errors: Vec<ValidationError> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let schema_active = has_schema(def);

    // ----- resource section -----
    let (primary_value, extras) = if schema_active {
        let primary_value = match md.get("path") {
            Some(v) if !v.is_empty() => v.clone(),
            _ => {
                errors.push(ValidationError::MissingPath);
                String::new()
            }
        };
        let allowed: std::collections::HashSet<&str> = std::iter::once("path")
            .chain(def.extras.iter().map(|s| s.as_str()))
            .chain(def.settings.keys().map(|s| s.as_str()))
            .collect();
        let mut extras_out: HashMap<String, String> = HashMap::new();
        for (k, v) in md {
            if k == "path" {
                continue;
            }
            if def.settings.contains_key(k) {
                // Settings have their own schema branch below.
                continue;
            }
            if def.extras.iter().any(|e| e == k) {
                extras_out.insert(k.clone(), v.clone());
            } else if !allowed.contains(k.as_str()) {
                warnings.push(format!(
                    "unknown metadata key '{k}' for renderer '{}' \
                     (not 'path', not in extras, not in settings); \
                     Step 3 will turn into error",
                    def.name
                ));
            }
        }
        (primary_value, extras_out)
    } else {
        (String::new(), HashMap::new())
    };

    // ----- settings section -----
    let settings: HashMap<String, String> = if def.settings.is_empty() {
        // No schema → pass-through. Filter out the keys we already
        // claimed for the resource section so we don't duplicate them.
        if schema_active {
            md.iter()
                .filter(|(k, _)| {
                    let in_primary = k.as_str() == "path";
                    let in_extras = def.extras.iter().any(|e| e == *k);
                    !in_primary && !in_extras
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            md.clone()
        }
    } else {
        let mut out: HashMap<String, String> = HashMap::new();
        for (key, schema) in &def.settings {
            match md.get(key) {
                Some(raw) => match coerce_setting(raw, schema.ty) {
                    Some(coerced) => {
                        if let Err(e) = check_setting_bounds(key, &coerced, schema) {
                            errors.push(e);
                        } else {
                            out.insert(key.clone(), coerced);
                        }
                    }
                    None => errors.push(ValidationError::BadSettingType {
                        key: key.clone(),
                        expected: schema.ty,
                        got: raw.clone(),
                    }),
                },
                None => {
                    let coerced = match toml_default_to_string(&schema.default, schema.ty) {
                        Some(s) => s,
                        None => {
                            // Defaults that can't be stringified are a
                            // manifest authoring bug; surface as warn
                            // and skip rather than crash.
                            warnings.push(format!(
                                "plugin setting '{key}' has default \
                                 incompatible with declared type \
                                 {:?}; skipping",
                                schema.ty
                            ));
                            continue;
                        }
                    };
                    out.insert(key.clone(), coerced);
                    warnings.push(format!(
                        "plugin setting '{key}' filled from manifest default"
                    ));
                }
            }
        }
        out
    };

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(ValidatedMetadata {
        primary_value,
        extras,
        settings,
        warnings,
    })
}

/// Validate one already-typecast setting value against the manifest's
/// optional `min` / `max` / `choices` envelope. Returns `Ok(())` when
/// the value is in range or no constraints apply. Used both by
/// `validate_metadata` (per-spawn metadata path) and by the
/// `SettingsSet` RPC handler (incoming user edits).
pub fn check_setting_bounds(
    key: &str,
    coerced: &str,
    schema: &SettingDef,
) -> std::result::Result<(), ValidationError> {
    match schema.ty {
        SettingType::U32 => {
            let v: u32 = coerced.parse().map_err(|_| ValidationError::BadSettingType {
                key: key.to_string(),
                expected: SettingType::U32,
                got: coerced.to_string(),
            })?;
            if let Some(min_v) = schema.min.as_ref().and_then(toml_to_u32) {
                if v < min_v {
                    return Err(out_of_range(key, coerced, schema));
                }
            }
            if let Some(max_v) = schema.max.as_ref().and_then(toml_to_u32) {
                if v > max_v {
                    return Err(out_of_range(key, coerced, schema));
                }
            }
            Ok(())
        }
        SettingType::F32 => {
            let v: f32 = coerced.parse().map_err(|_| ValidationError::BadSettingType {
                key: key.to_string(),
                expected: SettingType::F32,
                got: coerced.to_string(),
            })?;
            if let Some(min_v) = schema.min.as_ref().and_then(toml_to_f32) {
                if v < min_v {
                    return Err(out_of_range(key, coerced, schema));
                }
            }
            if let Some(max_v) = schema.max.as_ref().and_then(toml_to_f32) {
                if v > max_v {
                    return Err(out_of_range(key, coerced, schema));
                }
            }
            Ok(())
        }
        SettingType::String => {
            if let Some(choices) = schema.choices.as_ref() {
                if !choices.iter().any(|c| c == coerced) {
                    return Err(ValidationError::BadChoice {
                        key: key.to_string(),
                        got: coerced.to_string(),
                        choices: choices.clone(),
                    });
                }
            }
            Ok(())
        }
        SettingType::Bool => Ok(()),
    }
}

/// Top-level entry for `SettingsSet`: typecheck a raw user value
/// against the manifest schema and run bounds validation. Returns the
/// canonicalised string on success.
pub fn coerce_and_validate(
    key: &str,
    raw: &str,
    schema: &SettingDef,
) -> std::result::Result<String, ValidationError> {
    let coerced = coerce_setting(raw, schema.ty).ok_or_else(|| ValidationError::BadSettingType {
        key: key.to_string(),
        expected: schema.ty,
        got: raw.to_string(),
    })?;
    check_setting_bounds(key, &coerced, schema)?;
    Ok(coerced)
}

fn out_of_range(key: &str, coerced: &str, schema: &SettingDef) -> ValidationError {
    ValidationError::OutOfRange {
        key: key.to_string(),
        got: coerced.to_string(),
        min: schema.min.as_ref().map(toml_value_to_display),
        max: schema.max.as_ref().map(toml_value_to_display),
    }
}

fn toml_value_to_display(v: &toml::Value) -> String {
    match v {
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn toml_to_u32(v: &toml::Value) -> Option<u32> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => u32::try_from(*i).ok(),
        toml::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn toml_to_f32(v: &toml::Value) -> Option<f32> {
    match v {
        toml::Value::Integer(i) => Some(*i as f32),
        toml::Value::Float(f) => Some(*f as f32),
        toml::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Try to interpret a raw `String` as a value of `ty`. Returns the
/// canonicalised string form on success (e.g. `"42"` for u32, `"true"`
/// for bool). Returns `None` if the value can't be coerced.
fn coerce_setting(raw: &str, ty: SettingType) -> Option<String> {
    match ty {
        SettingType::U32 => raw.parse::<u32>().ok().map(|v| v.to_string()),
        SettingType::F32 => raw.parse::<f32>().ok().map(|v| v.to_string()),
        SettingType::Bool => match raw {
            "true" | "false" => Some(raw.to_string()),
            _ => None,
        },
        SettingType::String => Some(raw.to_string()),
    }
}

/// Stringify a `toml::Value` default, coerced to `ty`. Returns `None`
/// when the default is structurally incompatible (e.g. an array as a
/// `u32` default).
fn toml_default_to_string(value: &toml::Value, ty: SettingType) -> Option<String> {
    match (value, ty) {
        (toml::Value::Integer(i), SettingType::U32) => {
            if *i >= 0 {
                Some(i.to_string())
            } else {
                None
            }
        }
        (toml::Value::Integer(i), SettingType::F32) => Some((*i as f32).to_string()),
        (toml::Value::Float(f), SettingType::F32) => Some(f.to_string()),
        (toml::Value::Boolean(b), SettingType::Bool) => Some(b.to_string()),
        (toml::Value::String(s), SettingType::String) => Some(s.clone()),
        // Common manifest mistake: declaring `default = "30"` for a
        // u32 setting. Be lenient — try to parse the string.
        (toml::Value::String(s), other) => coerce_setting(s, other),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct RendererRegistry {
    /// type → list of RendererDef sorted by descending priority.
    by_type: HashMap<WallpaperType, Vec<RendererDef>>,
}

impl RendererRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
        }
    }

    /// Scan a directory for `*.toml` renderer manifest files and populate
    /// the registry. Non-parseable files are logged and skipped.
    pub fn scan(dir: &Path) -> Result<Self> {
        let mut reg = Self::new();
        let pattern = dir.join("*.toml");
        let pattern_str = pattern
            .to_str()
            .context("manifest dir path not valid UTF-8")?;
        for entry in glob::glob(pattern_str).context("glob pattern")? {
            let path = match entry {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("renderer manifest glob error: {e}");
                    continue;
                }
            };
            match std::fs::read_to_string(&path) {
                Ok(contents) => match toml::from_str::<RendererManifest>(&contents) {
                    Ok(mut manifest) => {
                        // Resolve relative bin paths against the manifest's directory.
                        if manifest.renderer.bin.is_relative() {
                            if let Some(manifest_dir) = path.parent() {
                                manifest.renderer.bin = manifest_dir.join(&manifest.renderer.bin);
                            }
                        }
                        log::info!(
                            "loaded renderer manifest: {} (types: {:?})",
                            manifest.renderer.name,
                            manifest.renderer.types
                        );
                        reg.register(manifest.renderer);
                    }
                    Err(e) => log::warn!("skip {}: {e}", path.display()),
                },
                Err(e) => log::warn!("skip {}: {e}", path.display()),
            }
        }
        Ok(reg)
    }

    /// Register a renderer definition programmatically.
    pub fn register(&mut self, def: RendererDef) {
        for wp_type in &def.types {
            let list = self.by_type.entry(wp_type.clone()).or_default();
            list.push(def.clone());
            list.sort_by(|a, b| b.priority.cmp(&a.priority));
        }
    }

    /// Find the highest-priority renderer for a wallpaper type.
    pub fn resolve(&self, wp_type: &str) -> Option<&RendererDef> {
        self.by_type.get(wp_type)?.first()
    }

    /// Find a renderer by its manifest `name`, regardless of type.
    /// Returns the first occurrence — `register` keeps duplicates
    /// across `by_type` so we walk every bucket and stop at the first
    /// match.
    pub fn resolve_by_name(&self, name: &str) -> Option<&RendererDef> {
        self.by_type
            .values()
            .flat_map(|v| v.iter())
            .find(|d| d.name == name)
    }

    /// List all wallpaper types that have at least one renderer.
    pub fn supported_types(&self) -> Vec<&WallpaperType> {
        self.by_type.keys().collect()
    }

    /// List all registered renderer definitions (deduplicated by name).
    pub fn all_renderers(&self) -> Vec<&RendererDef> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for defs in self.by_type.values() {
            for def in defs {
                if seen.insert(&def.name) {
                    out.push(def);
                }
            }
        }
        out
    }
}

/// Build a registry by scanning the two canonical plugin paths:
/// 1. `<exec>/../share/waywallen/renderers/`  (bundled / system install)
/// 2. `$XDG_DATA_HOME/waywallen/renderers/`   (user overrides)
///
/// User-local manifests (XDG) are loaded last so they can shadow bundled
/// ones by name. Non-existent directories are silently skipped.
pub fn build_default_registry() -> Result<RendererRegistry> {
    let mut registry = RendererRegistry::new();

    for dir in standard_plugin_dirs("renderers") {
        if dir.is_dir() {
            match RendererRegistry::scan(&dir) {
                Ok(scanned) => {
                    for def in scanned.all_renderers() {
                        registry.register(def.clone());
                    }
                }
                Err(e) => log::warn!("scan {}: {e}", dir.display()),
            }
        }
    }

    Ok(registry)
}

/// Return the two canonical plugin directories (bundled + XDG) for a
/// given subdirectory name (e.g. `"renderers"` or `"sources"`). Returned
/// in load order: bundled first, user-local second.
pub fn standard_plugin_dirs(subdir: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // Bundled: <exec>/../share/waywallen/<subdir>/
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if let Some(prefix) = parent.parent() {
                dirs.push(prefix.join("share/waywallen").join(subdir));
            }
        }
    }

    // User-local: $XDG_DATA_HOME/waywallen/<subdir>/
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            PathBuf::from(home).join(".local/share")
        });
    dirs.push(xdg.join("waywallen").join(subdir));

    dirs
}

/// Returns true when the manifest has any schema fields. Public so
/// `renderer_manager::build_init_msg` can branch on it without
/// duplicating the predicate.
pub fn manifest_has_schema(def: &RendererDef) -> bool {
    has_schema(def)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod schema_tests {
    use super::*;

    fn test_setting(ty: SettingType, default: toml::Value, identity: bool) -> SettingDef {
        SettingDef {
            ty,
            default,
            identity,
            label_key: None,
            description_key: None,
            min: None,
            max: None,
            step: None,
            choices: None,
            group: None,
            order: None,
        }
    }

    fn def_no_schema() -> RendererDef {
        RendererDef {
            name: "no-schema".into(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["scene".into()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: None,
            extras: Vec::new(),
            settings: HashMap::new(),
        }
    }

    fn def_image() -> RendererDef {
        // Image renderer: schema-bearing via a non-empty `extras`
        // (`assets`) that no real renderer needs but exercises the
        // schema-active branch with no settings.
        RendererDef {
            name: "waywallen-image".into(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["image".into()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: Some(1),
            extras: vec!["assets".into()],
            settings: HashMap::new(),
        }
    }

    fn def_mpv() -> RendererDef {
        let mut ps = HashMap::new();
        ps.insert(
            "loop_file".to_string(),
            test_setting(SettingType::String, toml::Value::String("inf".into()), false),
        );
        ps.insert(
            "hwdec".to_string(),
            test_setting(SettingType::String, toml::Value::String("auto".into()), false),
        );
        RendererDef {
            name: "waywallen-mpv".into(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["video".into()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: Some(1),
            extras: Vec::new(),
            settings: ps,
        }
    }

    #[test]
    fn no_schema_falls_through_metadata() {
        let mut md = HashMap::new();
        md.insert("foo".to_string(), "bar".to_string());
        md.insert("baz".to_string(), "42".to_string());
        let v = validate_metadata(&def_no_schema(), &md).expect("no errors");
        assert_eq!(v.primary_value, "");
        assert!(v.extras.is_empty());
        // No schema → metadata flows through as settings for legacy
        // argv fall-through.
        assert_eq!(v.settings.get("foo").map(|s| s.as_str()), Some("bar"));
        assert_eq!(v.settings.get("baz").map(|s| s.as_str()), Some("42"));
    }

    #[test]
    fn missing_path_is_an_error_when_schema_active() {
        let md = HashMap::new();
        let errs = validate_metadata(&def_image(), &md).expect_err("must error");
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], ValidationError::MissingPath));
    }

    #[test]
    fn empty_path_value_is_an_error() {
        let mut md = HashMap::new();
        md.insert("path".to_string(), "".to_string());
        let errs = validate_metadata(&def_image(), &md).expect_err("must error");
        assert!(matches!(errs[0], ValidationError::MissingPath));
    }

    #[test]
    fn extras_pass_when_in_allowlist() {
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/scene.pkg".to_string());
        md.insert("assets".to_string(), "/tmp/assets".to_string());
        let v = validate_metadata(&def_image(), &md).expect("ok");
        assert_eq!(v.primary_value, "/tmp/scene.pkg");
        assert_eq!(v.extras.len(), 1);
        assert_eq!(v.extras.get("assets").map(|s| s.as_str()), Some("/tmp/assets"));
        assert!(v.warnings.is_empty(), "no warnings expected: {:?}", v.warnings);
    }

    #[test]
    fn unknown_extra_key_warns_but_does_not_error() {
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/scene.pkg".to_string());
        md.insert("totally_unknown".to_string(), "yo".to_string());
        let v = validate_metadata(&def_image(), &md).expect("ok");
        assert_eq!(v.primary_value, "/tmp/scene.pkg");
        assert!(
            v.warnings.iter().any(|w| w.contains("totally_unknown")),
            "expected warning for unknown key, got {:?}",
            v.warnings
        );
    }

    #[test]
    fn settings_missing_filled_from_default() {
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/clip.mp4".to_string());
        let v = validate_metadata(&def_mpv(), &md).expect("ok");
        assert_eq!(v.primary_value, "/tmp/clip.mp4");
        assert_eq!(
            v.settings.get("loop_file").map(|s| s.as_str()),
            Some("inf"),
        );
        assert_eq!(
            v.settings.get("hwdec").map(|s| s.as_str()),
            Some("auto"),
        );
        assert!(
            v.warnings.iter().any(|w| w.contains("loop_file")),
            "expected default-fill warn for loop_file"
        );
    }

    #[test]
    fn settings_supplied_value_wins_over_default() {
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/clip.mp4".to_string());
        md.insert("loop_file".to_string(), "no".to_string());
        let v = validate_metadata(&def_mpv(), &md).expect("ok");
        assert_eq!(
            v.settings.get("loop_file").map(|s| s.as_str()),
            Some("no"),
        );
    }

    #[test]
    fn settings_bad_type_errors() {
        let mut def = def_mpv();
        def.settings.insert(
            "ratio".to_string(),
            test_setting(SettingType::F32, toml::Value::Float(1.0), false),
        );
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/clip.mp4".to_string());
        md.insert("ratio".to_string(), "not-a-number".to_string());
        let errs = validate_metadata(&def, &md).expect_err("must error");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::BadSettingType { key, .. } if key == "ratio"
            )),
            "expected BadSettingType for ratio, got {errs:?}"
        );
    }

    #[test]
    fn u32_default_from_integer_works() {
        let mut def = def_mpv();
        def.settings.insert(
            "fps".to_string(),
            test_setting(SettingType::U32, toml::Value::Integer(30), true),
        );
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/clip.mp4".to_string());
        let v = validate_metadata(&def, &md).expect("ok");
        assert_eq!(v.settings.get("fps").map(|s| s.as_str()), Some("30"));
    }

    #[test]
    fn manifest_parses_with_extras_and_settings() {
        // End-to-end: TOML → RendererDef → validate_metadata.
        let src = r#"
            [renderer]
            name = "waywallen-mpv"
            bin = "/usr/bin/waywallen-mpv-renderer"
            types = ["video"]
            priority = 100
            spawn_version = 1
            extras = ["subtitle"]

            [renderer.settings]
            loop_file = { type = "string", default = "inf",  identity = false }
            hwdec     = { type = "string", default = "auto", identity = false }
        "#;
        let manifest: RendererManifest =
            toml::from_str(src).expect("manifest parses");
        assert_eq!(manifest.renderer.spawn_version, Some(1));
        assert_eq!(manifest.renderer.extras, vec!["subtitle".to_string()]);
        assert_eq!(manifest.renderer.settings.len(), 2);

        // Wire it through the validator.
        let mut md = HashMap::new();
        md.insert("path".to_string(), "/tmp/clip.mp4".to_string());
        md.insert("hwdec".to_string(), "vaapi".to_string());
        let v = validate_metadata(&manifest.renderer, &md).expect("ok");
        assert_eq!(v.primary_value, "/tmp/clip.mp4");
        assert_eq!(v.settings.get("hwdec").map(|s| s.as_str()), Some("vaapi"));
        assert_eq!(v.settings.get("loop_file").map(|s| s.as_str()), Some("inf"));
    }

    #[test]
    fn coerce_and_validate_u32_in_range() {
        let s = SettingDef {
            min: Some(toml::Value::Integer(0)),
            max: Some(toml::Value::Integer(100)),
            ..test_setting(SettingType::U32, toml::Value::Integer(50), false)
        };
        assert_eq!(coerce_and_validate("volume", "75", &s).unwrap(), "75");
        // boundaries inclusive
        assert_eq!(coerce_and_validate("volume", "0", &s).unwrap(), "0");
        assert_eq!(coerce_and_validate("volume", "100", &s).unwrap(), "100");
    }

    #[test]
    fn coerce_and_validate_u32_out_of_range_errors() {
        let s = SettingDef {
            min: Some(toml::Value::Integer(0)),
            max: Some(toml::Value::Integer(100)),
            ..test_setting(SettingType::U32, toml::Value::Integer(50), false)
        };
        let err = coerce_and_validate("volume", "500", &s).expect_err("must error");
        assert!(matches!(err, ValidationError::OutOfRange { ref key, .. } if key == "volume"));
    }

    #[test]
    fn coerce_and_validate_f32_bounds() {
        let s = SettingDef {
            min: Some(toml::Value::Float(0.0)),
            max: Some(toml::Value::Float(1.5)),
            ..test_setting(SettingType::F32, toml::Value::Float(1.0), false)
        };
        assert!(coerce_and_validate("ratio", "0.75", &s).is_ok());
        assert!(matches!(
            coerce_and_validate("ratio", "2.0", &s),
            Err(ValidationError::OutOfRange { .. })
        ));
        assert!(matches!(
            coerce_and_validate("ratio", "-0.1", &s),
            Err(ValidationError::OutOfRange { .. })
        ));
    }

    #[test]
    fn coerce_and_validate_choices_hit_and_miss() {
        let s = SettingDef {
            choices: Some(vec!["auto".into(), "vaapi".into(), "nvdec".into()]),
            ..test_setting(SettingType::String, toml::Value::String("auto".into()), false)
        };
        assert_eq!(coerce_and_validate("hwdec", "vaapi", &s).unwrap(), "vaapi");
        let err = coerce_and_validate("hwdec", "ssh", &s).expect_err("must error");
        assert!(matches!(
            err,
            ValidationError::BadChoice { ref key, .. } if key == "hwdec"
        ));
    }

    #[test]
    fn coerce_and_validate_bad_type_errors() {
        let s = test_setting(SettingType::U32, toml::Value::Integer(0), false);
        let err = coerce_and_validate("fps", "lots", &s).expect_err("must error");
        assert!(matches!(err, ValidationError::BadSettingType { .. }));
    }

    #[test]
    fn validate_metadata_rejects_out_of_range() {
        // Schema-bearing renderer with a bounded u32 setting — make
        // sure the spawn-time validator path (not just SettingsSet)
        // also enforces bounds.
        let mut def = def_mpv();
        def.settings.insert(
            "volume".into(),
            SettingDef {
                min: Some(toml::Value::Integer(0)),
                max: Some(toml::Value::Integer(100)),
                ..test_setting(SettingType::U32, toml::Value::Integer(50), false)
            },
        );
        let mut md = HashMap::new();
        md.insert("path".into(), "/tmp/clip.mp4".into());
        md.insert("volume".into(), "999".into());
        let errs = validate_metadata(&def, &md).expect_err("must error");
        assert!(errs.iter().any(|e| matches!(e, ValidationError::OutOfRange { .. })));
    }
}
