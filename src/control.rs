//! Shared wallpaper control logic.
//!
//! The same operations (apply, next, previous, pause, resume, rescan) are
//! driven from two surfaces — the WebSocket control plane (`ws_server`)
//! and the session-bus `Daemon1` interface (`dbus_iface`) plus the tray.
//! This module owns the canonical implementation so both paths converge
//! on identical semantics (spawn-before-kill, router relink, playlist
//! cursor tracking).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;

use crate::error::{Error, Result};
use crate::ipc::proto::ControlMsg;
use crate::model::{repo, sync};
use crate::queue::rotator::RotationConfig;
use crate::queue::Mode;
use crate::renderer_manager;
use crate::wallpaper_type::WallpaperEntry;
use crate::AppState;

/// Re-export so callers that already wrote `control::QueueState`
/// don't have to chase the move into the `playlist` module.
pub use crate::queue::QueueState;

pub struct ApplyResult {
    pub renderer_id: String,
    pub entry: WallpaperEntry,
}

/// Apply a wallpaper by id, with single-flight semantics across the
/// daemon: only one apply is in flight at a time. A subsequent call
/// supersedes any in-flight prior call (the prior caller observes
/// `apply task superseded or cancelled` and the prior renderer-spawn
/// in progress is dropped, which kills its child via `kill_on_drop`).
///
/// This sits on top of `crate::tasks::TaskManager::spawn_async_unique`
/// using the fixed key `apply/global` — Iter 3 only serializes globally;
/// per-display keys land when displays can be assigned distinct
/// wallpapers.
pub async fn apply_wallpaper_by_id(app: &Arc<AppState>, id: &str) -> Result<ApplyResult> {
    let app_clone = app.clone();
    let id_owned = id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<Result<ApplyResult>>();
    app.tasks.spawn_async_unique(
        crate::tasks::TaskKind::Apply,
        "apply/global",
        format!("apply/{id_owned}"),
        async move {
            let res = apply_wallpaper_inner(&app_clone, &id_owned).await;
            // If the receiver is gone the caller already moved on (or
            // was itself cancelled); silently drop the result.
            let _ = tx.send(res);
            Ok(())
        },
    );
    rx.await
        .map_err(|_| Error::Internal(anyhow!("apply task superseded or cancelled")))?
}

/// The actual apply work — spawn renderer, relink displays, kill old
/// renderers, update playlist. Caller is the unique apply task.
async fn apply_wallpaper_inner(app: &Arc<AppState>, id: &str) -> Result<ApplyResult> {
    // Render-target hint comes from settings.global; same path as the
    // WS apply RPC. Hardcoding (0, 0, AS_GIVEN) here would let the
    // renderer subprocess fall back to its built-in default and break
    // the documented 1080p minimum.
    let (width, height, extent_mode) = {
        let g = app.settings.global();
        crate::settings::resolve_extent(g.render_size_policy, g.target_extent)
    };
    let entry = {
        let snap = app.source_snapshot.read().await;
        snap.get(id).cloned()
    };
    let entry = entry.ok_or_else(|| Error::WallpaperNotFound(id.to_string()))?;

    let renderer_plugin_name = app
        .renderer_manager
        .registry()
        .resolve(&entry.wp_type)
        .map(|def| def.name.clone())
        .ok_or_else(|| Error::NoRendererForType(entry.wp_type.clone()))?;

    // The D-Bus + rotator entry point always relinks every display
    // to the new renderer (relink_all_displays_to below). That means
    // every pre-existing renderer ends up with zero enabled links. We
    // stop them *before* spawning the new one so peak VRAM stays at
    // one renderer's working set instead of overlapping two.
    //
    // Ordering matters for cross-process Vulkan correctness:
    //   1. Arm unbind-ack tracking on the doomed renderers.
    //   2. unregister_renderer → emits `Unbind` to bound displays
    //      (recorded as pending acks under the renderer).
    //   3. Wait for the displays to send `unbind_done` (with timeout).
    //      This gives the consumer time to start its render-thread
    //      drain before the producer's GPU device goes away.
    //   4. kill the renderer (now-fixed graceful Shutdown path:
    //      sends Shutdown, waits 5s, escalates to SIGKILL only on
    //      timeout). The producer's clean exit drains its device
    //      (via the bridge's wait_idle vfunc), so its acquire
    //      dma_fence signals normally instead of being kernel-cancelled.
    let to_stop = app.router.renderers_fully_replaced_by(None).await;
    if !to_stop.is_empty() {
        // 1 s ack timeout: the consumer's unbind_done is sent
        // synchronously from handle_unbind after the textures_releasing
        // callback fires; that callback queues handles to a pending
        // list and posts an update(). Real-world latency is socket
        // RTT plus one event-loop tick — well under 1 s.
        app.router
            .stop_renderers_orderly(&to_stop, Duration::from_secs(1))
            .await;
    }

    // `width`/`height` of `0` are legal — they tell the renderer to
    // derive that axis from the wallpaper's intrinsic size.
    // SPAWN_VERSION 3: source plugin's `extras(entry)` Lua callback
    // returns the CLI argv dict. Lua failures used to fall back to
    // `entry.metadata` silently; now they surface as
    // `SourceExtrasFailed` so the dbus / rotator caller learns the
    // real problem instead of getting a confusing "wrong settings"
    // follow-up — same policy as the WS apply path.
    let extras = app
        .source_manager
        .lock()
        .await
        .call_extras(&entry.plugin_name, &entry)
        .await?;
    // Init.settings is the reconciled per-plugin section of the
    // settings store; defaults and bound-checks are already enforced
    // there (`Settings::reconcile` on startup, `coerce_and_validate`
    // on `SettingsSet`). The D-Bus / scheduler / rotator entry points
    // don't take per-call setting overrides, so this is the canonical
    // source.
    let spawn_settings = app
        .settings
        .plugin(&renderer_plugin_name)
        .unwrap_or_default();
    let spawn_req = renderer_manager::SpawnRequest {
        wp_type: entry.wp_type.clone(),
        extras,
        settings: spawn_settings,
        width,
        height,
        extent_mode,
        test_pattern: false,
        renderer_name: None,
    };
    // renderer_manager still returns anyhow today (Phase 3 will give
    // it typed Error). The blanket `From<anyhow::Error>` lands this in
    // `Error::Internal`; once Phase 3 ships, the typed
    // `RendererSpawnFailed` will flow through automatically.
    let renderer_id = app
        .renderer_manager
        .spawn(spawn_req)
        .await
        .map_err(|e| Error::RendererSpawnFailed(e.to_string()))?;
    if let Some(handle) = app.renderer_manager.get(&renderer_id).await {
        app.router.register_renderer(handle).await;
    }
    app.router.relink_all_displays_to(&renderer_id).await;

    {
        let mut q = app.queue.lock().await;
        q.current = Some(entry.id.clone());
        // Stash the DB id so sequential / random stepping has an anchor.
        // Best-effort: lookup may fail if sync hasn't picked the entry up.
        if !entry.library_root.is_empty() {
            if let Some(rel) =
                crate::queue::relative_under_root(&entry.library_root, &entry.resource)
            {
                if let Ok(Some(it)) =
                    repo::find_item_by_library_path(&app.db, &entry.library_root, &rel).await
                {
                    q.last_db_id = Some(it.id);
                }
            }
        }
    }

    app.settings.update(|s| {
        s.global.last_wallpaper = Some(entry.id.clone());
    });
    // Push the just-applied wallpaper to disk synchronously instead of
    // waiting on the 2s debounce. A kill / SIGTERM inside the debounce
    // window would otherwise lose `last_wallpaper`, which is exactly
    // the value the next start needs to reproduce playback. flush_now
    // is a cheap no-op when nothing actually changed.
    app.settings.flush_now().await;
    crate::dbus_iface::notify_current_wallpaper_id_changed(app).await;

    Ok(ApplyResult { renderer_id, entry })
}

/// Advance the queue cursor by `delta` and apply the result.
///
/// Sequential / Random go straight to the DB via the active filter
/// (`settings.global.wallpaper_filter`). Shuffle materializes a round
/// of matching DB ids on first entry / wrap, then walks it in memory.
/// The rotator (rotation tick) calls this with `delta = 1`.
pub async fn step(app: &Arc<AppState>, delta: i32) -> Result<String> {
    use crate::model::repo::{StepDirection, QueueRow};
    use crate::queue::Mode;

    let (filters, logics) = app.settings.global().wallpaper_filter.to_pb();
    let mode = app.queue.lock().await.mode;

    let row: QueueRow = match mode {
        Mode::Sequential => {
            let after = app.queue.lock().await.last_db_id;
            let dir = if delta >= 0 {
                StepDirection::Forward
            } else {
                StepDirection::Backward
            };
            repo::next_item_by_filter(&app.db, &filters, &logics, after, dir)
                .await?
                .ok_or_else(|| Error::FailedPrecondition("queue is empty".into()))?
        }
        Mode::Random => {
            let exclude = app.queue.lock().await.last_db_id;
            repo::random_item_by_filter(&app.db, &filters, &logics, exclude)
                .await?
                .ok_or_else(|| Error::FailedPrecondition("queue is empty".into()))?
        }
        Mode::Shuffle => step_shuffle(app, &filters, &logics, delta).await?,
    };

    let entry_id = bridge_to_entry_id(app, &row).await?;
    apply_wallpaper_by_id(app, &entry_id).await?;
    // Reset the rotator deadline so the user gets the full quiet
    // window after a manual advance instead of being walked over by
    // the next auto tick.
    app.rotation.kick();
    Ok(entry_id)
}

/// Bridge a DB row to a snapshot entry id (the `WallpaperApply`
/// argument). Returns `Error::WallpaperNotFound` if the snapshot
/// hasn't picked the row up yet (sync just ran but scan hasn't).
async fn bridge_to_entry_id(app: &Arc<AppState>, row: &repo::QueueRow) -> Result<String> {
    let snap = app.source_snapshot.read().await;
    for entry in snap.list() {
        if entry.library_root.is_empty() {
            continue;
        }
        let rel = match crate::queue::relative_under_root(&entry.library_root, &entry.resource) {
            Some(r) => r,
            None => continue,
        };
        if entry.library_root.trim_end_matches('/') == row.library_path.trim_end_matches('/')
            && rel == row.item_path
        {
            return Ok(entry.id.clone());
        }
    }
    Err(Error::WallpaperNotFound(format!(
        "{} / {}",
        row.library_path, row.item_path
    )))
}

async fn step_shuffle(
    app: &Arc<AppState>,
    filters: &[crate::control_proto::WallpaperFilterRule],
    logics: &[crate::control_proto::FilterLogic],
    delta: i32,
) -> Result<repo::QueueRow> {
    // Lock-free preflight: snapshot whether the round is empty so we
    // can fetch ids without holding the queue mutex through the DB call.
    let need_round = {
        let q = app.queue.lock().await;
        q.shuffle_round.is_empty()
    };
    if need_round {
        let ids = repo::list_item_ids_by_filter(&app.db, filters, logics).await?;
        if ids.is_empty() {
            return Err(Error::FailedPrecondition("queue is empty".into()));
        }
        let mut q = app.queue.lock().await;
        let avoid = q.last_db_id;
        q.build_shuffle_round(ids, avoid, 0);
        let pick = q.shuffle_round[0];
        q.shuffle_pos = 0;
        drop(q);
        return repo::get_item_with_library(&app.db, pick)
            .await?
            .ok_or_else(|| Error::FailedPrecondition("queue is empty".into()));
    }

    let pick = {
        let mut q = app.queue.lock().await;
        let len = q.shuffle_round.len() as i64;
        let raw = q.shuffle_pos as i64 + delta as i64;
        if raw >= len || raw < 0 {
            // Wrap: rebuild the round.
            let avoid = q.last_db_id;
            let target = if raw >= len {
                0usize
            } else {
                q.shuffle_round.len().saturating_sub(1)
            };
            let candidates = q.shuffle_round.clone();
            q.build_shuffle_round(candidates, avoid, target);
            q.shuffle_pos = target;
        } else {
            q.shuffle_pos = raw as usize;
        }
        q.shuffle_round[q.shuffle_pos]
    };

    repo::get_item_with_library(&app.db, pick)
        .await?
        .ok_or_else(|| Error::FailedPrecondition("queue is empty".into()))
}

/// Set the rotation mode on the active playlist. Pure in-memory; the
/// caller is responsible for persistence (settings + DB) when the
/// active playlist is a real DB row.
pub async fn set_mode(app: &Arc<AppState>, mode: Mode) {
    app.queue.lock().await.set_mode(mode);
    app.settings.update(|s| {
        s.global.queue_mode = mode.as_str().to_owned();
    });
    crate::dbus_iface::notify_queue_mode_changed(app).await;
    crate::tray::dbusmenu::notify_menu_changed(app).await;
}

/// Set the auto-rotation interval (seconds; `0` disables). Updates
/// the live rotator via the watch handle and persists the value to
/// settings so a daemon restart resumes the same cadence.
pub async fn set_rotation_interval(app: &Arc<AppState>, secs: u32) {
    app.rotation.set_interval(secs);
    app.settings.update(|s| {
        s.global.rotation_secs = secs;
    });
    crate::dbus_iface::notify_rotation_secs_changed(app).await;
    crate::tray::dbusmenu::notify_menu_changed(app).await;
}

/// Convenience: flip shuffle on/off without exposing the [`Mode`]
/// enum to D-Bus / WS callers. `true` → Shuffle, `false` → Sequential.
pub async fn set_shuffle(app: &Arc<AppState>, on: bool) {
    let mode = if on { Mode::Shuffle } else { Mode::Sequential };
    set_mode(app, mode).await;
}

/// Snapshot of the live playlist state for status reporting.
#[derive(Debug, Clone)]
pub struct QueueStatus {
    pub active_id: Option<i64>,
    pub mode: String,
    pub interval_secs: u32,
    pub current: Option<String>,
    pub position: Option<u32>,
    pub count: u32,
    pub is_smart: bool,
}

pub async fn queue_status(app: &Arc<AppState>) -> QueueStatus {
    let (filters, logics) = app.settings.global().wallpaper_filter.to_pb();
    let count = repo::count_items_by_filter(&app.db, &filters, &logics)
        .await
        .unwrap_or(0) as u32;
    let g = app.queue.lock().await;
    QueueStatus {
        active_id: None,
        mode: g.mode.as_str().to_owned(),
        interval_secs: app.rotation.interval(),
        current: g.current.clone(),
        position: None,
        count,
        is_smart: !filters.is_empty(),
    }
}

/// Restore the persisted wallpaper + queue state. Idempotent —
/// callable on demand if a future feature wants to "re-load saved
/// state" without a full daemon restart. Publishes `RestoreApplied`
/// or `RestoreFailed` on the global event bus on completion so
/// observers (logs, integration tests, future UI status) can react.
pub async fn run_restore(app: &Arc<AppState>, restore_last: bool) -> Result<()> {
    use crate::events::GlobalEvent;

    let mut applied: Option<String> = None;

    if restore_last {
        if let Some(last_id) = app.settings.global().last_wallpaper.clone() {
            log::info!("restoring last wallpaper: {last_id}");
            match apply_wallpaper_by_id(app, &last_id).await {
                Ok(_) => applied = Some(last_id),
                Err(e) => {
                    log::warn!("failed to restore last wallpaper: {e:#}");
                    app.events
                        .publish(GlobalEvent::RestoreFailed(format!("apply: {e:#}")));
                }
            }
        }
    }

    let g = app.settings.global();
    if let Some(mode) = crate::queue::Mode::from_str(&g.queue_mode) {
        app.queue.lock().await.set_mode(mode);
    }
    if g.rotation_secs > 0 {
        app.rotation.set_interval(g.rotation_secs);
    }

    app.events.publish(GlobalEvent::RestoreApplied(applied));
    Ok(())
}

/// Block until at least one display is registered with the router
/// (or `timeout` elapses, whichever comes first). Returns `true` if
/// a display is up by the time we return, `false` on timeout.
///
/// Used by the startup-restore path so applying the saved wallpaper
/// doesn't race the display backend's first connect — without this
/// gate the renderer spawns into a vacuum, the relink-all-displays
/// step is a no-op (no displays yet), and the wallpaper never
/// actually shows up on screen.
pub async fn wait_for_display(app: &Arc<AppState>, timeout: Duration) -> bool {
    // Fast path: a display is already registered (e.g. KDE wallpaper
    // plugin connected before the startup task got around to running).
    if !app.router.snapshot_displays().await.is_empty() {
        return true;
    }
    let mut events = app.router.subscribe_events();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return false,
            evt = events.recv() => match evt {
                Ok(crate::routing::RouterEvent::DisplayUpsert(_)) => return true,
                Ok(crate::routing::RouterEvent::DisplaysReplace(list)) if !list.is_empty() => {
                    return true;
                }
                Ok(_) => continue,
                Err(_) => {
                    // Broadcast lag or channel close — fall back to a
                    // direct snapshot. Either we missed the upsert
                    // event (and the snapshot is now non-empty, return
                    // true) or the router shut down (snapshot empty,
                    // restore won't help anyway).
                    return !app.router.snapshot_displays().await.is_empty();
                }
            }
        }
    }
}

/// Auto-rotation task body. Lives here (not in `playlist::rotator`)
/// because it depends on `AppState` + `control::step`, both private
/// to the binary. Reads the live `RotationConfig` from a watch and
/// either parks (interval = 0) or fires `step(+1)` every
/// `interval_secs`. Any config edit (new interval, manual kick)
/// resets the deadline.
pub async fn run_rotator(
    app: Arc<AppState>,
    mut rx: tokio::sync::watch::Receiver<RotationConfig>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    log::info!("playlist rotator started");
    loop {
        let cfg = *rx.borrow();
        if cfg.interval_secs == 0 {
            tokio::select! {
                _ = rx.changed() => continue,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        } else {
            let dur = std::time::Duration::from_secs(cfg.interval_secs as u64);
            tokio::select! {
                _ = tokio::time::sleep(dur) => {
                    if rx.borrow().interval_secs == 0 {
                        continue;
                    }
                    if let Err(e) = step(&app, 1).await {
                        log::warn!("rotator tick step failed: {e:#}");
                    }
                    // step() calls rotation.kick() on success which
                    // emits a watch change; the next iteration's
                    // rx.changed arm wakes immediately and we re-arm
                    // the sleep — the user-pressed-Next branch is
                    // identical, so manual + auto share one code path.
                }
                _ = rx.changed() => continue,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    }
    log::info!("playlist rotator exited");
}

pub async fn pause_all(app: &Arc<AppState>) -> Result<()> {
    send_all(app, ControlMsg::Pause).await
}

pub async fn resume_all(app: &Arc<AppState>) -> Result<()> {
    send_all(app, ControlMsg::Play).await
}

async fn send_all(app: &Arc<AppState>, msg: ControlMsg) -> Result<()> {
    let ids = app.renderer_manager.list().await;
    for id in ids {
        if let Err(e) = app.renderer_manager.send_control(&id, msg.clone()).await {
            log::warn!("control {id}: {e}");
        }
    }
    Ok(())
}

pub async fn rescan(app: &Arc<AppState>) -> Result<usize> {
    refresh_sources(app).await
}

/// Run every source plugin's `auto_detect(ctx)` against well-known
/// locations and register whatever exists as a library. Duplicates
/// (paths already registered for the same plugin) are silently
/// skipped. Emits `LibraryUpsert` events and kicks off a full
/// rescan so the newly-detected libraries immediately show up in the
/// UI. Returns the snapshots that were actually added.
pub async fn auto_detect_libraries(
    app: &Arc<AppState>,
) -> Result<Vec<crate::routing::LibrarySnapshot>> {
    use crate::routing::LibrarySnapshot;

    let detected = {
        let sm = app.source_manager.lock().await;
        sm.auto_detect_all().await?
    };
    if detected.is_empty() {
        return Ok(Vec::new());
    }

    let mut added: Vec<LibrarySnapshot> = Vec::new();
    for (plugin_name, paths) in detected {
        let plugin = match repo::find_plugin_by_name(&app.db, &plugin_name).await? {
            Some(p) => p,
            None => {
                log::warn!("auto_detect: plugin '{plugin_name}' not registered in DB, skipping");
                continue;
            }
        };
        for path in paths {
            match repo::find_library(&app.db, plugin.id, &path).await {
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(e) => {
                    log::warn!("auto_detect: find_library({path}): {e:#}");
                    continue;
                }
            }
            match repo::add_library(&app.db, plugin.id, &path).await {
                Ok(lib) => {
                    let snap = LibrarySnapshot {
                        id: lib.id,
                        path: lib.path,
                        plugin_name: plugin_name.clone(),
                    };
                    app.router.upsert_library(snap.clone());
                    added.push(snap);
                }
                Err(e) => log::warn!("auto_detect: add_library({path}): {e:#}"),
            }
        }
    }

    if !added.is_empty() {
        app.events
            .publish(crate::events::GlobalEvent::LibrariesAdded {
                paths: added.iter().map(|s| s.path.clone()).collect(),
            });
    }

    if !added.is_empty() {
        let app_clone = app.clone();
        tokio::spawn(async move {
            if let Err(e) = refresh_sources(&app_clone).await {
                log::warn!("rescan after auto_detect failed: {e:#}");
            }
        });
    }
    Ok(added)
}

/// Pull every library row out of the DB and rehydrate it into the
/// router-wire `LibrarySnapshot` shape (path + plugin_name). Used by
/// the `LibraryList` query and the initial snapshot sent to WS
/// subscribers; the router no longer caches these — DB is authoritative.
pub async fn list_library_snapshots(
    db: &sea_orm::DatabaseConnection,
) -> Vec<crate::routing::LibrarySnapshot> {
    let libs = match repo::list_libraries(db).await {
        Ok(v) => v,
        Err(e) => {
            log::warn!("list_libraries: {e:#}");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(libs.len());
    for lib in libs {
        let plugin_name = repo::find_plugin_by_id(db, lib.plugin_id)
            .await
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default();
        out.push(crate::routing::LibrarySnapshot {
            id: lib.id,
            path: lib.path,
            plugin_name,
        });
    }
    out.sort_by_key(|l| l.id);
    out
}

/// Query the DB for every registered library, grouped by the plugin
/// name that owns it. Feeds per-plugin library paths into Lua's
/// `ctx.libraries()` and seeds `protected_libraries` on sync so an
/// empty scan doesn't nuke user-configured folders.
pub async fn libraries_by_plugin_name(
    db: &sea_orm::DatabaseConnection,
) -> Result<HashMap<String, Vec<String>>> {
    let libs = repo::list_libraries(db).await?;
    let mut by_plugin_id: HashMap<i64, Vec<String>> = HashMap::new();
    for lib in libs {
        by_plugin_id
            .entry(lib.plugin_id)
            .or_default()
            .push(lib.path);
    }
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for (pid, paths) in by_plugin_id {
        if let Ok(Some(p)) = repo::find_plugin_by_id(db, pid).await {
            by_name.insert(p.name, paths);
        }
    }
    Ok(by_name)
}

/// Re-scan every loaded source plugin against the current DB library
/// set and persist the resulting entries. Returns the playlist size.
/// Called from startup after plugins load, from manual `rescan`, and
/// from `LibraryAdd` / `LibraryRemove` so the in-memory snapshot and
/// DB stay consistent with the user-managed library list.
pub async fn refresh_sources(app: &Arc<AppState>) -> Result<usize> {
    use std::sync::atomic::Ordering;
    app.scan_in_progress.store(true, Ordering::SeqCst);
    // Sync start is observable to UIs via `StatusSync.scan_in_progress`.
    app.events
        .publish(crate::events::GlobalEvent::StatusChanged);

    let result = refresh_sources_inner(app).await;

    app.scan_in_progress.store(false, Ordering::SeqCst);
    match &result {
        Ok(count) => app
            .events
            .publish(crate::events::GlobalEvent::SyncFinished { count: *count }),
        Err(e) => app
            .events
            .publish(crate::events::GlobalEvent::SyncFailed(format!("{e:#}"))),
    }
    app.events
        .publish(crate::events::GlobalEvent::StatusChanged);
    result
}

async fn refresh_sources_inner(app: &Arc<AppState>) -> Result<usize> {
    let libs_by_plugin = libraries_by_plugin_name(&app.db).await?;

    let source_mgr = app.source_manager.clone();
    let libs_for_scan = libs_by_plugin.clone();
    // The Lua VM lock (`source_manager`) is held only during the scan
    // itself. Read consumers (`WallpaperList`/`WallpaperApply`/
    // `SourceList`) go through `source_snapshot` instead and never park
    // behind this section.
    //
    // `scan_all` is async because Lua plugins can call mlua async
    // functions (`ctx.library_meta_*`) that await sea-orm. We still
    // run the scan inside `spawn_blocking` so the long CPU-bound
    // filesystem walks don't block an async worker — `Handle::block_on`
    // from inside `spawn_blocking` drives the async-Lua future to
    // completion on this dedicated thread.
    let handle = tokio::runtime::Handle::current();
    let snapshot: Vec<WallpaperEntry> = tokio::task::spawn_blocking(move || {
        let mut sm = source_mgr.blocking_lock();
        handle.block_on(sm.scan_all(&libs_for_scan))?;
        Ok::<_, anyhow::Error>(sm.list().to_vec())
    })
    .await
    .map_err(|e| Error::Internal(anyhow!("source scan join: {e}")))??;

    let plugins = {
        let sm = app.source_manager.lock().await;
        sm.plugins().unwrap_or_default()
    };

    // Install the fresh snapshot under the read-only mirror's brief
    // write guard. Cheap clone here keeps `snapshot` available for the
    // per-plugin DB sync below.
    {
        let mut snap = app.source_snapshot.write().await;
        snap.install(snapshot.clone(), plugins.clone());
    }

    for info in &plugins {
        let entries: Vec<_> = snapshot
            .iter()
            .filter(|e| e.plugin_name == info.name)
            .cloned()
            .collect();
        let protected = libs_by_plugin.get(&info.name).cloned().unwrap_or_default();
        match sync::sync_plugin_entries(
            &app.db,
            sync::PluginRef {
                name: &info.name,
                version: &info.version,
            },
            &entries,
            &protected,
        )
        .await
        {
            Ok((summary, _)) => log::info!(
                "sync plugin={} v{}: +{} / -{} items, -{} libraries, {} dropped",
                info.name,
                info.version,
                summary.items_upserted,
                summary.items_deleted,
                summary.libraries_deleted,
                summary.dropped,
            ),
            Err(e) => log::warn!("sync plugin={} failed: {e:#}", info.name),
        }
    }

    let count = snapshot.len();
    // Queue plays dynamically from settings.wallpaper_filter; nothing
    // to rebind after a sources refresh — the next `step()` will see
    // the new DB rows. Invalidate any pre-built shuffle round so it's
    // rematerialized including the freshly-imported items.
    app.queue.lock().await.reset_shuffle_round();

    // Kick a one-shot probe drain so newly-imported items don't have
    // to wait for the next scheduler tick. `spawn_async_unique` collapses
    // overlapping refresh→probe bursts (e.g. a flurry of LibraryAdd
    // calls) into a single in-flight pass.
    let probe = app.probe.clone();
    let db = app.db.clone();
    app.tasks.spawn_async_unique(
        crate::tasks::TaskKind::Generic,
        "probe/refresh",
        "probe/post-refresh",
        async move {
            // run_pending emits its own info log; we only care about
            // surfacing the error here.
            crate::probe::task::run_pending(
                &db,
                probe,
                Some(crate::probe::task::PROBE_REFRESH_BATCH),
            )
            .await
            .map(|_| ())
            .map_err(anyhow::Error::from)
        },
    );

    Ok(count)
}
