use anyhow::anyhow;

use crate::error::{Error, Result};
use mlua::prelude::*;
use sea_orm::DatabaseConnection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::probe::media::{AvFormatProbe, MediaProbe};
use crate::model::repo;
use crate::wallpaper_type::{WallpaperEntry, WallpaperType};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourcePluginInfo {
    pub name: String,
    pub types: Vec<WallpaperType>,
    pub version: String,
}

// ---------------------------------------------------------------------------
// SourceManager
// ---------------------------------------------------------------------------

pub struct SourceManager {
    lua: Lua,
    /// plugin name → registry key for the loaded module table.
    plugins: HashMap<String, LuaRegistryKey>,
    /// Flattened scan results from all plugins.
    entries: Vec<WallpaperEntry>,
    /// Index: wp_type → indices into `entries`.
    by_type: HashMap<WallpaperType, Vec<usize>>,
    /// Shared media probe exposed to Lua via ctx.probe(path).
    probe: Arc<dyn MediaProbe>,
    /// DB used by the `ctx.library_meta_*` async-Lua-function bridge.
    /// `None` means the bridge silently no-ops (used by tests that
    /// don't exercise persistence).
    db: Option<DatabaseConnection>,
}

// mlua with the `send` feature makes Lua: Send.
// We wrap SourceManager in Arc<TokioMutex<>> so this is required.
const _: () = {
    fn assert_send<T: Send>() {}
    fn check() {
        assert_send::<SourceManager>();
    }
};

impl SourceManager {
    pub fn new() -> Result<Self> {
        Self::with_probe(Arc::new(AvFormatProbe::new()))
    }

    pub fn with_probe(probe: Arc<dyn MediaProbe>) -> Result<Self> {
        let lua = Lua::new();
        Ok(Self {
            lua,
            plugins: HashMap::new(),
            entries: Vec::new(),
            by_type: HashMap::new(),
            probe,
            db: None,
        })
    }

    /// Hand the DB to the source manager so `ctx.library_meta_get/set`
    /// (registered as mlua async functions) can read/write the
    /// `library.metadata` JSON column. Without this, the metadata
    /// functions return nil / false.
    pub fn attach_db(&mut self, db: DatabaseConnection) {
        self.db = Some(db);
    }

    /// Load a single `.lua` source plugin. Returns the plugin name.
    pub fn load_plugin(&mut self, path: &Path) -> Result<String> {
        let source = std::fs::read_to_string(path)
            .map_err(|e| Error::Internal(anyhow!("read {}: {e}", path.display())))?;
        let module: LuaTable = self
            .lua
            .load(&source)
            .set_name(path.to_string_lossy())
            .eval()
            .map_err(|e| Error::Internal(anyhow!("eval {}: {e}", path.display())))?;

        // Call info() to get plugin metadata. All three steps are
        // load-time daemon-internal failures; collapse to Internal with
        // descriptive context.
        let info_fn: LuaFunction = module
            .get("info")
            .map_err(|e| Error::Internal(anyhow!("plugin must export info(): {e}")))?;
        let info_table: LuaTable = info_fn
            .call(())
            .map_err(|e| Error::Internal(anyhow!("info() failed: {e}")))?;
        let name: String = info_table
            .get("name")
            .map_err(|e| Error::Internal(anyhow!("info().name required: {e}")))?;

        let key = self.lua.create_registry_value(module)?;
        self.plugins.insert(name.clone(), key);
        log::info!("loaded source plugin: {name} from {}", path.display());
        Ok(name)
    }

    /// Scan a directory for `*.lua` plugin files and load all of them.
    pub fn load_all(&mut self, dir: &Path) -> Result<Vec<String>> {
        let pattern = dir.join("*.lua");
        let pattern_str = pattern
            .to_str()
            .ok_or_else(|| Error::Internal(anyhow!("source dir path not valid UTF-8")))?;
        let mut names = Vec::new();
        for entry in glob::glob(pattern_str).map_err(|e| Error::Internal(anyhow!("glob: {e}")))? {
            match entry {
                Ok(path) => match self.load_plugin(&path) {
                    Ok(name) => names.push(name),
                    Err(e) => log::warn!("skip {}: {e}", path.display()),
                },
                Err(e) => log::warn!("source plugin glob error: {e}"),
            }
        }
        Ok(names)
    }

    /// Run `scan(ctx)` on all loaded plugins and merge results.
    /// `libs_by_plugin` is the per-plugin library list pulled from the
    /// DB; each plugin sees its own slice via `ctx.libraries()`. A
    /// plugin missing from the map (or with an empty list) is scanned
    /// with no libraries — Lua plugins should emit zero entries in
    /// that case rather than fall back to defaults.
    ///
    /// Async because `ctx.library_meta_*` are mlua async functions
    /// that need a tokio runtime to drive their `sea-orm` calls; the
    /// caller drives this via either an enclosing async context or
    /// `Handle::block_on` from a `spawn_blocking` thread.
    pub async fn scan_all(&mut self, libs_by_plugin: &HashMap<String, Vec<String>>) -> Result<()> {
        self.entries.clear();
        self.by_type.clear();

        let plugin_names: Vec<String> = self.plugins.keys().cloned().collect();
        for name in &plugin_names {
            let libs = libs_by_plugin
                .get(name)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            if let Err(e) = self.scan_plugin(name, libs).await {
                log::warn!("scan plugin {name} failed: {e}");
            }
        }
        Ok(())
    }

    /// Run `scan(ctx)` on a single plugin by name with the supplied
    /// library list exposed as `ctx.libraries()`.
    async fn scan_plugin(&mut self, name: &str, libraries: &[String]) -> Result<()> {
        let key = self
            .plugins
            .get(name)
            .ok_or_else(|| Error::SourcePluginNotFound(name.to_string()))?;
        let module: LuaTable = self.lua.registry_value(key)?;
        let scan_fn: LuaFunction = module
            .get("scan")
            .map_err(|e| Error::Internal(anyhow!("plugin must export scan(ctx): {e}")))?;

        let ctx = self.build_ctx(Some(name), libraries)?;
        let results: LuaTable = scan_fn.call_async(ctx).await?;

        for pair in results.sequence_values::<LuaTable>() {
            let tbl = pair?;
            let entry = WallpaperEntry {
                id: tbl.get("id").unwrap_or_default(),
                name: tbl.get("name").unwrap_or_default(),
                wp_type: tbl.get("wp_type").unwrap_or_default(),
                resource: tbl.get("resource").unwrap_or_default(),
                preview: tbl.get::<String>("preview").ok(),
                metadata: parse_lua_string_map(&tbl, "metadata"),
                plugin_name: name.to_owned(),
                library_root: tbl.get("library_root").unwrap_or_default(),
                description: tbl.get::<String>("description").ok(),
                tags: tbl.get::<Vec<String>>("tags").unwrap_or_default(),
                external_id: tbl.get::<String>("external_id").ok(),
                // Optional plugin-supplied media meta. Plugins that
                // know their own metadata (e.g. wallpaper_engine via
                // project.json + ctx.file_size) can pre-fill these so
                // the daemon's background probe task has less work.
                // Missing fields stay `None` and are filled in later
                // by the probe scheduler.
                size: tbl.get::<i64>("size").ok(),
                width: tbl.get::<u32>("width").ok(),
                height: tbl.get::<u32>("height").ok(),
                format: tbl.get::<String>("format").ok(),
            };
            let idx = self.entries.len();
            self.by_type
                .entry(entry.wp_type.clone())
                .or_default()
                .push(idx);
            self.entries.push(entry);
        }
        Ok(())
    }

    /// Build the `ctx` table passed to Lua `scan(ctx)`. `libraries` is
    /// the per-plugin DB-driven library list exposed as
    /// `ctx.libraries()`. `plugin_name` is `Some` when called from
    /// `scan_plugin` (where plugin identity is known) and `None` from
    /// `auto_detect_all` (which runs before plugins are registered in
    /// the DB) — the latter disables the `library_meta_*` bridge
    /// because there is no plugin row to scope writes to.
    fn build_ctx(&self, plugin_name: Option<&str>, libraries: &[String]) -> Result<LuaTable> {
        let ctx = self.lua.create_table()?;

        // ctx.glob(pattern) -> list of file paths
        let glob_fn = self.lua.create_function(|lua, pattern: String| {
            let paths = lua.create_table()?;
            let mut i = 1;
            if let Ok(entries) = glob::glob(&pattern) {
                for entry in entries.flatten() {
                    if let Some(s) = entry.to_str() {
                        paths.set(i, s.to_string())?;
                        i += 1;
                    }
                }
            }
            Ok(paths)
        })?;
        ctx.set("glob", glob_fn)?;

        // ctx.list_dirs(path) -> list of subdirectory paths
        let list_dirs_fn = self.lua.create_function(|lua, path: String| {
            let dirs = lua.create_table()?;
            let mut i = 1;
            if let Ok(entries) = std::fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        if let Some(s) = entry.path().to_str() {
                            dirs.set(i, s.to_string())?;
                            i += 1;
                        }
                    }
                }
            }
            Ok(dirs)
        })?;
        ctx.set("list_dirs", list_dirs_fn)?;

        // ctx.file_exists(path) -> bool
        let file_exists_fn = self
            .lua
            .create_function(|_, path: String| Ok(std::path::Path::new(&path).exists()))?;
        ctx.set("file_exists", file_exists_fn)?;

        // ctx.read_file(path) -> string|nil (capped at 1MB)
        let read_file_fn =
            self.lua
                .create_function(|lua, path: String| match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() > 1_048_576 => Ok(mlua::Value::Nil),
                    Ok(_) => match std::fs::read_to_string(&path) {
                        Ok(s) => Ok(mlua::Value::String(lua.create_string(&s)?)),
                        Err(_) => Ok(mlua::Value::Nil),
                    },
                    Err(_) => Ok(mlua::Value::Nil),
                })?;
        ctx.set("read_file", read_file_fn)?;

        // ctx.extension(path) -> string|nil
        let extension_fn = self.lua.create_function(|_, path: String| {
            Ok(std::path::Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .map(String::from))
        })?;
        ctx.set("extension", extension_fn)?;

        // ctx.filename(path) -> string|nil
        let filename_fn = self.lua.create_function(|_, path: String| {
            Ok(std::path::Path::new(&path)
                .file_name()
                .and_then(|e| e.to_str())
                .map(String::from))
        })?;
        ctx.set("filename", filename_fn.clone())?;

        // ctx.basename(path) -> string|nil (same as filename on dirs)
        ctx.set("basename", filename_fn)?;

        // ctx.env(name) -> string|nil. Intentionally kept available for
        // auto-detect probing of well-known paths (e.g. $HOME). Not a
        // cache — resolves on every call.
        let env_fn = self
            .lua
            .create_function(|_, name: String| Ok(std::env::var(&name).ok()))?;
        ctx.set("env", env_fn)?;

        // ctx.libraries() -> list of absolute library paths registered
        // for this plugin in the daemon DB. Replaces the old
        // config/env-based directory discovery: libraries are now a
        // user-managed first-class concept owned by the daemon, and
        // Lua plugins only see what the DB authorizes for them.
        let libs_for_closure: Vec<String> = libraries.to_vec();
        let libraries_fn = self.lua.create_function(move |lua, ()| {
            let tbl = lua.create_table()?;
            for (i, lib) in libs_for_closure.iter().enumerate() {
                tbl.set(i + 1, lib.clone())?;
            }
            Ok(tbl)
        })?;
        ctx.set("libraries", libraries_fn)?;

        // ctx.json_parse(str) -> table|nil
        let json_parse_fn =
            self.lua.create_function(|lua, s: String| {
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(val) => json_to_lua(lua, &val),
                    Err(_) => Ok(mlua::Value::Nil),
                }
            })?;
        ctx.set("json_parse", json_parse_fn)?;

        // ctx.log(msg)
        let log_fn = self.lua.create_function(|_, msg: String| {
            log::info!("[lua] {msg}");
            Ok(())
        })?;
        ctx.set("log", log_fn)?;

        // ctx.file_size(path) -> integer|nil
        // Cheap stat-only helper: lets a Lua source plugin pre-fill
        // `entry.size` without paying for a libavformat probe.
        let file_size_fn = self.lua.create_function(|_, path: String| {
            let bytes = std::fs::metadata(&path)
                .ok()
                .and_then(|m| i64::try_from(m.len()).ok());
            Ok(bytes)
        })?;
        ctx.set("file_size", file_size_fn)?;

        // ctx.probe(path) -> table|nil
        // Returns a table with present file/media fields, or nil if all
        // fields are None. Composes the cheap stat tier (size) and the
        // libavformat tier (width/height/format) into a single table so
        // Lua plugins keep the historical schema.
        let probe_arc = Arc::clone(&self.probe);
        let probe_fn = self.lua.create_function(move |lua, path: String| {
            let s = crate::probe::stat::stat_file(&path);
            let m = probe_arc.probe_media(&path);
            if s.is_none() && m.width.is_none() && m.height.is_none() && m.format.is_none() {
                return Ok(mlua::Value::Nil);
            }
            let tbl = lua.create_table()?;
            if let Some(s) = s {
                tbl.set("size", s.size)?;
            }
            if let Some(v) = m.width {
                tbl.set("width", v)?;
            }
            if let Some(v) = m.height {
                tbl.set("height", v)?;
            }
            if let Some(v) = m.format {
                tbl.set("format", v)?;
            }
            Ok(mlua::Value::Table(tbl))
        })?;
        ctx.set("probe", probe_fn)?;

        // ctx.library_meta_get(library_path, key) -> string|nil
        // ctx.library_meta_set(library_path, key, value_or_nil) -> bool
        //
        // Per-library KV scratch space backed by `library.metadata`
        // (JSON column). Scoped to the *current* plugin: a plugin can
        // only read/write metadata on libraries it owns. The
        // (plugin_name, library_path) tuple resolves to a library row
        // via the existing `idx_library_plugin_path` unique index, so
        // both lookups are cheap.
        //
        // Implemented as mlua async functions: when invoked, the Lua
        // coroutine yields and the surrounding `scan_fn.call_async` /
        // `extras_fn.call_async` driver awaits the sea-orm future on
        // whatever runtime is driving the call. Returns nil / false if
        // the DB hasn't been attached or the (plugin, library) tuple
        // doesn't exist yet — set-before-scan is a no-op rather than an
        // error so plugins can be defensive without crashing the scan.
        {
            let kv_db = self.db.clone();
            let kv_plugin = plugin_name.map(str::to_owned);

            let getter_db = kv_db.clone();
            let getter_plugin = kv_plugin.clone();
            let library_meta_get_fn =
                self.lua
                    .create_async_function(move |lua, (lib_path, key): (String, String)| {
                        let db = getter_db.clone();
                        let plugin_name = getter_plugin.clone();
                        async move {
                            let (Some(db), Some(plugin_name)) = (db, plugin_name) else {
                                return Ok(mlua::Value::Nil);
                            };
                            let res: crate::error::Result<Option<String>> = async {
                                let Some(plugin) =
                                    repo::find_plugin_by_name(&db, &plugin_name).await?
                                else {
                                    return Ok(None);
                                };
                                let Some(lib) =
                                    repo::find_library(&db, plugin.id, &lib_path).await?
                                else {
                                    return Ok(None);
                                };
                                repo::get_library_metadata_value(&db, lib.id, &key).await
                            }
                            .await;
                            match res {
                                Ok(Some(v)) => Ok(mlua::Value::String(lua.create_string(&v)?)),
                                Ok(None) => Ok(mlua::Value::Nil),
                                Err(e) => {
                                    log::warn!("library_meta_get: {e:#}");
                                    Ok(mlua::Value::Nil)
                                }
                            }
                        }
                    })?;
            ctx.set("library_meta_get", library_meta_get_fn)?;

            let setter_db = kv_db;
            let setter_plugin = kv_plugin;
            let library_meta_set_fn = self.lua.create_async_function(
                move |_, (lib_path, key, value): (String, String, Option<String>)| {
                    let db = setter_db.clone();
                    let plugin_name = setter_plugin.clone();
                    async move {
                        let (Some(db), Some(plugin_name)) = (db, plugin_name) else {
                            return Ok(false);
                        };
                        let res: crate::error::Result<bool> = async {
                            let Some(plugin) = repo::find_plugin_by_name(&db, &plugin_name).await?
                            else {
                                return Ok(false);
                            };
                            let Some(lib) = repo::find_library(&db, plugin.id, &lib_path).await?
                            else {
                                return Ok(false);
                            };
                            repo::set_library_metadata_value(&db, lib.id, &key, value.as_deref())
                                .await?;
                            Ok(true)
                        }
                        .await;
                        match res {
                            Ok(b) => Ok(b),
                            Err(e) => {
                                log::warn!("library_meta_set: {e:#}");
                                Ok(false)
                            }
                        }
                    }
                },
            )?;
            ctx.set("library_meta_set", library_meta_set_fn)?;
        }

        // Source plugins write `entry.metadata` directly using the
        // canonical schema:
        //   metadata = { path = resource, [extras...] }
        // The daemon validates the resulting table against the
        // resolved renderer manifest in
        // `renderer_registry::validate_metadata`.

        Ok(ctx)
    }

    // -----------------------------------------------------------------------
    // Query API
    // -----------------------------------------------------------------------

    pub fn list(&self) -> &[WallpaperEntry] {
        &self.entries
    }

    pub fn list_by_type(&self, wp_type: &str) -> Vec<&WallpaperEntry> {
        self.by_type
            .get(wp_type)
            .map(|indices| indices.iter().map(|&i| &self.entries[i]).collect())
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<&WallpaperEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Ask the plugin that produced `entry` for the CLI `extras`
    /// dictionary the daemon should pass to the renderer subprocess
    /// at spawn time (under SPAWN_VERSION 3 these become `--<key>
    /// <value>` argv after `--ipc <socket>`).
    ///
    /// The Lua plugin exports `extras(entry, ctx) -> table` returning
    /// a flat `{string -> string}` map. `ctx` carries the same helpers
    /// scan(ctx) sees — including `library_meta_get` — so plugins can
    /// pull values they cached at scan time out of `library.metadata`
    /// instead of duplicating them on every entry. Plugins that
    /// haven't migrated fall through to `entry.metadata`.
    ///
    /// `extras["path"]` is mandatory in the result — that's the
    /// canonical resource path. The daemon does NOT enforce that here;
    /// renderers fail at spawn-time with `--path <file> is required`
    /// if the plugin omitted it.
    pub async fn call_extras(
        &self,
        plugin_name: &str,
        entry: &WallpaperEntry,
    ) -> Result<HashMap<String, String>> {
        let key = self
            .plugins
            .get(plugin_name)
            .ok_or_else(|| Error::SourcePluginNotFound(plugin_name.to_string()))?;
        // Body runs in a sub-block so any mlua failure rolls up into a
        // single typed `SourceExtrasFailed` carrying the plugin name —
        // callers (ws_server, control) just need the typed surface and
        // the underlying Lua trace as a string.
        let body = async {
            let module: LuaTable = self.lua.registry_value(key)?;
            let extras_fn: Option<LuaFunction> = module.get("extras").ok();
            let Some(extras_fn) = extras_fn else {
                // Legacy path: plugin hasn't migrated to extras(entry, ctx)
                // yet. Fall back to entry.metadata.
                log::debug!(
                    "source plugin '{plugin_name}' has no extras() function; \
                     using legacy entry.metadata as CLI extras"
                );
                return Ok::<_, mlua::Error>(entry.metadata.clone());
            };
            let entry_tbl = self.lua.create_table()?;
            entry_tbl.set("id", entry.id.clone())?;
            entry_tbl.set("name", entry.name.clone())?;
            entry_tbl.set("wp_type", entry.wp_type.clone())?;
            entry_tbl.set("resource", entry.resource.clone())?;
            if let Some(p) = &entry.preview {
                entry_tbl.set("preview", p.clone())?;
            }
            // Forward the (still-existing) metadata table so plugins that
            // wrote secondary keys at scan-time can read them back here.
            let md_tbl = self.lua.create_table()?;
            for (k, v) in &entry.metadata {
                md_tbl.set(k.clone(), v.clone())?;
            }
            entry_tbl.set("metadata", md_tbl)?;
            if let Some(d) = &entry.description {
                entry_tbl.set("description", d.clone())?;
            }
            // `library_root` and `external_id` are the two "where did this
            // come from" anchors plugins need at extras-time: the former
            // to look up library-scoped metadata, the latter as a
            // first-class id (e.g. wallpaper_engine workshop_id) without
            // re-parsing the resource path.
            if !entry.library_root.is_empty() {
                entry_tbl.set("library_root", entry.library_root.clone())?;
            }
            if let Some(eid) = &entry.external_id {
                entry_tbl.set("external_id", eid.clone())?;
            }
            // Build the same ctx scan(ctx) sees, so extras can call
            // `library_meta_get` etc. Empty libraries list — extras runs
            // per-entry, not per-library, and shouldn't need to enumerate.
            let ctx = self
                .build_ctx(Some(plugin_name), &[])
                .map_err(mlua::Error::external)?;
            let result: LuaTable = extras_fn.call_async((entry_tbl, ctx)).await?;
            let mut out = HashMap::new();
            for pair in result.pairs::<String, String>() {
                let (k, v) = pair?;
                out.insert(k, v);
            }
            Ok(out)
        };
        body.await
            .map_err(|e: mlua::Error| Error::SourceExtrasFailed {
                plugin: plugin_name.to_string(),
                message: e.to_string(),
            })
    }

    /// Ask every plugin that exports `auto_detect(ctx)` to probe
    /// well-known filesystem locations and report any that exist.
    /// Returns `(plugin_name -> [paths])`. Plugins without an
    /// `auto_detect` export are silently skipped. Each plugin's ctx
    /// sees an empty `libraries()` because auto-detect runs *before*
    /// any libraries are registered.
    pub async fn auto_detect_all(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let empty: [String; 0] = [];
        for (name, key) in &self.plugins {
            let module: LuaTable = self.lua.registry_value(key)?;
            let auto_fn: LuaFunction = match module.get("auto_detect") {
                Ok(f) => f,
                Err(_) => continue,
            };
            let ctx = self.build_ctx(None, &empty)?;
            let results: LuaTable = match auto_fn.call_async(ctx).await {
                Ok(t) => t,
                Err(e) => {
                    log::warn!("auto_detect plugin {name}: {e}");
                    continue;
                }
            };
            let paths: Vec<String> = results
                .sequence_values::<String>()
                .filter_map(|v| v.ok())
                .collect();
            if !paths.is_empty() {
                out.insert(name.clone(), paths);
            }
        }
        Ok(out)
    }

    pub fn plugins(&self) -> Result<Vec<SourcePluginInfo>> {
        let mut out = Vec::new();
        for (name, key) in &self.plugins {
            let module: LuaTable = self.lua.registry_value(key)?;
            let info_fn: LuaFunction = module.get("info")?;
            let info: LuaTable = info_fn.call(())?;
            let types: Vec<String> = info
                .get::<LuaTable>("types")
                .map(|t| {
                    t.sequence_values::<String>()
                        .filter_map(|v| v.ok())
                        .collect()
                })
                .unwrap_or_default();
            let version: String = info.get("version").unwrap_or_else(|_| "0.0.0".into());
            out.push(SourcePluginInfo {
                name: name.clone(),
                types,
                version,
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_lua_string_map(tbl: &LuaTable, key: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(meta) = tbl.get::<LuaTable>(key) {
        for pair in meta.pairs::<String, String>() {
            if let Ok((k, v)) = pair {
                map.insert(k, v);
            }
        }
    }
    map
}

fn json_to_lua(lua: &Lua, val: &serde_json::Value) -> LuaResult<LuaValue> {
    match val {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else {
                Ok(LuaValue::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(t))
        }
        serde_json::Value::Object(obj) => {
            let t = lua.create_table()?;
            for (k, v) in obj {
                t.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(t))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::media::{MediaMeta, MediaProbe};
    use std::io::Write;

    struct FakeProbe {
        meta: MediaMeta,
    }
    impl MediaProbe for FakeProbe {
        fn probe_media(&self, _path: &str) -> MediaMeta {
            self.meta.clone()
        }
    }

    /// Drive an async scan from a sync `#[test]` — these tests don't
    /// touch the DB so a single-thread runtime is fine.
    fn block(fut: impl std::future::Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    fn block_value<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }

    #[test]
    fn ctx_probe_callable_from_lua() {
        let probe = Arc::new(FakeProbe {
            meta: MediaMeta {
                width: Some(1920),
                height: Some(1080),
                format: Some("matroska,webm".to_owned()),
            },
        });
        let dir = tempfile::tempdir().unwrap();
        let plugin_path = dir.path().join("probe_test.lua");
        let mut f = std::fs::File::create(&plugin_path).unwrap();
        write!(
            f,
            r#"
local M = {{}}
function M.info()
    return {{ name = "probe_test", types = {{"video"}}, version = "1.0" }}
end
function M.scan(ctx)
    local m = ctx.probe("/fake/path/video.mp4")
    if m == nil then error("probe returned nil") end
    return {{
        {{
            id = "v1",
            name = "Video",
            wp_type = "video",
            resource = "/lib/v1.mp4",
            library_root = "/lib",
            metadata = {{}},
            _probe_size = m.size,
            _probe_width = m.width,
            _probe_height = m.height,
            _probe_format = m.format,
        }},
    }}
end
return M
"#
        )
        .unwrap();

        let mut mgr = SourceManager::with_probe(probe as Arc<dyn MediaProbe>).unwrap();
        mgr.load_plugin(&plugin_path).unwrap();
        block(async { mgr.scan_all(&HashMap::new()).await.unwrap() });

        let entries = mgr.list();
        assert_eq!(entries.len(), 1);
        // The Lua plugin called ctx.probe successfully (it would error() otherwise).
        // Verify the entry was emitted correctly.
        assert_eq!(entries[0].id, "v1");
    }

    #[test]
    fn test_load_and_scan_plugin() {
        let dir = tempfile::tempdir().unwrap();

        // Write a minimal source plugin
        let plugin_path = dir.path().join("test_source.lua");
        let mut f = std::fs::File::create(&plugin_path).unwrap();
        write!(
            f,
            r#"
local M = {{}}
function M.info()
    return {{ name = "test", types = {{"image"}}, version = "1.0" }}
end
function M.scan(ctx)
    return {{
        {{ id = "w1", name = "Test Wallpaper", wp_type = "image",
           resource = "/tmp/test.png", metadata = {{}} }},
    }}
end
return M
"#
        )
        .unwrap();

        let mut mgr = SourceManager::new().unwrap();
        let name = mgr.load_plugin(&plugin_path).unwrap();
        assert_eq!(name, "test");

        block(async { mgr.scan_all(&HashMap::new()).await.unwrap() });
        assert_eq!(mgr.list().len(), 1);
        assert_eq!(mgr.list()[0].id, "w1");
        assert_eq!(mgr.list()[0].wp_type, "image");
        assert_eq!(mgr.list()[0].plugin_name, "test");

        let by_type = mgr.list_by_type("image");
        assert_eq!(by_type.len(), 1);

        let by_type_empty = mgr.list_by_type("video");
        assert!(by_type_empty.is_empty());

        let found = mgr.get("w1");
        assert!(found.is_some());

        let plugins = mgr.plugins().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "test");
    }

    #[test]
    fn video_source_plugin_discovers_video_files() {
        let lib = tempfile::tempdir().unwrap();
        let nested = lib.path().join("album");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(lib.path().join("clip.MP4"), b"video bytes").unwrap();
        std::fs::write(lib.path().join("animated.gif"), b"image source owns gif").unwrap();
        std::fs::write(nested.join("poster.png"), b"not a video").unwrap();
        std::fs::write(nested.join("loop.webm"), b"more video bytes").unwrap();

        let plugin_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("plugins/video/sources/video.lua");

        let mut mgr = SourceManager::new().unwrap();
        let name = mgr.load_plugin(&plugin_path).unwrap();
        assert_eq!(name, "video");

        let mut libs = HashMap::new();
        libs.insert(
            "video".to_string(),
            vec![lib.path().to_string_lossy().to_string()],
        );
        block(async { mgr.scan_all(&libs).await.unwrap() });

        let entries = mgr.list();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.wp_type == "video"));
        assert!(entries.iter().all(|e| e.plugin_name == "video"));
        assert!(entries.iter().all(|e| e.preview.is_none()));
        assert!(entries.iter().all(|e| e.size.is_some()));
        assert!(entries.iter().all(|e| e.width.is_none()));
        assert!(entries.iter().all(|e| e.height.is_none()));
        assert!(entries.iter().all(|e| e.format.is_none()));
        // SPAWN_VERSION 3: plugins emit empty `metadata`; the canonical
        // resource path lives in `entry.resource` and is surfaced to
        // the renderer via the plugin's `extras(entry)` Lua callback.
        assert!(entries.iter().all(|e| e.metadata.is_empty()));

        let clip_path = lib.path().join("clip.MP4").to_string_lossy().to_string();
        let clip = mgr.get(&format!("video:{clip_path}")).unwrap().clone();
        assert_eq!(clip.name, "clip");
        assert_eq!(clip.resource, clip_path);

        let extras = block_value(async { mgr.call_extras("video", &clip).await.unwrap() });
        assert_eq!(extras.get("path"), Some(&clip.resource));

        assert_eq!(mgr.list_by_type("video").len(), 2);
        assert!(mgr.list_by_type("image").is_empty());
    }
}
