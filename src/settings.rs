//! Runtime-configurable settings store, persisted to
//! `$XDG_CONFIG_HOME/waywallen/config.toml`.
//!
//! Layout:
//!
//! ```toml
//! [global]
//! target_extent       = 1080
//! render_size_policy  = "one_axis_auto"   # native | one_axis_auto | one_axis_width | one_axis_height
//!
//! [plugin.wescene]
//! # Free-form per-plugin table: keys are owned by the plugin, not the
//! # daemon. M7 forwards these into the renderer subprocess via metadata.
//! ```
//!
//! Write strategy: every mutation goes through `update()`, which takes
//! the in-memory write lock, applies the closure, then pokes a
//! `Notify`. A background task debounces those pokes by
//! `DEBOUNCE_WRITE` and then atomically `rename`s a tempfile into
//! place. Callers never block on disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::RwLock as StdRwLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::display_layout::{Align, FillMode};

/// Quiet period after the last `update()` before the debounced writer
/// flushes to disk. Short enough that `Ctrl-C` shortly after a setting
/// change still persists if the user waits a beat; long enough that
/// rapid-fire UI toggles batch into a single write.
const DEBOUNCE_WRITE: Duration = Duration::from_secs(2);

/// Daemon-wide layout defaults applied to displays that have no
/// `[displays.<name>]` override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutDefaults {
    pub fillmode: FillMode,
    pub align: Align,
    /// sRGB straight alpha, 0..=1. Used as the letterbox color in fit
    /// modes when the texture doesn't fully cover the display.
    pub clear_rgba: [f32; 4],
}

impl Default for LayoutDefaults {
    fn default() -> Self {
        Self {
            fillmode: FillMode::default(),
            align: Align::default(),
            clear_rgba: [0.0, 0.0, 0.0, 1.0],
        }
    }
}

/// Per-display override. Each field is `Option`; `None` means "inherit
/// the global default". Keyed in `Settings::displays` by the display
/// name advertised in `register_display`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayPrefs {
    pub fillmode: Option<FillMode>,
    pub align: Option<Align>,
    pub clear_rgba: Option<[f32; 4]>,
}

impl DisplayPrefs {
    pub fn is_empty(&self) -> bool {
        self.fillmode.is_none() && self.align.is_none() && self.clear_rgba.is_none()
    }
}

/// Layout values resolved against (per-display override → global → built-in defaults).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLayout {
    pub fillmode: FillMode,
    pub align: Align,
    pub clear_rgba: [f32; 4],
}

/// How the daemon shapes the `(extent_w, extent_h, extent_mode)` it
/// hands to a renderer's `Init` request. The daemon does not know the
/// wallpaper's intrinsic resolution, so for anything other than
/// `Native` the renderer is responsible for filling the unspecified
/// axis (or both) using the helper in
/// `<waywallen-bridge/extent_resolve.h>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RenderSizePolicy {
    /// Send `(0, 0, AS_GIVEN)` — renderer uses its content's intrinsic
    /// size unchanged.
    Native,
    /// Send `(target_extent, target_extent, FIT_SHORTER)` — renderer
    /// scales its shorter native axis to `target_extent` and the
    /// longer axis proportionally.
    #[default]
    OneAxisAuto,
    /// Send `(target_extent, 0, AS_GIVEN)` — renderer fits to
    /// `target_extent` width and computes height.
    OneAxisWidth,
    /// Send `(0, target_extent, AS_GIVEN)` — renderer fits to
    /// `target_extent` height and computes width.
    OneAxisHeight,
}

/// `extent_mode` values matching `ww_extent_mode_t` in
/// `<waywallen-bridge/bridge.h>`. Kept in sync with the C enum by
/// hand — both sides serialize as a `u32` on the wire.
pub mod extent_mode {
    pub const AS_GIVEN: u32 = 0;
    pub const FIT_SHORTER: u32 = 1;
}

/// Translate a `RenderSizePolicy` + `target_extent` into the wire-level
/// `(extent_w, extent_h, extent_mode)` triple sent in `Init`.
pub fn resolve_extent(policy: RenderSizePolicy, target: u32) -> (u32, u32, u32) {
    match policy {
        RenderSizePolicy::Native => (0, 0, extent_mode::AS_GIVEN),
        RenderSizePolicy::OneAxisAuto => (target, target, extent_mode::FIT_SHORTER),
        RenderSizePolicy::OneAxisWidth => (target, 0, extent_mode::AS_GIVEN),
        RenderSizePolicy::OneAxisHeight => (0, target, extent_mode::AS_GIVEN),
    }
}

/// Daemon-wide defaults consumed by `WallpaperApply` when a renderer
/// has no per-plugin override.
///
/// Note: fps is intentionally NOT here. Frame rate is a per-plugin
/// concern (different renderer engines have different sane defaults
/// and capabilities), so it lives in `[plugin.<name>]` tables only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    /// Pixel value applied to whichever axis (or both) the policy
    /// designates. Ignored when `render_size_policy = Native`.
    pub target_extent: u32,
    /// How `target_extent` is mapped onto the (extent_w, extent_h,
    /// extent_mode) triple in `Init`. Default `OneAxisAuto` — daemon
    /// asks the renderer to fit its shorter native axis to the
    /// target, which is sensible for arbitrary aspect ratios without
    /// hard-coding a 16:9 assumption.
    pub render_size_policy: RenderSizePolicy,
    pub last_wallpaper: Option<String>,
    /// DB id of the playlist to activate on startup. `None` (default)
    /// = the All pseudo-playlist. Set/cleared via the playlist control
    /// surfaces; persisted so the daemon restarts in the same mode.
    pub active_playlist_id: Option<i64>,
    /// `"sequential"` / `"shuffle"` / `"random"`. Carries across
    /// restart for the All pseudo-playlist; for activated DB
    /// playlists the row's own `mode` column wins.
    pub playlist_mode: String,
    /// Auto-rotation interval in seconds; `0` = disabled. Restored on
    /// startup so the rotator picks the same cadence the user left
    /// the daemon in.
    pub rotation_secs: u32,
    /// Default fillmode/align/clear color applied when a display has
    /// no per-display override. Drives the daemon-side projection of
    /// `set_config` rects.
    pub layout: LayoutDefaults,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            target_extent: 1080,
            render_size_policy: RenderSizePolicy::default(),
            last_wallpaper: None,
            active_playlist_id: None,
            playlist_mode: "sequential".to_string(),
            rotation_secs: 0,
            layout: LayoutDefaults::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub global: GlobalSettings,
    /// Per-plugin string→string bag. Keyed by plugin name
    /// (`RendererDef.name`). String-only so the contents map cleanly
    /// onto `SpawnRequest.metadata` (which is also `String→String`)
    /// and the `SettingsGet/SetRequest` RPCs without per-value type
    /// gymnastics.
    #[serde(default, rename = "plugin")]
    pub plugins: HashMap<String, HashMap<String, String>>,
    /// Per-display layout overrides keyed by the display name
    /// advertised in `register_display`. Empty entries are pruned by
    /// `DisplayPrefs::is_empty`-aware writers; missing keys mean
    /// "inherit global defaults".
    #[serde(default, rename = "display")]
    pub displays: HashMap<String, DisplayPrefs>,
}

/// Resolve the on-disk location. Order:
///   1. `$XDG_CONFIG_HOME/waywallen/config.toml`
///   2. `$HOME/.config/waywallen/config.toml`
///   3. `./waywallen.toml` (last-resort fallback so tests can pass
///      `--config` without crossing a real home dir — phase 6 only
///      picks the former two).
pub fn default_config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("waywallen/config.toml");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/waywallen/config.toml");
    }
    PathBuf::from("waywallen.toml")
}

/// Resolve the SQLite database location. Mirrors [`default_config_path`]
/// but targets the XDG *data* dir instead of the config dir:
///   1. `$XDG_DATA_HOME/waywallen/waywallen.db`
///   2. `$HOME/.local/share/waywallen/waywallen.db`
///   3. `./waywallen.db` (last-resort fallback)
pub fn default_db_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("waywallen/waywallen.db");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/waywallen/waywallen.db");
    }
    PathBuf::from("waywallen.db")
}

pub struct SettingsStore {
    inner: Arc<StdRwLock<Settings>>,
    notify: Arc<Notify>,
    path: PathBuf,
    /// Serializes concurrent `flush()` calls. Without this the
    /// debounced writer_loop and `flush_now()` (called on shutdown)
    /// can both target the same `<name>.tmp` path simultaneously: one
    /// task's `O_TRUNC` clobbers the other's in-flight write, and
    /// whichever finishes the rename last installs whatever bytes its
    /// own tmp ended up with — typically a partial file because the
    /// other task interleaved a truncate. Held only across the
    /// in-memory snapshot + write + rename; the in-memory state
    /// itself is still guarded by `inner`.
    flush_lock: tokio::sync::Mutex<()>,
    /// Set when the in-memory state diverges from what's on disk.
    /// Cleared by a successful `flush()`. `update()` only marks
    /// dirty when the closure actually changed something
    /// (PartialEq on `Settings`), so duplicate apply / no-op tweaks
    /// don't trigger redundant writes.
    dirty: AtomicBool,
}

impl SettingsStore {
    /// Load from `path` if it exists, otherwise fall back to defaults
    /// AND seed the file with the default state so it's visible from
    /// day-one (handy for hand-editing without having to run the
    /// daemon first). Spawns the debounced-writer task on the current
    /// tokio runtime; callers should keep the returned `Arc` alive
    /// for the lifetime of the daemon or the writer exits.
    pub async fn load_or_default(path: PathBuf) -> Arc<Self> {
        let mut seed_on_disk = false;
        let initial = match tokio::fs::read_to_string(&path).await {
            Ok(s) => match toml::from_str::<Settings>(&s) {
                Ok(parsed) => {
                    log::info!("settings loaded from {}", path.display());
                    parsed
                }
                Err(e) => {
                    log::warn!(
                        "settings parse {}: {e}; continuing with defaults",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!(
                    "settings file {} not found, seeding defaults",
                    path.display()
                );
                seed_on_disk = true;
                Settings::default()
            }
            Err(e) => {
                log::warn!(
                    "settings file {} not readable ({e}); using defaults",
                    path.display()
                );
                Settings::default()
            }
        };

        let store = Arc::new(Self {
            inner: Arc::new(StdRwLock::new(initial)),
            notify: Arc::new(Notify::new()),
            path,
            flush_lock: tokio::sync::Mutex::new(()),
            // Mark dirty up-front when there's no on-disk file yet so
            // the seed flush below actually writes; otherwise dirty
            // starts clean and `update()` flips it on real changes.
            dirty: AtomicBool::new(seed_on_disk),
        });

        if seed_on_disk {
            store.flush().await;
        }

        // Debounced writer task.
        let writer = Arc::clone(&store);
        tokio::spawn(async move {
            writer.writer_loop().await;
        });

        store
    }

    /// Snapshot the current settings. Cheap: clones the inner struct
    /// under a read lock. Callers that only need a few fields should
    /// prefer `global()`/`plugin()` accessors instead.
    pub fn snapshot(&self) -> Settings {
        self.inner.read().expect("settings poisoned").clone()
    }

    /// Copy the `GlobalSettings` subset.
    pub fn global(&self) -> GlobalSettings {
        self.inner.read().expect("settings poisoned").global.clone()
    }

    /// Clone the value map for a single plugin, or `None` if the
    /// plugin has no recorded settings.
    pub fn plugin(&self, plugin_name: &str) -> Option<HashMap<String, String>> {
        self.inner
            .read()
            .expect("settings poisoned")
            .plugins
            .get(plugin_name)
            .cloned()
    }

    /// Resolve the effective layout for a display by name. Per-display
    /// overrides win field-by-field; missing fields fall back to the
    /// global `LayoutDefaults`. Hot path — called from
    /// `Router::sync_display` on every set_config emission.
    pub fn resolved_layout(&self, display_name: &str) -> ResolvedLayout {
        let g = self.inner.read().expect("settings poisoned");
        let defaults = &g.global.layout;
        let prefs = g.displays.get(display_name);
        ResolvedLayout {
            fillmode: prefs.and_then(|p| p.fillmode).unwrap_or(defaults.fillmode),
            align: prefs.and_then(|p| p.align).unwrap_or(defaults.align),
            clear_rgba: prefs.and_then(|p| p.clear_rgba).unwrap_or(defaults.clear_rgba),
        }
    }

    /// Snapshot just the per-display preferences (cloned). Used to
    /// expose the override map over the control plane (e.g.
    /// `DisplayInfo.layout_override` in protobuf).
    pub fn display_prefs(&self, display_name: &str) -> Option<DisplayPrefs> {
        self.inner
            .read()
            .expect("settings poisoned")
            .displays
            .get(display_name)
            .cloned()
    }

    /// Snapshot every registered display name in the prefs map.
    pub fn display_pref_names(&self) -> Vec<String> {
        self.inner
            .read()
            .expect("settings poisoned")
            .displays
            .keys()
            .cloned()
            .collect()
    }

    /// Apply an in-memory mutation. Compares the post-closure state
    /// against the pre-closure clone; only flips the dirty bit and
    /// pokes the writer if the closure actually changed something.
    /// No-op closures (or closures that set fields to their existing
    /// values) cost a clone + equality check but no disk I/O.
    pub fn update<F>(&self, f: F)
    where
        F: FnOnce(&mut Settings),
    {
        let changed = {
            let mut g = self.inner.write().expect("settings poisoned");
            let before = g.clone();
            f(&mut g);
            *g != before
        };
        if changed {
            self.dirty.store(true, Ordering::SeqCst);
            self.notify.notify_one();
        }
    }

    async fn writer_loop(self: Arc<Self>) {
        loop {
            // Block until something needs to be written.
            self.notify.notified().await;
            // Debounce: keep resetting the timer until DEBOUNCE_WRITE
            // elapses without another update.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(DEBOUNCE_WRITE) => break,
                    _ = self.notify.notified() => {}
                }
            }
            self.flush().await;
        }
    }

    /// Force a synchronous flush of the current settings to disk,
    /// bypassing the debounce window. Call this on shutdown so the
    /// last write doesn't get stranded by a SIGTERM that arrives
    /// inside the debounce period (otherwise `last_wallpaper`,
    /// `active_playlist_id`, and friends silently fail to persist).
    pub async fn flush_now(&self) {
        self.flush().await;
    }

    async fn flush(&self) {
        // Cheap fast path before grabbing the lock: if nothing has
        // changed since the last successful flush, skip entirely.
        if !self.dirty.load(Ordering::SeqCst) {
            return;
        }
        let _g = self.flush_lock.lock().await;
        // Re-check under the lock — another flush may have just
        // raced us to the same state.
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }

        let snapshot = self.snapshot();
        let serialized = match toml::to_string_pretty(&snapshot) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("settings serialize failed: {e}");
                self.dirty.store(true, Ordering::SeqCst);
                return;
            }
        };

        if let Some(parent) = self.path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                log::warn!(
                    "settings create_dir_all {}: {e}",
                    parent.display()
                );
                self.dirty.store(true, Ordering::SeqCst);
                return;
            }
        }

        let tmp = {
            let mut p = self.path.clone();
            let new_name = match p.file_name() {
                Some(n) => {
                    let mut s = n.to_os_string();
                    s.push(".tmp");
                    s
                }
                None => {
                    self.dirty.store(true, Ordering::SeqCst);
                    return;
                }
            };
            p.set_file_name(new_name);
            p
        };
        if let Err(e) = tokio::fs::write(&tmp, serialized).await {
            log::warn!("settings write {}: {e}", tmp.display());
            self.dirty.store(true, Ordering::SeqCst);
            return;
        }
        if let Err(e) = tokio::fs::rename(&tmp, &self.path).await {
            log::warn!(
                "settings rename {} → {}: {e}",
                tmp.display(),
                self.path.display()
            );
            self.dirty.store(true, Ordering::SeqCst);
            return;
        }
        log::debug!("settings flushed to {}", self.path.display());
    }

    /// Read-only view of the on-disk path (useful when the settings
    /// store is constructed before the rest of `AppState`, so callers
    /// can log the resolved path next to other startup diagnostics).
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Bring the in-memory plugin tables in line with the loaded
    /// renderer registry's manifest schemas:
    ///
    /// - declared keys missing from the user's toml are filled with
    ///   the manifest's `default`;
    /// - keys present in the toml but absent from the manifest are
    ///   dropped (with a warn) — they're stale leftovers from a
    ///   plugin removal/rename;
    /// - declared keys whose persisted value violates `min`/`max`/
    ///   `choices` get reset to default and warned about — the
    ///   daemon refuses to start serving an out-of-range value just
    ///   because it was on disk.
    ///
    /// Marks the store dirty when reconciliation altered anything so
    /// the cleaned-up table flushes to disk on the next debounce
    /// cycle. Returns `true` when a flush is needed (caller may also
    /// want to publish a `SettingsChanged` event so any already-
    /// connected WS client sees the merged truth).
    pub fn reconcile(
        &self,
        registry: &crate::plugin::renderer_registry::RendererRegistry,
    ) -> bool {
        use crate::plugin::renderer_registry::{
            check_setting_bounds, SettingDef, SettingType,
        };

        let mut changed = false;
        let mut g = self.inner.write().expect("settings poisoned");

        // Pre-compute manifest schemas keyed by plugin name so we can
        // also iterate the user table and warn on truly-unknown
        // plugins (versus a known plugin with an unknown key).
        let manifests: HashMap<String, &HashMap<String, SettingDef>> = registry
            .all_renderers()
            .into_iter()
            .map(|d| (d.name.clone(), &d.settings))
            .collect();

        // 1) Reconcile each known plugin's table.
        for (plugin_name, schema) in &manifests {
            if schema.is_empty() {
                continue;
            }
            let entry = g.plugins.entry(plugin_name.clone()).or_default();

            // Drop keys that aren't in the manifest anymore.
            let stale: Vec<String> = entry
                .keys()
                .filter(|k| !schema.contains_key(*k))
                .cloned()
                .collect();
            for k in stale {
                log::warn!(
                    "settings: dropping unknown key '{plugin_name}.{k}' \
                     (no longer in manifest schema)"
                );
                entry.remove(&k);
                changed = true;
            }

            // Fill in / reset bad values for declared keys.
            for (key, def) in schema.iter() {
                let needs_default = match entry.get(key) {
                    None => true,
                    Some(v) => match check_setting_bounds(key, v, def) {
                        Ok(()) => false,
                        Err(e) => {
                            log::warn!(
                                "settings: '{plugin_name}.{key}' = {v:?} \
                                 violates schema ({e}); resetting to default"
                            );
                            true
                        }
                    },
                };
                if needs_default {
                    let default = match def.ty {
                        SettingType::U32 => match &def.default {
                            toml::Value::Integer(i) if *i >= 0 => i.to_string(),
                            toml::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                        SettingType::F32 => match &def.default {
                            toml::Value::Float(f) => f.to_string(),
                            toml::Value::Integer(i) => (*i as f32).to_string(),
                            toml::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                        SettingType::Bool => match &def.default {
                            toml::Value::Boolean(b) => b.to_string(),
                            toml::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                        SettingType::String => match &def.default {
                            toml::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        },
                    };
                    if entry.get(key) != Some(&default) {
                        entry.insert(key.clone(), default);
                        changed = true;
                    }
                }
            }
        }

        // 2) Warn on whole plugins the user has settings for that the
        //    daemon doesn't know about. Keep them in memory — the
        //    plugin may come back on the next start (e.g. user just
        //    moved a manifest) — and they're harmless because nothing
        //    consumes them. Only the per-key drop above is destructive.
        for plugin_name in g.plugins.keys() {
            if !manifests.contains_key(plugin_name) {
                log::warn!(
                    "settings: plugin '{plugin_name}' has persisted values \
                     but no matching renderer manifest is loaded; \
                     leaving as-is"
                );
            }
        }

        if changed {
            self.dirty.store(true, Ordering::SeqCst);
            self.notify.notify_one();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrip() {
        let s: Settings = toml::from_str("").unwrap();
        assert_eq!(s.global.target_extent, 1080);
        assert_eq!(s.global.render_size_policy, RenderSizePolicy::OneAxisAuto);
        assert!(s.plugins.is_empty());
    }

    #[test]
    fn global_override_parses() {
        let src = "[global]\ntarget_extent = 1440\nrender_size_policy = \"one_axis_width\"\n";
        let s: Settings = toml::from_str(src).unwrap();
        assert_eq!(s.global.target_extent, 1440);
        assert_eq!(s.global.render_size_policy, RenderSizePolicy::OneAxisWidth);
    }

    #[test]
    fn resolve_extent_modes() {
        assert_eq!(
            resolve_extent(RenderSizePolicy::Native, 1080),
            (0, 0, extent_mode::AS_GIVEN)
        );
        assert_eq!(
            resolve_extent(RenderSizePolicy::OneAxisAuto, 1080),
            (1080, 1080, extent_mode::FIT_SHORTER)
        );
        assert_eq!(
            resolve_extent(RenderSizePolicy::OneAxisWidth, 1920),
            (1920, 0, extent_mode::AS_GIVEN)
        );
        assert_eq!(
            resolve_extent(RenderSizePolicy::OneAxisHeight, 1080),
            (0, 1080, extent_mode::AS_GIVEN)
        );
    }

    #[test]
    fn layout_defaults_roundtrip() {
        let src = r#"
[global.layout]
fillmode = "preserve_aspect_crop"
align = "top_right"
clear_rgba = [0.5, 0.0, 0.5, 1.0]
"#;
        let s: Settings = toml::from_str(src).unwrap();
        assert_eq!(s.global.layout.fillmode, FillMode::PreserveAspectCrop);
        assert_eq!(s.global.layout.align, Align::TopRight);
        assert_eq!(s.global.layout.clear_rgba, [0.5, 0.0, 0.5, 1.0]);
    }

    #[test]
    fn display_override_parses_and_resolves() {
        let src = r#"
[global.layout]
fillmode = "stretched"
align = "center"

[display.HDMI-A-1]
fillmode = "preserve_aspect_fit"
clear_rgba = [0.0, 0.0, 1.0, 1.0]
"#;
        let s: Settings = toml::from_str(src).unwrap();
        let prefs = s.displays.get("HDMI-A-1").unwrap();
        assert_eq!(prefs.fillmode, Some(FillMode::PreserveAspectFit));
        assert_eq!(prefs.align, None); // inherits
        assert_eq!(prefs.clear_rgba, Some([0.0, 0.0, 1.0, 1.0]));
    }

    #[tokio::test]
    async fn resolved_layout_falls_back_field_by_field() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let store = SettingsStore::load_or_default(path).await;

        // No per-display entry => pure global defaults.
        let r = store.resolved_layout("eDP-1");
        assert_eq!(r.fillmode, FillMode::default());
        assert_eq!(r.align, Align::default());

        // Set a partial override for "eDP-1" (only fillmode).
        store.update(|s| {
            s.global.layout.align = Align::Bottom;
            s.global.layout.clear_rgba = [0.1, 0.2, 0.3, 1.0];
            s.displays.insert(
                "eDP-1".into(),
                DisplayPrefs {
                    fillmode: Some(FillMode::PreserveAspectCrop),
                    align: None,
                    clear_rgba: None,
                },
            );
        });

        let r = store.resolved_layout("eDP-1");
        assert_eq!(r.fillmode, FillMode::PreserveAspectCrop); // override
        assert_eq!(r.align, Align::Bottom); // global
        assert_eq!(r.clear_rgba, [0.1, 0.2, 0.3, 1.0]); // global
    }

    #[test]
    fn plugin_section_preserved() {
        let src = r#"
[plugin.wescene]
foo = "bar"
baz = "7"
"#;
        let s: Settings = toml::from_str(src).unwrap();
        let wescene = s.plugins.get("wescene").expect("wescene section");
        assert_eq!(wescene.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(wescene.get("baz").map(String::as_str), Some("7"));
    }

    #[tokio::test]
    async fn debounced_write_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let store = SettingsStore::load_or_default(path.clone()).await;
        assert_eq!(store.global().target_extent, 1080);

        store.update(|s| s.global.target_extent = 1440);
        // Wait past the debounce window.
        tokio::time::sleep(DEBOUNCE_WRITE + Duration::from_millis(500)).await;

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        let parsed: Settings = toml::from_str(&written).unwrap();
        assert_eq!(parsed.global.target_extent, 1440);
    }

    // --- reconcile() tests --------------------------------------------

    use crate::plugin::renderer_registry::{
        RendererDef, RendererRegistry, SettingDef, SettingType,
    };
    use std::path::PathBuf;

    fn schema_setting(
        ty: SettingType,
        default: toml::Value,
        identity: bool,
    ) -> SettingDef {
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

    fn registry_with_video() -> RendererRegistry {
        let mut r = RendererRegistry::new();
        let mut s: HashMap<String, SettingDef> = HashMap::new();
        s.insert(
            "loop_file".into(),
            schema_setting(
                SettingType::String,
                toml::Value::String("inf".into()),
                false,
            ),
        );
        s.insert(
            "volume".into(),
            SettingDef {
                min: Some(toml::Value::Integer(0)),
                max: Some(toml::Value::Integer(100)),
                ..schema_setting(SettingType::U32, toml::Value::Integer(100), false)
            },
        );
        r.register(RendererDef {
            name: "waywallen-video".into(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["video".into()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: Some(1),
            extras: Vec::new(),
            settings: s,
        });
        r
    }

    fn make_store_with(plugins: HashMap<String, HashMap<String, String>>) -> Arc<SettingsStore> {
        Arc::new(SettingsStore {
            inner: Arc::new(StdRwLock::new(Settings {
                global: GlobalSettings::default(),
                plugins,
                displays: HashMap::new(),
            })),
            notify: Arc::new(Notify::new()),
            path: PathBuf::from("/dev/null"),
            flush_lock: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(false),
        })
    }

    #[test]
    fn reconcile_fills_missing_defaults() {
        let store = make_store_with(HashMap::new());
        let changed = store.reconcile(&registry_with_video());
        assert!(changed, "expected reconcile to fill defaults");
        let snap = store.snapshot();
        let video = snap.plugins.get("waywallen-video").expect("video table");
        assert_eq!(video.get("loop_file").map(String::as_str), Some("inf"));
        assert_eq!(video.get("volume").map(String::as_str), Some("100"));
    }

    #[test]
    fn reconcile_drops_unknown_keys() {
        let mut plugins = HashMap::new();
        let mut video = HashMap::new();
        video.insert("loop_file".into(), "inf".into());
        video.insert("volume".into(), "50".into());
        video.insert("ghost".into(), "should-disappear".into());
        plugins.insert("waywallen-video".into(), video);

        let store = make_store_with(plugins);
        let changed = store.reconcile(&registry_with_video());
        assert!(changed);
        let snap = store.snapshot();
        let video = snap.plugins.get("waywallen-video").unwrap();
        assert!(!video.contains_key("ghost"), "unknown key must be dropped");
        assert_eq!(video.get("volume").map(String::as_str), Some("50"));
    }

    #[test]
    fn reconcile_resets_out_of_range_to_default() {
        let mut plugins = HashMap::new();
        let mut video = HashMap::new();
        video.insert("loop_file".into(), "inf".into());
        video.insert("volume".into(), "999".into());
        plugins.insert("waywallen-video".into(), video);

        let store = make_store_with(plugins);
        let changed = store.reconcile(&registry_with_video());
        assert!(changed);
        let snap = store.snapshot();
        let video = snap.plugins.get("waywallen-video").unwrap();
        assert_eq!(video.get("volume").map(String::as_str), Some("100"));
    }

    #[test]
    fn reconcile_no_change_returns_false() {
        let mut plugins = HashMap::new();
        let mut video = HashMap::new();
        video.insert("loop_file".into(), "inf".into());
        video.insert("volume".into(), "100".into());
        plugins.insert("waywallen-video".into(), video);

        let store = make_store_with(plugins);
        let changed = store.reconcile(&registry_with_video());
        assert!(!changed, "all keys present and valid → no change");
    }

    #[test]
    fn reconcile_keeps_unknown_plugin_section() {
        // A plugin we don't know about should stay untouched (might
        // be a renamed/missing manifest the user'll re-add).
        let mut plugins = HashMap::new();
        let mut wescene = HashMap::new();
        wescene.insert("foo".into(), "bar".into());
        plugins.insert("waywallen-wescene".into(), wescene);

        let store = make_store_with(plugins);
        store.reconcile(&registry_with_video());
        let snap = store.snapshot();
        assert!(snap.plugins.contains_key("waywallen-wescene"));
        assert_eq!(
            snap.plugins
                .get("waywallen-wescene")
                .and_then(|m| m.get("foo"))
                .map(String::as_str),
            Some("bar")
        );
    }
}
