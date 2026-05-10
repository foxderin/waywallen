//! Router — owns a `RoutingTable` plus a per-renderer subscription
//! task. Translates renderer broadcasts and table mutations into
//! per-display `DisplayOutEvent` streams that `display::endpoint`
//! consumes via plain mpsc.
//!
//! Phase 1 policy:
//!   * One enabled link per display (single-wallpaper mode).
//!   * `register_display` auto-creates a link to whichever renderer is
//!     currently "first" in the table.
//!   * `relink_all_displays_to(id)` re-points every display at the
//!     same renderer (used by `WallpaperApply`).
//!
//! Each display has a `last_renderer` / `last_buffer_generation`
//! sentinel; `sync_display` is the single point where the router
//! decides whether to push `Unbind`/`Bind`/`SetConfig`. The sentinels
//! make `sync_display` idempotent — it can be called multiple times
//! safely after one mutation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, broadcast::error::RecvError, mpsc, Mutex as TokioMutex};
use tokio::task::JoinHandle;

/// Backstop only. The mainline path is `Router::mark_orphan`, which
/// schedules a per-renderer timer when a renderer loses its last
/// enabled link. This timeout exists purely to catch renderers that
/// somehow ended up paused without going through the orphan-marking
/// path (defensive — should never fire in practice).
const IDLE_KILL_TIMEOUT: Duration = Duration::from_secs(3600);
/// How often the backstop reaper task wakes up to scan for stragglers.
const IDLE_SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// Grace period an orphan renderer keeps running before it is killed.
/// Only granted when the daemon has zero displays AND the orphan is
/// the only renderer in the system — the grace window absorbs a quick
/// monitor hot-replug so the lone renderer survives. In every other
/// case orphans are reaped synchronously to free GPU memory promptly.
const ORPHAN_REAP_TIMEOUT: Duration = Duration::from_secs(5);

use crate::display::layout::{self, FillMode, LayoutInput};
use crate::ipc::proto::{ControlMsg, EventMsg};
use crate::renderer_manager::{
    DrmNode, RendererHandle, RendererId, RendererManager, BUF_HOST_VISIBLE,
};
use crate::scheduler::{DisplayId, DisplayInfo, ProjectedConfig};
use crate::settings::{ResolvedLayout, SettingsStore};

use super::table::{Link, LinkDstRect, LinkId, LinkSrcRect, RoutingTable};

/// Wire-translated event streamed from router to a display endpoint.
/// The endpoint owns translation to the on-the-wire `Event`.
pub enum DisplayOutEvent {
    /// Bind the buffer pool currently published by `renderer`. The
    /// endpoint reads `renderer.bind_snapshot()` itself so the router
    /// doesn't have to clone fds for every subscriber.
    Bind { renderer: Arc<RendererHandle> },
    /// Retire the named buffer pool generation.
    Unbind { buffer_generation: u64 },
    /// Update composition geometry / clear color.
    SetConfig(ProjectedConfig),
    /// A frame is ready on `renderer` at `buffer_index` for the named
    /// generation. The endpoint pulls the matching sync_fd from
    /// `renderer.clone_sync_fd(seq)` itself.
    Frame {
        renderer: Arc<RendererHandle>,
        buffer_generation: u64,
        buffer_index: u32,
        seq: u64,
        /// Timeline value the producer assigned to this frame on its
        /// release_syncobj. The endpoint reports this back to the
        /// reaper alongside the per-consumer binary syncobj it
        /// allocates.
        release_point: u64,
        /// Total number of consumer endpoints the router dispatched
        /// this `release_point` to (i.e. fan-out width). The reaper
        /// uses it to bucket per-consumer FrameRecords with the same
        /// release_point and only TRANSFER once every consumer has
        /// signaled — preventing the producer from racing against a
        /// late consumer.
        expected_count: u32,
    },
}

/// Initial-registration payload from `display::endpoint::do_handshake`.
pub struct DisplayRegistration {
    pub name: String,
    /// Stable identifier persisted by the consumer (e.g. UUID4 stored in
    /// the KDE/GNOME extension config). When `Some`, used as the key
    /// into [`SettingsStore::displays`]; on `None` the router falls back
    /// to the v3 behavior of indexing settings by `name`.
    pub instance_id: Option<String>,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    /// DRM render-node id of the GPU this display will sample dmabufs
    /// on (i.e. the GPU backing the consumer's EGL/Vulkan context).
    /// `DrmNode::UNKNOWN` means the consumer couldn't introspect its
    /// backend and the router must conservatively assume a cross-GPU
    /// path (force `BUF_HOST_VISIBLE` on every connected renderer).
    pub gpu: DrmNode,
    pub properties: Vec<(String, String)>,
    /// Modifier-negotiation capabilities the consumer declared in
    /// its `consumer_caps` request (sent immediately after
    /// `register_display`). `None` if the consumer hasn't been
    /// ported to v2; the router falls back to legacy behavior in
    /// that case.
    pub consumer_caps: Option<crate::dma::negotiate::PeerCaps>,
}

/// Returned from `register_display` — the assigned id plus the rx end
/// of the dispatcher's per-display channel.
pub struct DisplayHandle {
    pub id: DisplayId,
    pub rx: mpsc::UnboundedReceiver<DisplayOutEvent>,
}

/// Read-only view of a single (renderer → display) link for UI
/// consumers. Subset of `table::Link` that hides table-internal ids.
#[derive(Debug, Clone)]
pub struct DisplayLinkSnapshot {
    pub renderer_id: RendererId,
    pub z_order: i32,
}

/// Transport-agnostic router event. `ws_server` subscribes and
/// translates these into `pb::Event`s on the wire; tests can also
/// subscribe and observe router state changes without going through the
/// protobuf layer.
#[derive(Debug, Clone)]
pub enum RouterEvent {
    /// A single display was added or its fields changed (links, size).
    /// Receivers should upsert by `snap.id`.
    DisplayUpsert(DisplaySnapshot),
    /// A display was unregistered. Receivers should drop the entry.
    DisplayRemoved(DisplayId),
    /// A batch mutation affected many displays — send the whole list
    /// as a single replace instead of N upserts.
    DisplaysReplace(Vec<DisplaySnapshot>),
    /// A renderer was added or its runtime fields changed (status, fps).
    /// Receivers should upsert by `snap.id`.
    RendererUpsert(RendererSnapshot),
    /// A renderer was unregistered. Receivers should drop the entry.
    RendererRemoved(RendererId),
    /// A batch mutation affected many renderers — send the whole list
    /// as a single replace.
    RenderersReplace(Vec<RendererSnapshot>),
    /// A single library was added or its fields changed.
    LibraryUpsert(LibrarySnapshot),
    /// A library was removed.
    LibraryRemoved(i64),
    /// A batch mutation affected many libraries.
    LibrariesReplace(Vec<LibrarySnapshot>),
}

/// Read-only view of a registered library.
#[derive(Debug, Clone)]
pub struct LibrarySnapshot {
    pub id: i64,
    pub path: String,
    pub plugin_name: String,
}

/// Lifecycle state of a renderer as seen by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererStatus {
    Playing,
    Paused,
}

impl RendererStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
        }
    }
}

/// Read-only view of a registered renderer. Returned from
/// `Router::snapshot_renderers`; mirrors the fields surfaced on the
/// control-plane `RendererInstance` message minus per-plugin settings
/// (those live in the settings store and are looked up at the wire-
/// translation boundary).
#[derive(Debug, Clone)]
pub struct RendererSnapshot {
    pub id: RendererId,
    pub wp_type: String,
    pub name: String,
    pub status: RendererStatus,
    pub pid: u32,
    pub drm_render_major: u32,
    pub drm_render_minor: u32,
    pub texture_width: u32,
    pub texture_height: u32,
}

/// Read-only view of a registered display. Returned from
/// `Router::snapshot_displays`; carries metadata from `DisplayInfo`
/// plus the enabled links currently pointing at this display.
#[derive(Debug, Clone)]
pub struct DisplaySnapshot {
    pub id: DisplayId,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub links: Vec<DisplayLinkSnapshot>,
    pub drm_render_major: u32,
    pub drm_render_minor: u32,
}

struct DisplayState {
    info: DisplayInfo,
    /// DRM render-node id of the consumer's GPU. Compared against
    /// `RendererHandle::gpu` to decide whether dmabufs need to be
    /// re-exported as HOST_VISIBLE (cross-GPU) or can stay
    /// DEVICE_LOCAL (zero-copy).
    gpu: DrmNode,
    tx: mpsc::UnboundedSender<DisplayOutEvent>,
    /// Last renderer this display was bound to (None if currently unbound).
    last_renderer: Option<RendererId>,
    /// Last `buffer_generation` we sent in a `Bind` to this display.
    /// Tracked so a follow-up `Unbind` retires the right gen.
    last_buffer_generation: Option<u64>,
    /// Consumer's modifier-negotiation caps. `None` until the
    /// `consumer_caps` request has been received (or forever for
    /// legacy clients). The router pairs this with the bound
    /// renderer's `format_caps` to compute a `NegotiatedScheme`.
    consumer_caps: Option<crate::dma::negotiate::PeerCaps>,
}

struct Inner {
    table: RoutingTable,
    displays: HashMap<DisplayId, DisplayState>,
    renderer_tasks: HashMap<RendererId, JoinHandle<()>>,
    /// Renderers we've already sent `Pause` to. Used to compute the
    /// Play/Pause diff when ref_counts change so we never send the
    /// same control twice.
    paused_renderers: std::collections::HashSet<RendererId>,
    /// Timestamp of the Pause transition for each paused renderer.
    /// Consumed by the reaper task to enforce `IDLE_KILL_TIMEOUT`.
    paused_since: HashMap<RendererId, Instant>,
    /// Pending orphan-reap timers, keyed by renderer id. Inserted by
    /// `mark_orphan` and cleared by `cancel_orphan_timer`. The task
    /// itself also clears its own entry once it commits to the kill
    /// path so a re-mark after wake-up reschedules cleanly.
    orphan_timers: HashMap<RendererId, JoinHandle<()>>,
    next_display_id: u64,
    next_config_generation: u64,
}

pub struct Router {
    inner: TokioMutex<Inner>,
    /// For Pause/Play lifecycle control. Phase 2: a renderer with zero
    /// enabled links is paused; the next link added resumes it.
    mgr: Arc<RendererManager>,
    /// Fan-out channel for `RouterEvent`s. Always present; `send` errors
    /// when there are no subscribers are logged at debug and ignored.
    events_tx: broadcast::Sender<RouterEvent>,
    /// Settings store used to resolve per-display fillmode/align when
    /// computing `set_config`. Set once at startup via
    /// [`Router::attach_settings`]; tests omit it and fall back to
    /// `LayoutDefaults::default()` (Stretched + Center, identity).
    settings: std::sync::OnceLock<Arc<SettingsStore>>,
}

impl Router {
    /// Borrow the underlying RendererManager. Used by the display
    /// endpoint to forward pointer events to the currently bound
    /// renderer without going through routing-table walks (the bind
    /// event already hands it the right renderer handle).
    pub fn renderer_manager(&self) -> &Arc<RendererManager> {
        &self.mgr
    }

    pub fn new(mgr: Arc<RendererManager>) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(128);
        let router = Arc::new(Self {
            inner: TokioMutex::new(Inner {
                table: RoutingTable::new(),
                displays: HashMap::new(),
                renderer_tasks: HashMap::new(),
                paused_renderers: std::collections::HashSet::new(),
                paused_since: HashMap::new(),
                orphan_timers: HashMap::new(),
                next_display_id: 0,
                next_config_generation: 0,
            }),
            mgr,
            events_tx,
            settings: std::sync::OnceLock::new(),
        });
        // Spawn the idle-renderer reaper.
        {
            let weak = Arc::downgrade(&router);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(IDLE_SCAN_INTERVAL).await;
                    let Some(this) = weak.upgrade() else { return };
                    this.reap_idle_renderers().await;
                }
            });
        }
        router
    }

    /// Wire the daemon's `SettingsStore` so `sync_display` can resolve
    /// per-display fillmode/align when projecting `set_config`. Called
    /// exactly once at boot from `main.rs`. Tests skip it and fall
    /// back to `LayoutDefaults::default()`.
    pub fn attach_settings(self: &Arc<Self>, settings: Arc<SettingsStore>) {
        if self.settings.set(settings).is_err() {
            log::warn!("router: attach_settings called twice; ignoring second call");
        }
    }

    /// Resolve effective layout for a display, defaulting to identity
    /// (Stretched + Center) when settings haven't been attached (tests,
    /// very early boot).
    ///
    /// Lookup precedence (v4):
    ///   1. `[display.<instance_id>]` if the consumer advertised one,
    ///   2. `[display.<name>]` as legacy fallback (v3 clients + un-
    ///      migrated TOML entries).
    fn resolved_layout(&self, info: &DisplayInfo) -> ResolvedLayout {
        let Some(s) = self.settings.get() else {
            return ResolvedLayout {
                fillmode: FillMode::default(),
                align: Default::default(),
            };
        };
        if let Some(iid) = info.instance_id.as_deref() {
            if s.display_prefs(iid).is_some() {
                return s.resolved_layout(iid);
            }
            // No instance_id-keyed entry yet — fall back to the legacy
            // name-keyed entry so old config keeps working until the
            // one-shot migration in `register_display` runs.
        }
        s.resolved_layout(&info.name)
    }

    /// Settings TOML key used for this display's persistent prefs.
    /// Prefers the v4 stable `instance_id`; falls back to `name` for
    /// legacy v3 clients (or v4 clients that explicitly sent empty).
    fn settings_key_for(info: &DisplayInfo) -> &str {
        info.instance_id.as_deref().unwrap_or(&info.name)
    }

    /// Set or clear per-display layout fields. `None` for a field
    /// means "no change"; the only way to *clear* a per-display
    /// override is via the explicit `clear_*` flags (set on the
    /// caller side before invoking). This method is the entry point
    /// for the `DisplayLayoutSet` control RPC. After mutating the
    /// settings store it re-syncs the display so the consumer
    /// receives an updated `set_config`.
    pub async fn set_display_layout(
        self: &Arc<Self>,
        display_name: String,
        new_fillmode: Option<crate::display::layout::FillMode>,
        new_align: Option<crate::display::layout::Align>,
        clear_fillmode: bool,
        clear_align: bool,
    ) {
        let Some(settings) = self.settings.get().cloned() else {
            log::warn!(
                "router: set_display_layout({display_name}) called before settings attached"
            );
            return;
        };
        // Resolve the live display first so we know whether it has a
        // stable v4 `instance_id` to key persistent settings under.
        // Falls back to `display_name` for legacy v3 clients (or when
        // the display is currently disconnected — the RPC still lets
        // the user edit prefs by name).
        let target_id = self.find_display_by_name(&display_name).await;
        let key = match target_id {
            Some(did) => {
                let inner = self.inner.lock().await;
                inner
                    .displays
                    .get(&did)
                    .and_then(|s| s.info.instance_id.clone())
                    .unwrap_or_else(|| display_name.clone())
            }
            None => display_name.clone(),
        };
        settings.update(|s| {
            let entry = s.displays.entry(key.clone()).or_default();
            if clear_fillmode {
                entry.fillmode = None;
            }
            if let Some(v) = new_fillmode {
                entry.fillmode = Some(v);
            }
            if clear_align {
                entry.align = None;
            }
            if let Some(v) = new_align {
                entry.align = Some(v);
            }
            // Prune empty entry to keep the on-disk file tidy.
            if entry.is_empty() {
                s.displays.remove(&key);
            }
        });
        if let Some(did) = target_id {
            self.resync_display_set_config(did).await;
            if let Some(snap) = self.snapshot_display(did).await {
                self.emit(RouterEvent::DisplayUpsert(snap));
            }
        }
    }

    /// Re-emit `set_config` for a single display to pick up new
    /// settings. Cheaper than `sync_display` because it skips the
    /// Bind/Unbind diff check; settings changes never alter
    /// renderer-binding state.
    async fn resync_display_set_config(self: &Arc<Self>, display_id: DisplayId) {
        let mut inner = self.inner.lock().await;
        if !inner.displays.contains_key(&display_id) {
            return;
        }
        let display_links = inner.table.links_for_display(display_id);
        let target = display_links.into_iter().find(|l| l.enabled).and_then(|l| {
            let renderer = inner.table.get_renderer(&l.renderer_id)?;
            let gen = renderer
                .bind_snapshot()
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| s.generation))?;
            Some((l, renderer, gen))
        });
        let Some((link, renderer, _gen)) = target else {
            return;
        };
        inner.next_config_generation += 1;
        let cfg_gen = inner.next_config_generation;
        let info = inner.displays.get(&display_id).unwrap().info.clone();
        let layout = self.resolved_layout(&info);
        let cfg = project_link(&link, &renderer, &info, cfg_gen, &layout);
        if let Some(state) = inner.displays.get(&display_id) {
            let _ = state.tx.send(DisplayOutEvent::SetConfig(cfg));
        }
    }

    async fn find_display_by_name(self: &Arc<Self>, name: &str) -> Option<DisplayId> {
        let inner = self.inner.lock().await;
        inner
            .displays
            .iter()
            .find(|(_, s)| s.info.name == name)
            .map(|(id, _)| *id)
    }

    /// Re-emit `set_config` for every registered display. Called from
    /// the control surface after a global `SettingsSet` so per-display
    /// overrides plus the new global defaults propagate uniformly.
    pub async fn resync_all_set_configs(self: &Arc<Self>) {
        let ids: Vec<DisplayId> = {
            let inner = self.inner.lock().await;
            inner.displays.keys().copied().collect()
        };
        for did in ids {
            self.resync_display_set_config(did).await;
        }
    }

    /// Push a DisplaysReplace router event after a settings-only
    /// change so subscribed UIs refresh `effective_layout` /
    /// `layout_override` for every display. The argument is a
    /// pre-fetched snapshot to avoid a redundant lock round-trip.
    pub fn emit_displays_replace_for_settings_change(self: &Arc<Self>, snap: Vec<DisplaySnapshot>) {
        self.emit(RouterEvent::DisplaysReplace(snap));
    }

    /// Kill renderers that have been paused longer than
    /// `IDLE_KILL_TIMEOUT`. Called periodically by the reaper task
    /// spawned in `new()`.
    async fn reap_idle_renderers(self: &Arc<Self>) {
        let now = Instant::now();
        let victims: Vec<RendererId> = {
            let inner = self.inner.lock().await;
            inner
                .paused_since
                .iter()
                .filter_map(|(id, t)| {
                    if now.duration_since(*t) >= IDLE_KILL_TIMEOUT {
                        Some(id.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        for id in victims {
            log::info!("router: reaping idle renderer {id}");
            self.unregister_renderer(&id).await;
            if let Err(e) = self.mgr.kill(&id).await {
                log::warn!("router: reaper kill {id}: {e}");
            }
        }
    }

    // ---------------------------------------------------------------
    // Renderer lifecycle
    // ---------------------------------------------------------------

    pub async fn register_renderer(self: &Arc<Self>, handle: Arc<RendererHandle>) {
        let id = handle.id.clone();
        let task = {
            let mut events = handle.events();
            let router = Arc::clone(self);
            let rid = id.clone();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(EventMsg::BindBuffers { .. }) => {
                            router.on_renderer_bind(&rid).await;
                        }
                        Ok(EventMsg::FrameReady {
                            image_index,
                            seq,
                            release_point,
                            ..
                        }) => {
                            router
                                .on_renderer_frame(&rid, image_index, seq, release_point)
                                .await;
                        }
                        Ok(EventMsg::FormatCaps { .. }) => {
                            // Renderer just shipped its
                            // modifier-negotiation caps. Trigger
                            // reconcile so the picker can run for any
                            // already-registered displays — without
                            // this, a display that registered before
                            // the caps arrived would never see a
                            // NegotiateBuffers dispatch.
                            router.reconcile_buffer_flags().await;
                        }
                        Ok(EventMsg::BindFailed {
                            fourcc, modifier, ..
                        }) => {
                            // Iter 5: renderer rejected the picked
                            // (fourcc, modifier). Blacklist it on
                            // the producer side and re-pick.
                            router.on_renderer_bind_failed(&rid, fourcc, modifier).await;
                        }
                        Ok(EventMsg::ReportState { .. }) => {
                            // The reader thread already parsed
                            // recognised keys onto the handle (clear
                            // color today). Re-emit set_config so live
                            // displays pick up the new value.
                            router.on_renderer_state_changed(&rid).await;
                        }
                        Ok(_) => {}
                        Err(RecvError::Closed) => {
                            log::info!("router: renderer {rid} broadcast closed");
                            return;
                        }
                        Err(RecvError::Lagged(n)) => {
                            log::warn!("router: renderer {rid} lagged {n} events");
                        }
                    }
                }
            })
        };
        let snap_id = id.clone();
        {
            let mut inner = self.inner.lock().await;
            inner.table.add_renderer(handle);
            inner.renderer_tasks.insert(id, task);
        }
        if let Some(snap) = self.snapshot_renderer(&snap_id).await {
            self.emit(RouterEvent::RendererUpsert(snap));
        }
    }

    pub async fn unregister_renderer(self: &Arc<Self>, id: &str) {
        let affected: Vec<DisplayId> = {
            let mut inner = self.inner.lock().await;
            let removed = inner.table.remove_renderer(id);
            if let Some(task) = inner.renderer_tasks.remove(id) {
                task.abort();
            }
            if let Some(task) = inner.orphan_timers.remove(id) {
                task.abort();
            }
            inner.paused_renderers.remove(id);
            inner.paused_since.remove(id);
            removed.into_iter().map(|(_, did)| did).collect()
        };
        self.emit(RouterEvent::RendererRemoved(id.to_string()));
        let had_affected = !affected.is_empty();
        for did in affected {
            self.sync_display(did).await;
        }
        self.reconcile_lifecycle().await;
        if had_affected {
            let all = self.snapshot_displays().await;
            self.emit(RouterEvent::DisplaysReplace(all));
        }
    }

    // ---------------------------------------------------------------
    // Display lifecycle
    // ---------------------------------------------------------------

    pub async fn register_display(self: &Arc<Self>, reg: DisplayRegistration) -> DisplayHandle {
        // One-time legacy migration: if the consumer advertised a v4
        // `instance_id` and there's still only a name-keyed entry from
        // v3 days, copy it to the instance_id key so subsequent
        // resolves hit the new key. The old name key is kept (don't
        // delete) so a roll-back to a v3 client still finds its prefs.
        if let (Some(iid), Some(settings)) =
            (reg.instance_id.as_deref(), self.settings.get().cloned())
        {
            if settings.display_prefs(iid).is_none() {
                if let Some(legacy) = settings.display_prefs(&reg.name) {
                    let iid_owned = iid.to_string();
                    settings.update(|s| {
                        s.displays.entry(iid_owned).or_insert(legacy);
                    });
                    log::info!(
                        "display settings: migrated [display.{}] → [display.{}]",
                        reg.name,
                        iid
                    );
                }
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let (display_id, auto_linked) = {
            let mut inner = self.inner.lock().await;
            inner.next_display_id += 1;
            let id = inner.next_display_id;
            let info = DisplayInfo {
                id,
                name: reg.name,
                instance_id: reg.instance_id,
                width: reg.width,
                height: reg.height,
                refresh_mhz: reg.refresh_mhz,
                properties: reg.properties,
                bound: false,
            };
            inner.displays.insert(
                id,
                DisplayState {
                    info,
                    gpu: reg.gpu,
                    tx,
                    last_renderer: None,
                    last_buffer_generation: None,
                    consumer_caps: reg.consumer_caps,
                },
            );
            // Phase 1 policy: auto-link to whichever renderer is "first".
            let auto = inner.table.first_renderer();
            if let Some(rid) = auto.clone() {
                inner.table.add_link(rid, id);
            }
            (id, auto)
        };
        // A freshly auto-linked renderer just gained an audience —
        // cancel any pending orphan timer so it survives.
        if let Some(rid) = auto_linked.as_deref() {
            self.cancel_orphan_timer(rid).await;
        }
        self.sync_display(display_id).await;
        self.reconcile_lifecycle().await;
        self.reconcile_buffer_flags().await;
        if let Some(snap) = self.snapshot_display(display_id).await {
            self.emit(RouterEvent::DisplayUpsert(snap));
        }
        DisplayHandle { id: display_id, rx }
    }

    pub async fn unregister_display(self: &Arc<Self>, display_id: DisplayId) {
        {
            let mut inner = self.inner.lock().await;
            inner.displays.remove(&display_id);
            inner.table.remove_display(display_id);
        }
        // Any renderer that just lost its last link enters the 5s
        // grace window. `keep = None` because there's no
        // newly-applied renderer to preserve here — every newly
        // orphaned renderer is fair game.
        self.mark_orphans(None).await;
        self.reconcile_lifecycle().await;
        self.reconcile_buffer_flags().await;
        self.emit(RouterEvent::DisplayRemoved(display_id));
    }

    /// Stash the consumer's modifier-negotiation caps on the
    /// display's state and re-run the picker. Idempotent — a later
    /// `ConsumerCaps` overrides the prior one.
    pub async fn set_consumer_caps(
        self: &Arc<Self>,
        display_id: DisplayId,
        caps: crate::dma::negotiate::PeerCaps,
    ) {
        {
            let mut inner = self.inner.lock().await;
            if let Some(s) = inner.displays.get_mut(&display_id) {
                s.consumer_caps = Some(caps);
            } else {
                return;
            }
        }
        self.reconcile_buffer_flags().await;
    }

    /// Iter 5: consumer reported a `bind_failed` for `(fourcc, modifier)`.
    /// Add the pair to this consumer's blacklist on its
    /// [`crate::dma::negotiate::PeerCaps`] and re-run the picker so the
    /// daemon dispatches a fallback scheme. No-op for legacy
    /// consumers that never sent `consumer_caps` (they have nowhere
    /// to put a blacklist).
    pub async fn on_consumer_bind_failed(
        self: &Arc<Self>,
        display_id: DisplayId,
        fourcc: u32,
        modifier: u64,
    ) {
        let inserted = {
            let mut inner = self.inner.lock().await;
            let Some(state) = inner.displays.get_mut(&display_id) else {
                return;
            };
            let Some(caps) = state.consumer_caps.as_mut() else {
                return;
            };
            caps.blacklist.insert((fourcc, modifier))
        };
        if inserted {
            log::info!(
                "router: display {display_id}: blacklisted (0x{fourcc:08x}, 0x{modifier:x}) — re-running picker"
            );
        }
        self.reconcile_buffer_flags().await;
    }

    /// Iter 5: renderer reported a `bind_failed` for `(fourcc, modifier)`.
    /// Add the pair to this producer's blacklist on its
    /// [`crate::dma::negotiate::PeerCaps`] and re-run the picker so the
    /// daemon dispatches a fallback scheme. No-op for legacy
    /// producers that never sent `format_caps`.
    pub async fn on_renderer_bind_failed(
        self: &Arc<Self>,
        renderer_id: &str,
        fourcc: u32,
        modifier: u64,
    ) {
        let inserted = {
            let inner = self.inner.lock().await;
            let Some(renderer) = inner.table.get_renderer(renderer_id) else {
                return;
            };
            renderer.blacklist_format(fourcc, modifier)
        };
        if inserted {
            log::info!(
                "router: renderer {renderer_id}: blacklisted (0x{fourcc:08x}, 0x{modifier:x}) — re-running picker"
            );
        }
        self.reconcile_buffer_flags().await;
    }

    /// Renderer published a `ReportState` event. The reader thread
    /// already merged recognised keys onto the handle (currently just
    /// `clear_color`). Propagate the latest values into every link
    /// the renderer drives and re-emit `set_config` so live displays
    /// pick up the change.
    pub async fn on_renderer_state_changed(self: &Arc<Self>, renderer_id: &str) {
        let new_clear = {
            let inner = self.inner.lock().await;
            let Some(renderer) = inner.table.get_renderer(renderer_id) else {
                return;
            };
            renderer.clear_rgba()
        };
        let affected: Vec<DisplayId> = {
            let mut inner = self.inner.lock().await;
            let link_ids: Vec<LinkId> = inner
                .table
                .links_for_renderer(renderer_id)
                .into_iter()
                .map(|l| l.id)
                .collect();
            let mut affected = Vec::new();
            for lid in link_ids {
                let changed = inner.table.update_link_geometry(
                    lid,
                    None,
                    None,
                    None,
                    Some(new_clear),
                    None,
                );
                if changed {
                    if let Some(link) = inner.table.get_link(lid) {
                        affected.push(link.display_id);
                    }
                }
            }
            affected
        };
        for did in affected {
            self.resync_display_set_config(did).await;
        }
    }

    pub async fn update_display_size(
        self: &Arc<Self>,
        display_id: DisplayId,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 {
            log::warn!(
                "update_display_size: ignoring zero dim ({width}x{height}) for display {display_id:?}",
            );
            return;
        }
        let changed = {
            let mut inner = self.inner.lock().await;
            if let Some(s) = inner.displays.get_mut(&display_id) {
                let differs = s.info.width != width || s.info.height != height;
                s.info.width = width;
                s.info.height = height;
                differs
            } else {
                return;
            }
        };
        // Layout depends on disp_w/disp_h, so any size change must
        // trigger a fresh set_config under the resolved fillmode/align.
        if changed {
            self.resync_display_set_config(display_id).await;
        }
        if let Some(snap) = self.snapshot_display(display_id).await {
            self.emit(RouterEvent::DisplayUpsert(snap));
        }
    }

    /// Whether this renderer is currently in the paused set (zero
    /// enabled links). Returns `false` for unknown ids.
    pub async fn is_paused(self: &Arc<Self>, renderer_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .paused_renderers
            .contains(renderer_id)
    }

    /// Subscribe to router events (display add/change/remove). The
    /// returned receiver is lagged-on-overflow — callers should expect
    /// `RecvError::Lagged` and resync via `snapshot_displays` when it
    /// happens.
    pub fn subscribe_events(self: &Arc<Self>) -> broadcast::Receiver<RouterEvent> {
        self.events_tx.subscribe()
    }

    /// Number of currently registered displays. Cheap (O(1) on the
    /// inner displays map) read used by the apply path to gate
    /// `WallpaperApply` when nothing would observe a fresh spawn.
    pub async fn display_count(self: &Arc<Self>) -> usize {
        self.inner.lock().await.displays.len()
    }

    /// Walk every renderer in the table and schedule a 5s reap timer
    /// for any that have no enabled link, **except** any id in
    /// `keep`. Used by the apply path to reclaim renderers that just
    /// lost their last link — and to preserve the just-applied
    /// renderer in the 0-display case where it has no links yet but
    /// should still hang around for the next display hotplug.
    ///
    /// Returns the list of ids whose timers were scheduled.
    pub async fn mark_orphans(self: &Arc<Self>, keep: Option<&str>) -> Vec<RendererId> {
        // Snapshot candidates plus the system-wide grace condition in
        // one critical section so that all orphans in this batch agree
        // on whether the lone-renderer rule applies — without that, an
        // iterative kill could drop the renderer count to 1 mid-loop
        // and accidentally promote the last orphan into the grace
        // window.
        let (candidates, lone_renderer_no_displays) = {
            let inner = self.inner.lock().await;
            let cs: Vec<RendererId> = inner
                .table
                .renderer_ids()
                .into_iter()
                .filter(|rid| {
                    if Some(rid.as_str()) == keep {
                        return false;
                    }
                    inner
                        .table
                        .links_for_renderer(rid)
                        .iter()
                        .all(|l| !l.enabled)
                })
                .collect();
            let lone = inner.displays.is_empty() && inner.table.renderer_ids().len() == 1;
            (cs, lone)
        };
        for rid in &candidates {
            if lone_renderer_no_displays {
                self.schedule_orphan_grace(rid.clone()).await;
            } else {
                self.kill_orphan_now(rid).await;
            }
        }
        if let Some(k) = keep {
            self.cancel_orphan_timer(k).await;
        }
        candidates
    }

    /// Mark `renderer_id` as orphaned. Reaps immediately unless this
    /// is the only renderer in the system AND no displays are
    /// registered — in that case schedules the 5s grace timer so a
    /// hot-replugged display can re-acquire it.
    pub async fn mark_orphan(self: &Arc<Self>, renderer_id: RendererId) {
        let lone_renderer_no_displays = {
            let inner = self.inner.lock().await;
            inner.displays.is_empty() && inner.table.renderer_ids().len() == 1
        };
        if lone_renderer_no_displays {
            self.schedule_orphan_grace(renderer_id).await;
        } else {
            self.kill_orphan_now(&renderer_id).await;
        }
    }

    async fn schedule_orphan_grace(self: &Arc<Self>, renderer_id: RendererId) {
        let weak = Arc::downgrade(self);
        let rid_for_task = renderer_id.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(ORPHAN_REAP_TIMEOUT).await;
            let Some(this) = weak.upgrade() else { return };
            this.fire_orphan_reap(&rid_for_task).await;
        });
        let mut inner = self.inner.lock().await;
        if let Some(prev) = inner.orphan_timers.insert(renderer_id.clone(), task) {
            prev.abort();
        }
        log::debug!(
            "router: orphan timer scheduled for {renderer_id} ({:?})",
            ORPHAN_REAP_TIMEOUT
        );
    }

    async fn kill_orphan_now(self: &Arc<Self>, renderer_id: &str) {
        log::info!("router: reaping orphan renderer {renderer_id} immediately");
        self.unregister_renderer(renderer_id).await;
        if let Err(e) = self.mgr.kill(renderer_id).await {
            log::warn!("router: kill orphan {renderer_id}: {e}");
        }
    }

    /// Cancel a pending orphan-reap timer for `renderer_id` (if any).
    /// Called from `register_display` / link-success paths so a
    /// renderer that just re-acquired an audience survives.
    pub async fn cancel_orphan_timer(self: &Arc<Self>, renderer_id: &str) {
        let removed = self.inner.lock().await.orphan_timers.remove(renderer_id);
        if let Some(task) = removed {
            task.abort();
            log::debug!("router: orphan timer cancelled for {renderer_id}");
        }
    }

    /// Timer body: re-check the orphan condition under the lock and
    /// kill if it still holds. Always clears the timer entry from the
    /// map before unregistering (which itself touches the lock).
    async fn fire_orphan_reap(self: &Arc<Self>, renderer_id: &str) {
        let still_orphan = {
            let mut inner = self.inner.lock().await;
            // Drop our own entry first so a concurrent re-mark sees an
            // empty slot and schedules a fresh timer.
            inner.orphan_timers.remove(renderer_id);
            // Renderer might have been removed via `unregister_renderer`
            // already (manual kill, etc.) — bail in that case.
            if !inner.table.renderer_ids().iter().any(|r| r == renderer_id) {
                return;
            }
            inner
                .table
                .links_for_renderer(renderer_id)
                .iter()
                .all(|l| !l.enabled)
        };
        if !still_orphan {
            return;
        }
        log::info!("router: reaping orphan renderer {renderer_id} after grace");
        self.unregister_renderer(renderer_id).await;
        if let Err(e) = self.mgr.kill(renderer_id).await {
            log::warn!("router: kill orphan {renderer_id}: {e}");
        }
    }

    /// Fire an event to all subscribers. Send errors (no subscribers)
    /// are downgraded to debug logs.
    pub fn emit(&self, evt: RouterEvent) {
        if let Err(e) = self.events_tx.send(evt) {
            log::debug!("router: no event subscribers ({e})");
        }
    }

    /// Snapshot of a single display by id. Returns `None` if the
    /// display has been unregistered. Must not be called while the
    /// inner lock is held.
    pub async fn snapshot_display(self: &Arc<Self>, id: DisplayId) -> Option<DisplaySnapshot> {
        let inner = self.inner.lock().await;
        let s = inner.displays.get(&id)?;
        let links = inner
            .table
            .links_for_display(id)
            .into_iter()
            .filter(|l| l.enabled)
            .map(|l| DisplayLinkSnapshot {
                renderer_id: l.renderer_id,
                z_order: l.z_order,
            })
            .collect();
        Some(DisplaySnapshot {
            id,
            name: s.info.name.clone(),
            width: s.info.width,
            height: s.info.height,
            refresh_mhz: s.info.refresh_mhz,
            links,
            drm_render_major: s.gpu.major,
            drm_render_minor: s.gpu.minor,
        })
    }

    /// Snapshot of a single renderer by id. Returns `None` if the
    /// renderer has been unregistered from the routing table.
    pub async fn snapshot_renderer(self: &Arc<Self>, id: &str) -> Option<RendererSnapshot> {
        let inner = self.inner.lock().await;
        let handle = inner.table.get_renderer(id)?;
        let status = if inner.paused_renderers.contains(id) {
            RendererStatus::Paused
        } else {
            RendererStatus::Playing
        };
        let (tw, th) = handle.texture_size();
        Some(RendererSnapshot {
            id: handle.id.clone(),
            wp_type: handle.wp_type.clone(),
            name: handle.name.clone(),
            status,
            pid: handle.pid.unwrap_or(0),
            drm_render_major: handle.gpu.major,
            drm_render_minor: handle.gpu.minor,
            texture_width: tw,
            texture_height: th,
        })
    }

    /// Snapshot of every registered renderer, ordered by ascending id
    /// for UI stability. Pure read — does not touch renderer state or
    /// emit events.
    pub async fn snapshot_renderers(self: &Arc<Self>) -> Vec<RendererSnapshot> {
        let inner = self.inner.lock().await;
        let mut ids = inner.table.renderer_ids();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| {
                let handle = inner.table.get_renderer(&id)?;
                let status = if inner.paused_renderers.contains(&id) {
                    RendererStatus::Paused
                } else {
                    RendererStatus::Playing
                };
                let (tw, th) = handle.texture_size();
                Some(RendererSnapshot {
                    id: handle.id.clone(),
                    wp_type: handle.wp_type.clone(),
                    name: handle.name.clone(),
                    status,
                    pid: handle.pid.unwrap_or(0),
                    drm_render_major: handle.gpu.major,
                    drm_render_minor: handle.gpu.minor,
                    texture_width: tw,
                    texture_height: th,
                })
            })
            .collect()
    }

    /// Snapshot of every registered display plus the enabled links
    /// pointing at it, ordered by ascending id for UI stability.
    /// Pure read — does not touch renderer state or emit events.
    pub async fn snapshot_displays(self: &Arc<Self>) -> Vec<DisplaySnapshot> {
        let inner = self.inner.lock().await;
        let mut ids: Vec<DisplayId> = inner.displays.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| {
                let s = inner.displays.get(&id)?;
                let links = inner
                    .table
                    .links_for_display(id)
                    .into_iter()
                    .filter(|l| l.enabled)
                    .map(|l| DisplayLinkSnapshot {
                        renderer_id: l.renderer_id,
                        z_order: l.z_order,
                    })
                    .collect();
                Some(DisplaySnapshot {
                    id,
                    name: s.info.name.clone(),
                    width: s.info.width,
                    height: s.info.height,
                    refresh_mhz: s.info.refresh_mhz,
                    links,
                    drm_render_major: s.gpu.major,
                    drm_render_minor: s.gpu.minor,
                })
            })
            .collect()
    }

    /// Emit a `LibraryUpsert` event so subscribers (UI) refresh their
    /// view. The router no longer caches libraries — the DB is the
    /// source of truth; callers query it directly when they need the
    /// full list (see `control::list_library_snapshots`).
    pub fn upsert_library(self: &Arc<Self>, snap: LibrarySnapshot) {
        self.emit(RouterEvent::LibraryUpsert(snap));
    }

    pub fn remove_library(self: &Arc<Self>, id: i64) {
        self.emit(RouterEvent::LibraryRemoved(id));
    }

    // ---------------------------------------------------------------
    // Routing policy
    // ---------------------------------------------------------------

    /// Return the renderers whose every enabled display link is
    /// covered by `target` — i.e. the renderers that an imminent
    /// `relink_displays_to(target, …)` (or `relink_all_displays_to`
    /// when `target` is `None`) would leave with zero enabled links.
    ///
    /// `WallpaperApply` uses this to stop the soon-to-be-orphaned
    /// renderers *before* spawning the new one, capping VRAM peak at
    /// one renderer's working set instead of two.
    pub async fn renderers_fully_replaced_by(
        self: &Arc<Self>,
        target: Option<&[DisplayId]>,
    ) -> Vec<RendererId> {
        let inner = self.inner.lock().await;
        inner
            .table
            .renderer_ids()
            .into_iter()
            .filter(|rid| {
                let links = inner.table.links_for_renderer(rid);
                let enabled: Vec<_> = links.iter().filter(|l| l.enabled).collect();
                if enabled.is_empty() {
                    // Already orphaned (no enabled links). Counts as
                    // "fully replaced" so the caller folds it into
                    // the same pre-spawn cleanup pass.
                    return true;
                }
                match target {
                    None => true, // relink_all replaces every display
                    Some(ts) => enabled.iter().all(|l| ts.contains(&l.display_id)),
                }
            })
            .collect()
    }

    /// Synchronously unregister + kill each `id` in `ids`. Used by
    /// the apply path to drop pre-existing renderers that the new
    /// renderer is going to fully replace, before the new one is
    /// spawned.
    pub async fn stop_renderers(self: &Arc<Self>, ids: &[RendererId]) {
        for id in ids {
            self.unregister_renderer(id).await;
            if let Err(e) = self.mgr.kill(id).await {
                log::warn!("router: stop_renderers: kill {id}: {e}");
            }
        }
    }

    /// Re-point every enabled link to `new_renderer_id`. Used by
    /// `WallpaperApply` in single-wallpaper mode. Idempotent: calling
    /// twice with the same id is a no-op (the link already points
    /// there, sync_display sees no diff).
    /// Re-point the single enabled link of every display in
    /// `display_ids` at `new_renderer_id`. Displays not in the list
    /// keep their current renderer binding. Unknown display ids are
    /// skipped silently (callers are expected to validate upstream).
    pub async fn relink_displays_to(
        self: &Arc<Self>,
        display_ids: &[DisplayId],
        new_renderer_id: &str,
    ) {
        let applied: Vec<DisplayId> = {
            let mut inner = self.inner.lock().await;
            let mut out = Vec::with_capacity(display_ids.len());
            for did in display_ids {
                if !inner.displays.contains_key(did) {
                    continue;
                }
                let existing = inner.table.links_for_display(*did);
                for link in existing {
                    inner.table.remove_link(link.id);
                }
                inner.table.add_link(new_renderer_id.to_string(), *did);
                out.push(*did);
            }
            out
        };
        for did in &applied {
            self.sync_display(*did).await;
        }
        self.reconcile_lifecycle().await;
        // See `relink_all_displays_to` for the GC rationale. We always
        // run the mark pass so that switching one display away from a
        // renderer that no other display still uses starts the orphan
        // grace timer immediately.
        self.mark_orphans(Some(new_renderer_id)).await;
        self.reconcile_buffer_flags().await;
        if !applied.is_empty() {
            let all = self.snapshot_displays().await;
            self.emit(RouterEvent::DisplaysReplace(all));
        }
    }

    pub async fn relink_all_displays_to(self: &Arc<Self>, new_renderer_id: &str) {
        let display_ids: Vec<DisplayId> = {
            let mut inner = self.inner.lock().await;
            let ids: Vec<DisplayId> = inner.displays.keys().copied().collect();
            for did in &ids {
                let existing = inner.table.links_for_display(*did);
                for link in existing {
                    inner.table.remove_link(link.id);
                }
                inner.table.add_link(new_renderer_id.to_string(), *did);
            }
            ids
        };
        let had_ids = !display_ids.is_empty();
        for did in display_ids {
            self.sync_display(did).await;
        }
        self.reconcile_lifecycle().await;
        // Active GC: any renderer that is no longer referenced by any
        // display gets a 5s reap timer scheduled. The new renderer is
        // preserved by id (its own timer cancelled if pending) even if
        // no displays were affected (0-display apply path).
        self.mark_orphans(Some(new_renderer_id)).await;
        self.reconcile_buffer_flags().await;
        if had_ids {
            let all = self.snapshot_displays().await;
            self.emit(RouterEvent::DisplaysReplace(all));
        }
    }

    /// Mutate a link's geometry/clear color and re-emit `SetConfig` to
    /// the affected display. Sends only `SetConfig` (no Bind/Unbind):
    /// the buffer pool is unchanged, only the composition geometry.
    /// Returns `true` if the link existed and any field was updated.
    pub async fn set_link_geometry(
        self: &Arc<Self>,
        link_id: LinkId,
        src: Option<LinkSrcRect>,
        dst: Option<LinkDstRect>,
        transform: Option<u32>,
        clear_rgba: Option<[f32; 4]>,
        z_order: Option<i32>,
    ) -> bool {
        let payload: Option<(DisplayId, ProjectedConfig)> = {
            let mut inner = self.inner.lock().await;
            let changed = inner
                .table
                .update_link_geometry(link_id, src, dst, transform, clear_rgba, z_order);
            if !changed {
                return false;
            }
            let Some(link) = inner.table.get_link(link_id).cloned() else {
                return false;
            };
            let Some(renderer) = inner.table.get_renderer(&link.renderer_id) else {
                return false;
            };
            let (info, bound_to_this) = match inner.displays.get(&link.display_id) {
                Some(state) => (
                    state.info.clone(),
                    state.last_renderer.as_deref() == Some(link.renderer_id.as_str()),
                ),
                None => return false,
            };
            if !bound_to_this {
                return true;
            }
            inner.next_config_generation += 1;
            let cfg_gen = inner.next_config_generation;
            let layout = self.resolved_layout(&info);
            let cfg = project_link(&link, &renderer, &info, cfg_gen, &layout);
            Some((link.display_id, cfg))
        };
        let affected_display = payload.as_ref().map(|(d, _)| *d);
        if let Some((did, cfg)) = payload {
            let inner = self.inner.lock().await;
            if let Some(state) = inner.displays.get(&did) {
                let _ = state.tx.send(DisplayOutEvent::SetConfig(cfg));
            }
        }
        if let Some(did) = affected_display {
            if let Some(snap) = self.snapshot_display(did).await {
                self.emit(RouterEvent::DisplayUpsert(snap));
            }
        }
        true
    }

    // ---------------------------------------------------------------
    // Internal — renderer event handlers and sync core
    // ---------------------------------------------------------------

    async fn on_renderer_bind(self: &Arc<Self>, renderer_id: &str) {
        let display_ids: Vec<DisplayId> = {
            let inner = self.inner.lock().await;
            inner
                .table
                .links_for_renderer(renderer_id)
                .into_iter()
                .filter(|l| l.enabled)
                .map(|l| l.display_id)
                .collect()
        };
        for did in display_ids {
            self.sync_display(did).await;
        }
        // The first BindBuffers exposes the renderer's flags so the
        // router can compare against the consumer set; subsequent ones
        // (after a ConfigureBuffers) clear pending_configure inside
        // the reader thread before this hook fires, so a re-evaluation
        // here will compute a fresh diff if the topology meanwhile
        // shifted again.
        self.reconcile_buffer_flags().await;
        // BindBuffers is also when the renderer's actual texture dims
        // become known; push a fresh snapshot so the UI flips from the
        // spawn-time hint to the real resolution.
        if let Some(snap) = self.snapshot_renderer(renderer_id).await {
            self.emit(RouterEvent::RendererUpsert(snap));
        }
    }

    async fn on_renderer_frame(
        self: &Arc<Self>,
        renderer_id: &str,
        buffer_index: u32,
        seq: u64,
        release_point: u64,
    ) {
        let inner = self.inner.lock().await;
        let Some(renderer) = inner.table.get_renderer(renderer_id) else {
            return;
        };
        let gen = renderer
            .bind_snapshot()
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.generation));
        let Some(gen) = gen else { return };

        // First pass: collect every display that should get this frame
        // so we can pre-compute fan-out width — the reaper needs it to
        // know how many consumer FrameRecords to wait for at this
        // `release_point` before TRANSFERing.
        //
        // The `last_buffer_generation == Some(gen)` clause is what
        // primarily holds frames back during a v2 renegotiation — the
        // gate in `sync_display` prevents a Bind being pushed until
        // `bind_snapshot` matches the dispatched scheme, so
        // `last_buffer_generation` stays on the prior gen until the
        // gate opens and a fresh Bind goes out. Once the gate opens
        // both fields converge and frames flow.
        let recipients: Vec<&DisplayState> = inner
            .table
            .links_for_renderer(renderer_id)
            .into_iter()
            .filter(|link| link.enabled)
            .filter_map(|link| inner.displays.get(&link.display_id))
            .filter(|state| {
                state.last_buffer_generation == Some(gen)
                    && state.last_renderer.as_deref() == Some(renderer_id)
            })
            .collect();
        let expected_count = recipients.len() as u32;
        if expected_count == 0 {
            // No enabled recipients: still hand the producer's release
            // timeline a synthetic signal at this point so its
            // back-pressure wait doesn't time out forever.
            if let Err(e) = renderer.submit_frame_record(crate::sync::FrameRecord {
                release_point,
                consumer_handle: None,
                expected_count: 0,
            }) {
                log::warn!(
                    "router: renderer {renderer_id}: failed to enqueue \
                     advance-only FrameRecord (point {release_point}): {e}"
                );
            }
            return;
        }
        for state in recipients {
            let _ = state.tx.send(DisplayOutEvent::Frame {
                renderer: renderer.clone(),
                buffer_generation: gen,
                buffer_index,
                seq,
                release_point,
                expected_count,
            });
        }
    }

    /// Compute the current Pause/Play diff and dispatch control
    /// messages outside the inner lock. Call after any mutation that
    /// can change a renderer's enabled-link count.
    async fn reconcile_lifecycle(self: &Arc<Self>) {
        let actions: Vec<(RendererId, ControlMsg)> = {
            let mut inner = self.inner.lock().await;
            let mut out = Vec::new();
            for rid in inner.table.renderer_ids() {
                let active = inner
                    .table
                    .links_for_renderer(&rid)
                    .iter()
                    .any(|l| l.enabled);
                let was_paused = inner.paused_renderers.contains(&rid);
                if active && was_paused {
                    inner.paused_renderers.remove(&rid);
                    inner.paused_since.remove(&rid);
                    out.push((rid, ControlMsg::Play));
                } else if !active && !was_paused {
                    inner.paused_renderers.insert(rid.clone());
                    inner.paused_since.insert(rid.clone(), Instant::now());
                    out.push((rid, ControlMsg::Pause));
                }
            }
            out
        };
        let changed_ids: Vec<RendererId> = actions.iter().map(|(id, _)| id.clone()).collect();
        for (id, msg) in actions {
            let label = match msg {
                ControlMsg::Pause => "pause",
                ControlMsg::Play => "play",
                _ => "ctl",
            };
            if let Err(e) = self.mgr.send_control(&id, msg).await {
                log::warn!("router: {label} {id}: {e}");
            } else {
                log::info!("router: {label} renderer {id} (ref_count diff)");
            }
        }
        for id in changed_ids {
            if let Some(snap) = self.snapshot_renderer(&id).await {
                self.emit(RouterEvent::RendererUpsert(snap));
            }
        }
    }

    /// Re-run the modifier picker for every (renderer, display) link
    /// the router knows about. Iter 2 dispatch policy: this is
    /// observation-only — it logs the chosen scheme (or the reason a
    /// pick failed) but does NOT yet send `negotiate_buffers` to
    /// renderers. Iter 3 wires the dispatch.
    ///
    /// Pairs where either side hasn't yet shipped its caps are
    /// skipped silently (legacy v1 path). Once Iter 2 lands the
    /// producer/consumer probes, every pair will go through
    /// `negotiate::pick`.
    ///
    /// Call after any topology mutation (display add/remove, link
    /// change, renderer bind, caps update).
    async fn reconcile_buffer_flags(self: &Arc<Self>) {
        // Snapshot under the inner lock: (rid, did, producer_caps,
        // consumer_caps). pick() is pure, so we run it outside the
        // lock to keep the critical section small. Buffer extent is
        // intentionally NOT part of the snapshot — the renderer is
        // the producer, already resolved its render extent at
        // `advertise_caps` time, and the bridge sizes dmabuf slots
        // from that. The daemon learns the actual size through
        // `bind_buffers` and forwards it to consumers; nothing in
        // the format/modifier negotiation needs the extent.
        struct Pair {
            rid: RendererId,
            did: DisplayId,
            producer: crate::dma::negotiate::PeerCaps,
            consumer: crate::dma::negotiate::PeerCaps,
        }
        let pairs: Vec<Pair> = {
            let inner = self.inner.lock().await;
            let mut out = Vec::new();
            for rid in inner.table.renderer_ids() {
                let Some(renderer) = inner.table.get_renderer(&rid) else {
                    continue;
                };
                let Some(producer_caps) = renderer.format_caps() else {
                    continue; // legacy renderer — skip silently
                };
                for link in inner.table.links_for_renderer(&rid) {
                    if !link.enabled {
                        continue;
                    }
                    let Some(state) = inner.displays.get(&link.display_id) else {
                        continue;
                    };
                    let Some(consumer_caps) = state.consumer_caps.clone() else {
                        continue; // legacy consumer — skip silently
                    };
                    out.push(Pair {
                        rid: rid.clone(),
                        did: link.display_id,
                        producer: producer_caps.clone(),
                        consumer: consumer_caps,
                    });
                }
            }
            out
        };
        // Iter 3a: dispatch the picked scheme via NegotiateBuffers.
        // For multi-display fan-out the picker still runs per (renderer,
        // display) pair, but we only want to dispatch ONCE per renderer
        // per reconcile pass — collapse pairs by renderer, picking the
        // most-recently-computed scheme. (TODO: when fan-out picks
        // diverge, the daemon should pick the most restrictive scheme
        // covering all consumers; for the prototype "last write wins"
        // is fine because layer-shell + waywallen-display lib advertise
        // identical hardcoded LINEAR caps.)
        let mut by_renderer: std::collections::HashMap<
            RendererId,
            crate::dma::negotiate::NegotiatedScheme,
        > = std::collections::HashMap::new();
        for p in pairs {
            match crate::dma::negotiate::pick(&p.producer, &p.consumer) {
                Ok(scheme) => {
                    log::info!(
                        "router: pick({rid}, display {did}) = \
                         path={path:?} mem_source={ms:?} \
                         fourcc=0x{fourcc:08x} modifier=0x{modifier:x} \
                         plane_count={pc} sync=0x{sync:x} color=0x{color:x} \
                         mem_hint=0x{mem:x} count={count}",
                        rid = p.rid,
                        did = p.did,
                        path = scheme.path,
                        ms = scheme.mem_source,
                        fourcc = scheme.fourcc,
                        modifier = scheme.modifier,
                        pc = scheme.plane_count,
                        sync = scheme.sync_mode,
                        color = scheme.color,
                        mem = scheme.mem_hint,
                        count = scheme.count,
                    );
                    by_renderer.insert(p.rid.clone(), scheme);
                }
                Err(e) => {
                    log::warn!(
                        "router: pick({rid}, display {did}) failed: {e:?}",
                        rid = p.rid,
                        did = p.did,
                    );
                }
            }
        }
        // Outside the inner lock — send_negotiate_buffers takes its own.
        for (rid, scheme) in by_renderer {
            if let Err(e) = self.mgr.send_negotiate_buffers(&rid, scheme).await {
                log::warn!("router: NegotiateBuffers {rid}: {e}");
            }
        }
    }

    /// Bring `display_id`'s sent state in line with its current link
    /// target (renderer + generation). Idempotent.
    async fn sync_display(self: &Arc<Self>, display_id: DisplayId) {
        let mut inner = self.inner.lock().await;
        if !inner.displays.contains_key(&display_id) {
            return;
        }
        // Compute target (link + renderer + generation) under immutable borrows.
        let display_links = inner.table.links_for_display(display_id);
        debug_assert!(
            display_links.iter().filter(|l| l.enabled).count() <= 1,
            "display {display_id} has multiple enabled links — invariant violated"
        );
        let target: Option<(Link, Arc<RendererHandle>, u64)> =
            display_links.into_iter().find(|l| l.enabled).and_then(|l| {
                let renderer = inner.table.get_renderer(&l.renderer_id)?;
                let gen = renderer
                    .bind_snapshot()
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.generation))?;
                Some((l, renderer, gen))
            });

        // Modifier-negotiation gate (Iter 3 step 1): when both producer
        // and consumer ship v2 caps, the renderer's bind_snapshot must
        // match the daemon's last-dispatched `NegotiatedScheme` before
        // a Bind is fanned out. Frames are silently held back until
        // the renderer answers `negotiate_buffers` with a matching
        // `bind_buffers`.
        //
        // Skipping is conservative: we leave `last_renderer` /
        // `last_buffer_generation` untouched so the consumer keeps its
        // previous bind on screen rather than getting an Unbind during
        // the brief renegotiation window. The follow-up call from
        // `on_renderer_bind` (when the matching snapshot arrives)
        // re-enters this function with the gate open and finishes the
        // transition.
        if let Some((_, ref renderer, _)) = target {
            let state = inner.displays.get(&display_id).unwrap();
            let v2_both = renderer.format_caps().is_some() && state.consumer_caps.is_some();
            if v2_both && !renderer.scheme_satisfied() {
                log::debug!(
                    "router: sync_display({display_id}) gated — renderer {} \
                     bind_snapshot does not yet match last-dispatched scheme",
                    renderer.id
                );
                return;
            }
        }

        // Snapshot what was last sent.
        let (last_renderer, last_gen, info) = {
            let s = inner.displays.get(&display_id).unwrap();
            (
                s.last_renderer.clone(),
                s.last_buffer_generation,
                s.info.clone(),
            )
        };

        let needs_update = match (&last_renderer, last_gen, &target) {
            (Some(or), Some(og), Some((link, _, ng))) => or != &link.renderer_id || og != *ng,
            (None, None, None) => false,
            _ => true,
        };
        if !needs_update {
            return;
        }

        // Phase A: retire the prior pool (if any).
        if let Some(og) = last_gen {
            let s = inner.displays.get(&display_id).unwrap();
            let _ = s.tx.send(DisplayOutEvent::Unbind {
                buffer_generation: og,
            });
        }

        // Phase B: bind the new pool (if any).
        if let Some((link, renderer, new_g)) = target {
            inner.next_config_generation += 1;
            let cfg_gen = inner.next_config_generation;
            let layout = self.resolved_layout(&info);
            let cfg = project_link(&link, &renderer, &info, cfg_gen, &layout);
            let new_r = link.renderer_id.clone();
            let s = inner.displays.get_mut(&display_id).unwrap();
            let _ = s.tx.send(DisplayOutEvent::Bind {
                renderer: renderer.clone(),
            });
            let _ = s.tx.send(DisplayOutEvent::SetConfig(cfg));
            s.last_renderer = Some(new_r);
            s.last_buffer_generation = Some(new_g);
        } else {
            let s = inner.displays.get_mut(&display_id).unwrap();
            s.last_renderer = None;
            s.last_buffer_generation = None;
        }
    }
}

/// Resolve a `Link`'s geometry into a wire-ready `ProjectedConfig`.
///
/// Two paths:
///
/// 1. If both rects are the `FULL_SRC`/`FULL_DST` sentinels (the
///    common case — Phase 1 auto-link, no explicit per-link geometry),
///    delegate to `display::layout::compute()` so the user-configured
///    fillmode/align takes effect.
/// 2. If either rect is explicit, that explicit geometry wins for
///    all four fields (preserves the future per-link composition
///    use case where geometry is set deliberately and shouldn't be
///    overridden by per-display preferences).
///
/// The `link.clear_rgba` always wins over the resolved layout's
/// clear color when the link sets a non-default value — but right
/// now `add_link` always seeds [0,0,0,1] and there's no API to
/// change it per-link, so in practice the layout's clear color is
/// what the user sees.
fn project_link(
    link: &Link,
    renderer: &Arc<RendererHandle>,
    info: &DisplayInfo,
    config_generation: u64,
    layout: &ResolvedLayout,
) -> ProjectedConfig {
    let src_full = link.src_rect == super::table::FULL_SRC;
    let dst_full = link.dst_rect == super::table::FULL_DST;

    if src_full && dst_full {
        let (tex_w, tex_h) = renderer.texture_size();
        let out = crate::display::layout::compute(LayoutInput {
            tex_w: tex_w as f32,
            tex_h: tex_h as f32,
            disp_w: info.width as f32,
            disp_h: info.height as f32,
            fillmode: layout.fillmode,
            align: layout.align,
            clear_rgba: link.clear_rgba,
        });
        return ProjectedConfig {
            config_generation,
            source_x: out.source.0,
            source_y: out.source.1,
            source_w: out.source.2,
            source_h: out.source.3,
            dest_x: out.dest.0,
            dest_y: out.dest.1,
            dest_w: out.dest.2,
            dest_h: out.dest.3,
            transform: link.transform,
            clear_rgba: out.clear_rgba,
        };
    }

    // Explicit per-link geometry: keep the legacy resolve-sentinels
    // path. Falls through here when an integration test or future
    // multi-link composition has set explicit rects on the Link.
    let (rtex_w, rtex_h) = renderer.texture_size();
    let resolve_src = |r: LinkSrcRect| -> (f32, f32, f32, f32) {
        let w = if r.w.is_infinite() {
            rtex_w as f32
        } else {
            r.w
        };
        let h = if r.h.is_infinite() {
            rtex_h as f32
        } else {
            r.h
        };
        (r.x, r.y, w, h)
    };
    let resolve_dst = |r: LinkDstRect| -> (f32, f32, f32, f32) {
        let w = if r.w.is_infinite() {
            info.width as f32
        } else {
            r.w
        };
        let h = if r.h.is_infinite() {
            info.height as f32
        } else {
            r.h
        };
        (r.x, r.y, w, h)
    };
    let (sx, sy, sw, sh) = resolve_src(link.src_rect);
    let (dx, dy, dw, dh) = resolve_dst(link.dst_rect);
    ProjectedConfig {
        config_generation,
        source_x: sx,
        source_y: sy,
        source_w: sw,
        source_h: sh,
        dest_x: dx,
        dest_y: dy,
        dest_w: dw,
        dest_h: dh,
        transform: link.transform,
        clear_rgba: link.clear_rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer_manager::RendererManager;

    fn reg(name: &str, w: u32, h: u32) -> DisplayRegistration {
        DisplayRegistration {
            name: name.into(),
            instance_id: None,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            gpu: DrmNode::UNKNOWN,
            properties: vec![],
            consumer_caps: None,
        }
    }

    #[tokio::test]
    async fn snapshot_displays_empty() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr);
        assert!(router.snapshot_displays().await.is_empty());
    }

    #[tokio::test]
    async fn snapshot_displays_sorted_by_id_with_metadata() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr);

        // register_display has no registered renderer, so no auto-link —
        // each display shows up with an empty link vector.
        let _h1 = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        let _h2 = router.register_display(reg("DP-1", 2560, 1440)).await;
        let _h3 = router.register_display(reg("eDP-1", 1366, 768)).await;

        let snap = router.snapshot_displays().await;
        assert_eq!(snap.len(), 3);

        // Stable ascending ordering by id — matches register order here.
        let ids: Vec<u64> = snap.iter().map(|d| d.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);

        // Metadata round-trips unchanged.
        assert_eq!(snap[0].name, "HDMI-A-1");
        assert_eq!((snap[0].width, snap[0].height), (1920, 1080));
        assert_eq!(snap[1].name, "DP-1");
        assert_eq!((snap[1].width, snap[1].height), (2560, 1440));
        assert_eq!(snap[2].name, "eDP-1");
        assert_eq!((snap[2].width, snap[2].height), (1366, 768));

        // No renderers registered → every link vector is empty.
        for d in &snap {
            assert!(
                d.links.is_empty(),
                "display {} unexpectedly has links",
                d.id
            );
        }
    }

    #[tokio::test]
    async fn snapshot_reflects_display_unregister() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr);

        let h1 = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        let h2 = router.register_display(reg("DP-1", 2560, 1440)).await;
        assert_eq!(router.snapshot_displays().await.len(), 2);

        router.unregister_display(h1.id).await;
        let snap = router.snapshot_displays().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, h2.id);
        assert_eq!(snap[0].name, "DP-1");
    }

    // -----------------------------------------------------------------
    // M8 — orphan reaping
    // -----------------------------------------------------------------

    /// Register a stub renderer with both the manager and the router
    /// so apply-side lookups (`mgr.kill`, `table.get_renderer`) both
    /// succeed.
    async fn add_stub_renderer(mgr: &Arc<RendererManager>, router: &Arc<Router>, id: &str) {
        let h = RendererHandle::test_stub(id, "scene");
        mgr.register_test_handle(h.clone()).await;
        router.register_renderer(h).await;
    }

    /// Are these ids still in the manager's live list?
    async fn live_renderers(mgr: &Arc<RendererManager>) -> Vec<RendererId> {
        let mut ids = mgr.list().await;
        ids.sort();
        ids
    }

    /// Yield enough times that any spawned task chains awaiting on
    /// inner-lock + spawn_blocking + child-wait paths can complete.
    /// `tokio::time::advance` already yields once but the orphan reap
    /// chain requires additional polls to finish `mgr.kill` (which
    /// uses `spawn_blocking` whose JoinHandle resolves out-of-band).
    async fn drain_executor() {
        for _ in 0..256 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn renderers_fully_replaced_by_target_subset() {
        // r1 binds {A, B}, r2 binds {C}. relink target {A, B}: r1 is
        // fully replaced (every link in target), r2 is not. relink
        // target None (== all): both are fully replaced.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        add_stub_renderer(&mgr, &router, "r2").await;
        let a = router.register_display(reg("A", 1920, 1080)).await;
        let b = router.register_display(reg("B", 1920, 1080)).await;
        let c = router.register_display(reg("C", 1920, 1080)).await;
        // Initial auto-link picks the first renderer ("r1") for every
        // display. Move C onto r2.
        router.relink_displays_to(&[c.id], "r2").await;
        drain_executor().await;
        // After this point the table is: r1 ↔ {A, B}, r2 ↔ {C}.

        let mut killable = router
            .renderers_fully_replaced_by(Some(&[a.id, b.id]))
            .await;
        killable.sort();
        assert_eq!(
            killable,
            vec!["r1".to_string()],
            "only r1's enabled links are within {{A,B}}",
        );

        let mut all = router.renderers_fully_replaced_by(None).await;
        all.sort();
        assert_eq!(
            all,
            vec!["r1".to_string(), "r2".to_string()],
            "target=None means relink_all → every renderer gets fully replaced",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_renderers_unregisters_and_kills() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        add_stub_renderer(&mgr, &router, "r2").await;
        router.stop_renderers(&["r1".to_string()]).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r2".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn reap_kills_orphan_after_relink_all() {
        // Single display starts on r1; relink_all → r2 must reap r1
        // immediately. The grace window is reserved for the lone-
        // renderer-no-displays case (display hot-replug); when there
        // are 1+ displays or 2+ renderers the orphan dies right away.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        add_stub_renderer(&mgr, &router, "r2").await;

        let _h = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        // r1 was registered first → first_renderer() picked it for the auto-link.
        router.relink_all_displays_to("r2").await;
        drain_executor().await;

        let live = live_renderers(&mgr).await;
        assert_eq!(
            live,
            vec!["r2".to_string()],
            "r1 must be reaped immediately — display present, so no grace"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reap_keeps_renderer_still_referenced() {
        // Two displays both on r1. Relink only display A → r2; r1 must
        // survive because display B still uses it (no orphan timer
        // scheduled).
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        add_stub_renderer(&mgr, &router, "r2").await;

        let a = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        let _b = router.register_display(reg("DP-1", 1920, 1080)).await;

        router.relink_displays_to(&[a.id], "r2").await;
        drain_executor().await;
        // r1 is alive — display B still links it.
        let live = live_renderers(&mgr).await;
        assert_eq!(live, vec!["r1".to_string(), "r2".to_string()]);

        // Now move display B over too — r1 fully orphaned; reaped
        // immediately (displays present + 2 renderers → no grace).
        router.relink_all_displays_to("r2").await;
        drain_executor().await;
        let live = live_renderers(&mgr).await;
        assert_eq!(live, vec!["r2".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn relink_all_with_zero_displays_replaces_old_renderer() {
        // Apply path semantics with no displays attached:
        //   1. apply wp1 → r1 spawned and preserved (no displays to link).
        //   2. apply wp2 → r2 spawned; r1 is no longer the lone
        //      renderer (renderer_count == 2 at the mark moment) so
        //      the grace window does not apply — r1 dies immediately.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());

        // First apply: r1 spawn + relink_all (no displays).
        add_stub_renderer(&mgr, &router, "r1").await;
        router.relink_all_displays_to("r1").await;
        assert_eq!(live_renderers(&mgr).await, vec!["r1".to_string()]);

        // Second apply: r2 spawn + relink_all (still no displays).
        add_stub_renderer(&mgr, &router, "r2").await;
        router.relink_all_displays_to("r2").await;
        drain_executor().await;
        assert_eq!(
            live_renderers(&mgr).await,
            vec!["r2".to_string()],
            "r1 must be reaped immediately — 2 renderers means no grace",
        );
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert_eq!(
            live_renderers(&mgr).await,
            vec!["r2".to_string()],
            "r1 must be reaped after the orphan grace window",
        );

        // Third apply: same wallpaper as r2 → caller would `find_reusable`
        // and reuse r2; relink_all("r2") is a no-op + mark_orphans keeps r2.
        router.relink_all_displays_to("r2").await;
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r2".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn unregister_last_display_reaps_after_grace() {
        // After all displays unplug, the lone renderer enters the
        // orphan grace window. Within 5s a fresh display register
        // cancels the timer and keeps it alive; past 5s it dies.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        let h = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        assert_eq!(live_renderers(&mgr).await, vec!["r1".to_string()]);

        router.unregister_display(h.id).await;
        drain_executor().await;
        // Hot-replug within the window: timer cancelled, r1 lives on.
        tokio::time::advance(Duration::from_secs(4)).await;
        drain_executor().await;
        let h2 = router.register_display(reg("DP-1", 1920, 1080)).await;
        let snap = router.snapshot_displays().await;
        let entry = snap.iter().find(|d| d.id == h2.id).unwrap();
        assert_eq!(entry.links.len(), 1);
        assert_eq!(entry.links[0].renderer_id, "r1");
        tokio::time::advance(Duration::from_secs(2)).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r1".to_string()]);

        // Now unplug again and let the grace window elapse — r1 dies.
        router.unregister_display(h2.id).await;
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert!(
            live_renderers(&mgr).await.is_empty(),
            "renderer must be reaped past the orphan grace window",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn mark_preserves_keep_id_with_no_displays() {
        // 0-display: spawn r1 → it has no link, but `keep=Some("r1")`
        // protects it (no timer scheduled). Then spawn r2 and
        // mark_orphans(Some("r2")) schedules r1's timer; after the
        // grace window r1 is reaped and r2 lives on.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;
        let scheduled = router.mark_orphans(Some("r1")).await;
        assert!(scheduled.is_empty(), "keep id must not be marked");
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r1".to_string()]);

        add_stub_renderer(&mgr, &router, "r2").await;
        let scheduled = router.mark_orphans(Some("r2")).await;
        assert_eq!(scheduled, vec!["r1".to_string()]);
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r2".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn orphan_mark_then_cancel_keeps_renderer() {
        // Mark r1, advance 4s, cancel — r1 must outlive the original
        // 5s deadline.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;

        router.mark_orphan("r1".to_string()).await;
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        drain_executor().await;
        router.cancel_orphan_timer("r1").await;
        tokio::time::advance(Duration::from_secs(2)).await;
        drain_executor().await;
        assert_eq!(live_renderers(&mgr).await, vec!["r1".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn orphan_mark_fires_after_grace() {
        // Mark r1, advance past 5s — r1 must be reaped.
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "r1").await;

        router.mark_orphan("r1".to_string()).await;
        drain_executor().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        drain_executor().await;
        assert!(live_renderers(&mgr).await.is_empty());
    }

    // -----------------------------------------------------------------
    // Active-sync RouterEvent::Renderer* emission
    // -----------------------------------------------------------------

    async fn recv_event(rx: &mut broadcast::Receiver<RouterEvent>) -> Option<RouterEvent> {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Ok(ev)) => Some(ev),
            _ => None,
        }
    }

    #[tokio::test]
    async fn renderer_upsert_on_register() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        let mut rx = router.subscribe_events();

        add_stub_renderer(&mgr, &router, "R1").await;

        let evt = recv_event(&mut rx).await.expect("no event");
        match evt {
            RouterEvent::RendererUpsert(snap) => {
                assert_eq!(snap.id, "R1");
                assert_eq!(snap.wp_type, "scene");
                assert_eq!(snap.status, RendererStatus::Playing);
                assert_eq!(snap.name, "test-stub");
            }
            other => panic!("expected RendererUpsert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn renderer_removed_on_unregister() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        let mut rx = router.subscribe_events();

        add_stub_renderer(&mgr, &router, "R1").await;
        let _ = recv_event(&mut rx).await; // consume the RendererUpsert

        router.unregister_renderer("R1").await;
        let evt = recv_event(&mut rx).await.expect("no event");
        match evt {
            RouterEvent::RendererRemoved(id) => assert_eq!(id, "R1"),
            other => panic!("expected RendererRemoved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn renderer_upsert_on_pause_transition() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());

        add_stub_renderer(&mgr, &router, "R1").await;
        let display = router.register_display(reg("D1", 1920, 1080)).await;

        // Subscribe *after* setup so we only observe the unregister path.
        let mut rx = router.subscribe_events();

        router.unregister_display(display.id).await;

        let mut saw_paused = false;
        for _ in 0..6 {
            let Some(evt) = recv_event(&mut rx).await else {
                break;
            };
            if let RouterEvent::RendererUpsert(snap) = evt {
                if snap.id == "R1" && snap.status == RendererStatus::Paused {
                    saw_paused = true;
                    break;
                }
            }
        }
        assert!(
            saw_paused,
            "expected R1 Paused upsert after display unregister"
        );
    }

    // -----------------------------------------------------------------
    // Iter 5 — bind_failed + per-peer blacklist + retry
    // -----------------------------------------------------------------

    /// Build a single-fourcc PeerCaps with the given (modifier,plane_count) list.
    /// Mirrors `negotiate::tests::caps_one_fourcc` but in scope here.
    fn build_caps(fourcc: u32, mods: &[(u64, u32)], uuid_byte: u8) -> crate::dma::negotiate::PeerCaps {
        use crate::dma::negotiate as N;
        let mod_count = mods.len() as u32;
        let modifiers: Vec<u64> = mods.iter().map(|(m, _)| *m).collect();
        let plane_counts: Vec<u32> = mods.iter().map(|(_, p)| *p).collect();
        let dev_words = [u32::from_le_bytes([uuid_byte; 4]); 4];
        let drv_words = [u32::from_le_bytes([uuid_byte; 4]); 4];
        N::unflatten_caps(
            &[fourcc],
            &[mod_count],
            &modifiers,
            &plane_counts,
            &dev_words,
            &drv_words,
            DrmNode {
                major: 226,
                minor: 128,
            },
            N::SYNC_SYNCOBJ_TIMELINE,
            N::DEFAULT_COLOR,
            N::MEM_HINT_HOST_VISIBLE,
            (1920, 1080),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn consumer_bind_failed_inserts_blacklist() {
        // Wire a v2 consumer + producer, then push a BindFailed via
        // the router. The display's consumer_caps blacklist must
        // grow by one entry; reconcile_buffer_flags is called as a
        // side effect (we don't observe it directly here — covered
        // by the next test).
        use crate::dma::negotiate as N;
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        add_stub_renderer(&mgr, &router, "R1").await;
        let h = router.register_display(reg("D1", 1920, 1080)).await;

        let caps = build_caps(
            N::DRM_FORMAT_ABGR8888,
            &[(N::DRM_FORMAT_MOD_LINEAR, 1)],
            0xAA,
        );
        router.set_consumer_caps(h.id, caps).await;

        let nl: u64 = 0x0100_0000_0000_0001;
        router
            .on_consumer_bind_failed(h.id, N::DRM_FORMAT_ABGR8888, nl)
            .await;

        let inner = router.inner.lock().await;
        let state = inner.displays.get(&h.id).unwrap();
        let bl = &state.consumer_caps.as_ref().unwrap().blacklist;
        assert!(bl.contains(&(N::DRM_FORMAT_ABGR8888, nl)));
    }

    #[tokio::test]
    async fn renderer_bind_failed_inserts_blacklist() {
        // Same shape as the consumer test, but on the producer side.
        use crate::dma::negotiate as N;
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        let h = RendererHandle::test_stub("R1", "scene");
        let nl: u64 = 0x0100_0000_0000_0001;
        h.test_set_format_caps(build_caps(
            N::DRM_FORMAT_ABGR8888,
            &[(N::DRM_FORMAT_MOD_LINEAR, 1), (nl, 1)],
            0xAA,
        ));
        mgr.register_test_handle(h.clone()).await;
        router.register_renderer(h.clone()).await;

        assert_eq!(h.test_blacklist_len(), 0);
        router
            .on_renderer_bind_failed("R1", N::DRM_FORMAT_ABGR8888, nl)
            .await;
        assert_eq!(h.test_blacklist_len(), 1);
    }

    #[tokio::test]
    async fn picker_falls_back_after_consumer_blacklist() {
        // End-to-end: producer + consumer both advertise LINEAR + a
        // non-LINEAR modifier with a matching device UUID, so the
        // picker prefers non-LINEAR. Simulate the consumer rejecting
        // the non-LINEAR modifier; the picker must now fall back to
        // LINEAR.
        //
        // We can't observe `last_dispatched_scheme` because the test
        // stub's UnixStream peer end is closed (`_b` drops at the
        // end of `test_stub`), so `send_negotiate_buffers` returns
        // EPIPE and never records the scheme. Instead, drive the
        // picker directly with the post-mutation peer caps — that
        // proves the blacklist mutation reached the right `PeerCaps`.
        use crate::dma::negotiate as N;
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());

        let nl: u64 = 0x0100_0000_0000_0001;
        let h = RendererHandle::test_stub("R1", "scene");
        h.test_set_format_caps(build_caps(
            N::DRM_FORMAT_ABGR8888,
            &[(N::DRM_FORMAT_MOD_LINEAR, 1), (nl, 1)],
            0xAA,
        ));
        mgr.register_test_handle(h.clone()).await;
        router.register_renderer(h.clone()).await;

        let dh = router.register_display(reg("D1", 1920, 1080)).await;
        router
            .set_consumer_caps(
                dh.id,
                build_caps(
                    N::DRM_FORMAT_ABGR8888,
                    &[(N::DRM_FORMAT_MOD_LINEAR, 1), (nl, 1)],
                    0xAA,
                ),
            )
            .await;

        // Pre-blacklist pick must land on the non-LINEAR (same-device preference).
        {
            let inner = router.inner.lock().await;
            let prod = h.format_caps().expect("producer caps");
            let cons = inner.displays[&dh.id]
                .consumer_caps
                .clone()
                .expect("consumer caps");
            let s = N::pick(&prod, &cons).expect("pick ok");
            assert_eq!(s.modifier, nl, "pre-blacklist must prefer non-LINEAR");
        }

        // Consumer reports the non-LINEAR is unimportable.
        router
            .on_consumer_bind_failed(dh.id, N::DRM_FORMAT_ABGR8888, nl)
            .await;

        // Post-blacklist pick must fall back to LINEAR.
        let inner = router.inner.lock().await;
        let prod = h.format_caps().expect("producer caps");
        let cons = inner.displays[&dh.id]
            .consumer_caps
            .clone()
            .expect("consumer caps");
        let s = N::pick(&prod, &cons).expect("post-blacklist pick ok");
        assert_eq!(
            s.modifier,
            N::DRM_FORMAT_MOD_LINEAR,
            "after consumer blacklist, picker must fall back to LINEAR"
        );
    }

    // -----------------------------------------------------------------
    // project_link layout integration
    // -----------------------------------------------------------------

    fn make_link(rid: &str, did: DisplayId) -> Link {
        Link {
            id: 1,
            renderer_id: rid.to_string(),
            display_id: did,
            enabled: true,
            src_rect: super::super::table::FULL_SRC,
            dst_rect: super::super::table::FULL_DST,
            transform: 0,
            clear_rgba: [0.0, 0.0, 0.0, 1.0],
            z_order: 0,
        }
    }

    fn make_info(name: &str, w: u32, h: u32) -> DisplayInfo {
        DisplayInfo {
            id: 1,
            name: name.into(),
            instance_id: None,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            properties: vec![],
            bound: true,
        }
    }

    #[test]
    fn project_link_explicit_link_geometry_skips_layout() {
        // A link with explicit (non-sentinel) src/dst rects should
        // bypass display::layout::compute and pass the rects through
        // verbatim — even if the resolved layout wants something else.
        let renderer = RendererHandle::test_stub("r1", "scene");
        let info = make_info("eDP-1", 1280, 720);
        let mut link = make_link("r1", 1);
        link.src_rect = super::super::table::LinkSrcRect {
            x: 100.0,
            y: 200.0,
            w: 800.0,
            h: 600.0,
        };
        link.dst_rect = super::super::table::LinkDstRect {
            x: 50.0,
            y: 75.0,
            w: 400.0,
            h: 300.0,
        };
        link.clear_rgba = [1.0, 0.0, 0.0, 1.0];
        let layout = ResolvedLayout {
            // Even with PreserveAspectFit, explicit geometry must win.
            fillmode: FillMode::PreserveAspectFit,
            align: Default::default(),
        };
        let cfg = project_link(&link, &renderer, &info, 1, &layout);
        assert_eq!(
            (cfg.source_x, cfg.source_y, cfg.source_w, cfg.source_h),
            (100.0, 200.0, 800.0, 600.0)
        );
        assert_eq!(
            (cfg.dest_x, cfg.dest_y, cfg.dest_w, cfg.dest_h),
            (50.0, 75.0, 400.0, 300.0)
        );
        // Explicit clear color survives.
        assert_eq!(cfg.clear_rgba, [1.0, 0.0, 0.0, 1.0]);
    }

    // -----------------------------------------------------------------
    // update_display_size — Phase 3 resync
    // -----------------------------------------------------------------

    use crate::renderer_manager::BindSnapshot;

    fn fake_bind_snapshot(generation: u64, w: u32, h: u32) -> BindSnapshot {
        BindSnapshot {
            generation,
            flags: 0,
            count: 0,
            fourcc: 0x34325258, // XR24
            width: w,
            height: h,
            modifier: 0,
            planes_per_buffer: 1,
            stride: vec![],
            plane_offset: vec![],
            size: vec![],
            fds: vec![],
        }
    }

    /// Drain everything currently sitting on the rx and return only the
    /// last `SetConfig` payload — the one the consumer would actually
    /// observe after the wire flush.
    fn last_set_config(
        rx: &mut mpsc::UnboundedReceiver<DisplayOutEvent>,
    ) -> Option<ProjectedConfig> {
        let mut out = None;
        while let Ok(ev) = rx.try_recv() {
            if let DisplayOutEvent::SetConfig(c) = ev {
                out = Some(c);
            }
        }
        out
    }

    #[tokio::test]
    async fn update_display_size_resyncs_set_config() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());

        // Renderer with a bind snapshot so resync_display_set_config has
        // a generation to read and project_link gets the renderer's
        // tex dims.
        let r = RendererHandle::test_stub("r1", "scene"); // 1920x1080
        *r.bind_snapshot().lock().unwrap() = Some(fake_bind_snapshot(1, 1920, 1080));
        mgr.register_test_handle(r.clone()).await;
        router.register_renderer(r.clone()).await;

        // Register display 1920x1080 — auto-link + initial Bind/SetConfig.
        let mut h = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        let initial = last_set_config(&mut h.rx).expect("initial SetConfig");
        assert_eq!((initial.dest_w, initial.dest_h), (1920.0, 1080.0));

        // Resize to 1280x720 — Stretched + Center default → identity at new dims.
        router.update_display_size(h.id, 1280, 720).await;
        let resized = last_set_config(&mut h.rx).expect("SetConfig after resize");
        assert_eq!((resized.dest_x, resized.dest_y), (0.0, 0.0));
        assert_eq!((resized.dest_w, resized.dest_h), (1280.0, 720.0));
        assert!(resized.config_generation > initial.config_generation);
    }

    #[tokio::test]
    async fn update_display_size_same_dims_no_resync() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        let r = RendererHandle::test_stub("r1", "scene");
        *r.bind_snapshot().lock().unwrap() = Some(fake_bind_snapshot(1, 1920, 1080));
        mgr.register_test_handle(r.clone()).await;
        router.register_renderer(r.clone()).await;

        let mut h = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        // Drain initial events.
        let _ = last_set_config(&mut h.rx);

        router.update_display_size(h.id, 1920, 1080).await;
        // No new SetConfig should land on the rx.
        assert!(last_set_config(&mut h.rx).is_none());
    }

    #[tokio::test]
    async fn update_display_size_zero_dim_ignored() {
        let mgr = Arc::new(RendererManager::new_default());
        let router = Router::new(mgr.clone());
        let r = RendererHandle::test_stub("r1", "scene");
        *r.bind_snapshot().lock().unwrap() = Some(fake_bind_snapshot(1, 1920, 1080));
        mgr.register_test_handle(r.clone()).await;
        router.register_renderer(r.clone()).await;

        let mut h = router.register_display(reg("HDMI-A-1", 1920, 1080)).await;
        let _ = last_set_config(&mut h.rx);

        // Zero dim → drop on the floor; field stays at 1920x1080.
        router.update_display_size(h.id, 0, 720).await;
        router.update_display_size(h.id, 1280, 0).await;
        assert!(last_set_config(&mut h.rx).is_none());
        let snap = router.snapshot_display(h.id).await.unwrap();
        assert_eq!((snap.width, snap.height), (1920, 1080));
    }
}
