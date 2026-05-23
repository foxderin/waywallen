//! RendererManager — spawns and supervises `waywallen-renderer` child
//! processes, forwards control messages to them over Unix-domain sockets,
//! and parks their event stream into per-renderer broadcast channels.
//!
//! This module is the Rust daemon's counterpart to the C++ host program
//! in `open-wallpaper-engine/host/`.

use crate::error::{Error, Result, ResultExt};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, Weak as StdWeak};
use std::thread;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, Mutex as TokioMutex};
use uuid::Uuid;

use crate::ipc::proto::{ControlMsg, EventMsg};
use crate::ipc::uds::{recv_event, send_control, CodecError};

/// Spawn-time `Init` payload version the daemon currently emits. Bump
/// this when the wire shape of `ControlMsg::Init` changes; renderers
/// reply with `EventMsg::InitNack` if they don't recognise the value.
pub const SPAWN_VERSION: u32 = 4;
use crate::plugin::renderer_registry::{RendererDef, RendererRegistry};
use crate::routing::Router;
use crate::wallpaper_type::WallpaperType;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub type RendererId = String;

#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// The wallpaper type determines which renderer binary is spawned.
    pub wp_type: WallpaperType,
    /// CLI argv dictionary the daemon turns into `--<key> <value>`
    /// pairs after `--ipc <socket>`. Source plugins fill this via
    /// `extras(entry)`; `extras["path"]` is the canonical resource
    /// (mandatory). Plugin-specific keys (`assets`, `workshop_id`, …)
    /// must be ⊆ the renderer manifest's `extras` whitelist + `path`.
    pub extras: HashMap<String, String>,
    /// Plugin settings kv that flows directly into `Init.settings`.
    /// The caller is responsible for sourcing this — typically the
    /// reconciled per-plugin section of the daemon's settings store.
    /// Identity-tagged keys (per the manifest schema) gate reuse;
    /// non-identity keys can be hot-applied via `ApplySettings`.
    pub settings: HashMap<String, String>,
    /// Hint to the renderer for one or both render-target axes. `0` on
    /// either axis means "renderer fills this in from native". See
    /// `extent_mode` for the interpretation.
    pub width: u32,
    pub height: u32,
    /// Wire-level interpretation of `width`/`height`; values match
    /// `crate::settings::extent_mode::*` (and `ww_extent_mode_t` in
    /// the C bridge). `0` = `AS_GIVEN`.
    pub extent_mode: u32,
    /// When true, pass `--test-pattern` to the renderer host, which
    /// bypasses `SceneWallpaper::loadScene` and drives the offscreen
    /// ExSwapchain ring on a host-owned timer. Used to bring up the
    /// full daemon/display pipeline before a real Wallpaper Engine
    /// assets directory is available (see plan.md I4).
    pub test_pattern: bool,
    /// Optional explicit renderer plugin name. `None` (default) lets
    /// `spawn` and `find_reusable` pick the highest-priority renderer
    /// for `wp_type`; `Some(name)` pins both to that exact plugin so a
    /// user-chosen non-default renderer isn't transparently swapped
    /// for the priority winner on reuse or fresh spawn.
    pub renderer_name: Option<String>,
    /// Raw JSON dump of the DB row's `user_property_overrides` column
    /// (an object mapping project.json property keys to user-edited
    /// values). Forwarded verbatim through `Init.user_properties`; the
    /// renderer parses it once on startup. `None` / empty string when
    /// the wallpaper has no per-item overrides.
    pub user_properties_json: Option<String>,
}

/// Snapshot of the most recent `BindBuffers` event, plus the DMA-BUF FDs
/// the host attached to it. Owned by the manager; display endpoints will
/// `dup(2)` individual fds out of it when a new subscriber connects.
///
/// Multi-plane modifiers (e.g. AMD DCC where plane 0 = colour data and
/// plane 1 = compression metadata) flatten the per-plane info into the
/// `stride` / `plane_offset` / `size` / `fds` arrays. Each has length
/// `count * planes_per_buffer`, indexed
/// `[buffer_idx * planes_per_buffer + plane_idx]`. Single-plane modifiers
/// (LINEAR, plain tile-only) keep `planes_per_buffer = 1` and the arrays
/// have length `count`.
pub struct BindSnapshot {
    /// Monotonically increasing per-renderer pool generation. Sourced
    /// from the `bind_buffers.generation` field the renderer sets;
    /// propagated as `buffer_generation` on the display wire.
    pub generation: u64,
    /// Placement flag set the renderer used when allocating this pool.
    /// Bit 0 = host_visible (GTT). See `BUF_HOST_VISIBLE`.
    pub flags: u32,
    pub count: u32,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub planes_per_buffer: u32,
    /// `count * planes_per_buffer` entries, flattened (buffer, plane).
    pub stride: Vec<u32>,
    /// `count * planes_per_buffer` entries, flattened (buffer, plane).
    pub plane_offset: Vec<u32>,
    /// `count * planes_per_buffer` entries, flattened (buffer, plane).
    /// Per-plane memory span (`stride * height` for plane 0; for
    /// later planes the contribution between this and next plane's
    /// offset, or 0 if the renderer didn't compute it).
    pub size: Vec<u64>,
    /// `count * planes_per_buffer` entries, flattened (buffer, plane).
    /// For modifiers backed by a single dma-buf allocation, the
    /// renderer typically dups the same fd into every plane slot.
    pub fds: Vec<OwnedFd>,
}

/// Bit 0 of `BindSnapshot::flags` / `ControlMsg::ConfigureBuffers.flags`:
/// the renderer must back the dmabuf with HOST_VISIBLE memory (GTT/system
/// RAM) so it can be PRIME-imported by another GPU. Cleared means the
/// renderer is free to use DEVICE_LOCAL (VRAM) for zero-copy on same-GPU
/// consumers.
pub const BUF_HOST_VISIBLE: u32 = 1 << 0;

/// DRM render-node identity reported by a renderer in its `Ready` event.
/// `(0, 0)` is the sentinel for "renderer cannot resolve its render node",
/// in which case the daemon must conservatively assume cross-GPU paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DrmNode {
    pub major: u32,
    pub minor: u32,
}

impl DrmNode {
    pub const UNKNOWN: Self = Self { major: 0, minor: 0 };
    pub fn is_known(&self) -> bool {
        self.major != 0 || self.minor != 0
    }
}

/// Upper bound on the number of per-seq sync_fd entries the reader
/// keeps around before evicting the oldest. Renderers produce ~60 fps,
/// so 16 gives display clients ~250 ms to drain before fences start
/// dropping — plenty for a healthy event loop.
const SYNC_FD_RETENTION: usize = 16;

/// Per-renderer state. Cheap to clone via `Arc`; the inner fields are
/// shared across HTTP handlers and the reader thread.
pub struct RendererHandle {
    pub id: RendererId,
    pub wp_type: WallpaperType,
    pub width: u32,
    pub height: u32,
    /// Mirrors `SpawnRequest.extent_mode` so reuse can distinguish two
    /// requests that share the same `width`/`height` but disagree on
    /// the daemon's interpretation hint.
    pub extent_mode: u32,
    /// The `SpawnRequest.extras` this renderer was started with —
    /// canonical resource path + manifest-allowlisted keys
    /// (`assets`, `workshop_id`, …) that ride on CLI argv. This is
    /// the per-spawn identity differentiator: two SpawnRequests of
    /// the same plugin / wp_type / extent that disagree on `extras`
    /// MUST get different renderer processes (different `path` =
    /// different wallpaper). Settings, by contrast, are plugin-wide
    /// (`Settings::plugin(&name)`) and shared across all renderers
    /// of a plugin, so they don't differentiate.
    pub extras: HashMap<String, String>,
    /// Renderer plugin name from the resolved `RendererDef` (e.g.
    /// `"wescene"`). Surfaced to the UI so users see a friendly
    /// `<name>-<pid>` label instead of the opaque UUID.
    pub name: String,
    /// OS pid of the renderer child captured right after `spawn()`.
    /// `None` only if tokio could not return one (process already
    /// exited before id() was queried).
    pub pid: Option<u32>,
    /// DRM render-node id of the GPU the renderer's Vulkan instance
    /// picked. Reported in the renderer's `Ready` event. Used by the
    /// router to decide whether each subscribed display is on the same
    /// GPU (zero-copy) or a different GPU (must rebind via GTT). The
    /// sentinel `DrmNode::UNKNOWN` (0, 0) means the driver lacks
    /// `VK_EXT_physical_device_drm` and the daemon should assume
    /// cross-GPU.
    pub gpu: DrmNode,

    /// Blocking std UnixStream. Guarded by a std Mutex so HTTP handlers
    /// hold the lock only while a `sendmsg` is in flight; they spawn the
    /// actual send onto the blocking pool so the runtime isn't parked.
    sock: Arc<StdMutex<StdUnixStream>>,

    /// Broadcast of every event the host emits (besides the FDs on the
    /// initial BindBuffers — those are stored in `bind_snapshot` so
    /// late subscribers can dup them).
    events: broadcast::Sender<EventMsg>,

    /// Populated when the host sends its first `BindBuffers` event.
    bind_snapshot: Arc<StdMutex<Option<BindSnapshot>>>,

    /// In-flight `ConfigureBuffers` request. `Some(flags)` while the
    /// router has asked for a re-export and the renderer has not yet
    /// answered with a fresh `BindBuffers` whose `flags` matches; reset
    /// to `None` once the answering snapshot arrives. Guards the router
    /// from issuing a second reconfigure on top of an in-flight one.
    pending_configure: Arc<StdMutex<Option<u32>>>,

    /// Per-frame acquire fence file descriptors, indexed by `seq`.
    /// The reader thread stashes the `OwnedFd` that arrives with each
    /// `FrameReady { has_sync_fd: true }` event; the display endpoint
    /// consumes it (exactly once per seq) via `take_sync_fd`. Older
    /// entries are evicted once the map exceeds `SYNC_FD_RETENTION`.
    ///
    /// Phase 3b limitation: only one consumer gets the real fd per
    /// (seq). Multi-display real-sync fan-out will require a
    /// dup-on-take API.
    sync_fds: Arc<StdMutex<std::collections::VecDeque<(u64, OwnedFd)>>>,

    /// Producer-exported timeline drm_syncobj used as the release
    /// fence target. Populated by exactly one `ReleaseSyncobj` event
    /// the renderer subprocess emits between `Ready` and the first
    /// `FrameReady`. The fd is the OPAQUE_FD export of a Vulkan
    /// TIMELINE semaphore on the renderer's `VkDevice` (= a
    /// drm_syncobj on Mesa drivers); the reaper imports it via
    /// `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` and `TRANSFER`s consumer
    /// release fences onto each frame's `release_point`.
    release_syncobj: Arc<StdMutex<Option<OwnedFd>>>,

    /// Modifier-negotiation capabilities the producer declared in
    /// its `FormatCaps` event (sent once after `Ready`, before any
    /// `BindBuffers`). The router pairs this with each consumer's
    /// `consumer_caps` to compute a `NegotiatedScheme`. Stays `None`
    /// until the event arrives — older renderers that don't
    /// implement Iter 2 yet leave it empty, in which case the
    /// daemon skips negotiation for them and Iter 1 behavior
    /// (blind forward) prevails.
    format_caps: Arc<StdMutex<Option<crate::dma::negotiate::PeerCaps>>>,

    /// Last `NegotiatedScheme` the daemon dispatched via
    /// `NegotiateBuffers` to this renderer. Used for idempotence in
    /// `send_negotiate_buffers` — repeat calls with the same scheme
    /// short-circuit. `None` until the first dispatch.
    last_dispatched_scheme: Arc<StdMutex<Option<crate::dma::negotiate::NegotiatedScheme>>>,

    /// Sink for per-frame [`crate::sync::FrameRecord`]s. The display
    /// endpoint pushes one record per consumer per frame; the reaper
    /// task (spawned alongside this handle) drains them, waits for
    /// the consumer signal, and transfers the resulting fence onto
    /// the producer's release timeline. `Option` so test stubs can
    /// skip wiring the channel.
    frame_record_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::sync::FrameRecord>>,

    /// The child process. Kept alive so dropping the manager reaps it.
    child: Arc<TokioMutex<Option<Child>>>,

    /// Inbound-event family subscriptions copied from the renderer's
    /// manifest at spawn time. Pointer-event senders consult this to
    /// decide whether to encode (subscribed) or silently drop. Strings
    /// are validated against the recognised set in
    /// `RendererRegistry::scan`.
    events_subscribed: Arc<Vec<String>>,

    /// Renderer-published clear color (RGBA, 0..=1, sRGB straight
    /// alpha). Sole source of truth for the daemon's outbound display
    /// `set_config.clear_*` field. Default `[0, 0, 0, 1]` until the
    /// renderer emits a `ReportState { clear_color = "..." }`.
    clear_rgba: Arc<StdMutex<[f32; 4]>>,
}

impl RendererHandle {
    pub fn events(&self) -> broadcast::Receiver<EventMsg> {
        self.events.subscribe()
    }

    /// Borrow the cached bind snapshot. Returns `None` until the host's
    /// first frame has been rendered and the fds arrived.
    pub fn bind_snapshot(&self) -> Arc<StdMutex<Option<BindSnapshot>>> {
        Arc::clone(&self.bind_snapshot)
    }

    /// Actual texture dimensions reported by the renderer's most recent
    /// `BindBuffers`. Falls back to the spawn-time `(width, height)`
    /// hint until the first BindBuffers arrives — the spawn-time hint
    /// is just `Init.extent_w/h`, which after the renderer resolves
    /// it against the wallpaper's intrinsic size may not match the
    /// actual buffer dims.
    pub fn texture_size(&self) -> (u32, u32) {
        if let Ok(g) = self.bind_snapshot.lock() {
            if let Some(snap) = g.as_ref() {
                return (snap.width, snap.height);
            }
        }
        (self.width, self.height)
    }

    /// Current placement flags from the latest `BindBuffers`, or 0 if
    /// no snapshot has arrived yet. Used by the router to compare
    /// against the desired flag set.
    pub fn current_flags(&self) -> u32 {
        self.bind_snapshot
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.flags))
            .unwrap_or(0)
    }

    /// Whether a `ConfigureBuffers` request is currently in flight (sent
    /// to the renderer but not yet answered with a matching new
    /// `BindBuffers`). The router uses this to coalesce reconfigures.
    pub fn pending_configure(&self) -> Option<u32> {
        self.pending_configure.lock().ok().and_then(|g| *g)
    }

    /// Obtain a dup'd copy of the acquire sync_fd that arrived with
    /// `FrameReady` seq. Each caller gets an independent kernel
    /// reference to the same underlying `dma_fence` sync_file, so
    /// multiple display subscribers can each wait on (or import) the
    /// fence without interfering with one another.
    ///
    /// The original fd stays in the deque and is evicted only when
    /// the retention limit is hit (new frames push out old ones) or
    /// on a rebind.
    ///
    /// Returns `None` if the fd was never recorded (producer didn't
    /// export one) or has already been evicted (>SYNC_FD_RETENTION
    /// newer frames have arrived).
    pub fn clone_sync_fd(&self, seq: u64) -> Option<OwnedFd> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let guard = self.sync_fds.lock().ok()?;
        let (_, fd) = guard.iter().find(|(s, _)| *s == seq)?;
        let dup_raw = nix::unistd::dup(fd.as_raw_fd()).ok()?;
        // SAFETY: nix::unistd::dup returned a fresh fd we now own.
        Some(unsafe { OwnedFd::from_raw_fd(dup_raw) })
    }

    /// Borrow a dup'd handle to the producer's release timeline
    /// syncobj fd. Returns `None` until the `ReleaseSyncobj` event has
    /// arrived. The reaper uses this once per renderer (after import
    /// to a drm_syncobj handle the result is cached on the daemon
    /// side).
    pub fn clone_release_syncobj_fd(&self) -> Option<OwnedFd> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let guard = self.release_syncobj.lock().ok()?;
        let fd = guard.as_ref()?;
        let dup_raw = nix::unistd::dup(fd.as_raw_fd()).ok()?;
        Some(unsafe { OwnedFd::from_raw_fd(dup_raw) })
    }

    /// Borrow a clone of the producer's declared modifier-negotiation
    /// capabilities. Returns `None` until the `FormatCaps` event has
    /// arrived (or forever, for renderers that haven't been ported to
    /// Iter 2). The router calls this on every reconcile pass; it's
    /// cheap (cloning a HashMap of small structs).
    pub fn format_caps(&self) -> Option<crate::dma::negotiate::PeerCaps> {
        self.format_caps.lock().ok().and_then(|g| g.clone())
    }

    /// Mutate the producer's blacklist with `(fourcc, modifier)`. The
    /// blacklist lives inside the producer's [`PeerCaps`] and is
    /// consulted on every `negotiate::pick`. No-op if FormatCaps
    /// haven't arrived yet (legacy renderer).
    ///
    /// Returns `true` when the entry was newly inserted. The router
    /// uses the boolean to decide whether to re-run the picker (a
    /// duplicate insert means the renderer reported the same
    /// (fourcc, modifier) twice — already handled).
    pub fn blacklist_format(&self, fourcc: u32, modifier: u64) -> bool {
        let Ok(mut guard) = self.format_caps.lock() else {
            return false;
        };
        let Some(caps) = guard.as_mut() else {
            return false;
        };
        caps.blacklist.insert((fourcc, modifier))
    }

    /// Most recently dispatched [`crate::dma::negotiate::NegotiatedScheme`]
    /// for this renderer. `None` until the daemon has run a successful
    /// `pick` and called `send_negotiate_buffers`. Used by the router
    /// to gate `Bind`/`Frame` dispatch — frames are silently held
    /// until `bind_snapshot` matches the dispatched scheme.
    pub fn current_scheme(&self) -> Option<crate::dma::negotiate::NegotiatedScheme> {
        self.last_dispatched_scheme.lock().ok().and_then(|g| *g)
    }

    /// True iff the renderer's most recent `BindBuffers` snapshot
    /// matches the most recently dispatched [`crate::dma::negotiate::NegotiatedScheme`]
    /// on `(fourcc, modifier)`. Returns `false` if either side is
    /// missing — the gate stays closed until both arrive. Caller is
    /// responsible for ensuring v2 negotiation actually applies (i.e.
    /// both peers shipped caps); for legacy peers this method has no
    /// useful answer.
    pub fn scheme_satisfied(&self) -> bool {
        let Some(scheme) = self.current_scheme() else {
            return false;
        };
        let snap = self.bind_snapshot();
        let Ok(guard) = snap.lock() else {
            return false;
        };
        match guard.as_ref() {
            Some(s) => s.fourcc == scheme.fourcc && s.modifier == scheme.modifier,
            None => false,
        }
    }

    /// Push a per-frame [`crate::sync::FrameRecord`] to the reaper.
    /// The display endpoint calls this once per consumer per frame,
    /// after creating the consumer's binary release_syncobj. Returns
    /// `Err` if no reaper is wired (test_stub) or the channel was
    /// already closed (renderer evicted) — in either case the caller
    /// should drop the SyncobjHandle (which destroys the kernel
    /// object) and skip the frame.
    pub fn submit_frame_record(
        &self,
        record: crate::sync::FrameRecord,
    ) -> std::result::Result<(), &'static str> {
        let Some(tx) = self.frame_record_tx.as_ref() else {
            return Err("no reaper wired (test stub or unconfigured renderer)");
        };
        tx.send(record).map_err(|_| "reaper channel closed")
    }

    /// Renderer-published clear color (RGBA, 0..=1). Defaults to
    /// opaque black until the renderer emits its first `ReportState`
    /// with a `clear_color` key.
    pub fn clear_rgba(&self) -> [f32; 4] {
        self.clear_rgba
            .lock()
            .map(|g| *g)
            .unwrap_or([0.0, 0.0, 0.0, 1.0])
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct RendererManager {
    inner: TokioMutex<Inner>,
    /// Plugin registry mapping wallpaper types to renderer binaries.
    registry: RendererRegistry,
    /// Back-reference to the router, installed after construction via
    /// `attach_router`. Held weak to avoid a cycle with `Router::mgr`.
    /// Consulted on the crash path (`evict`) so a dead renderer gets
    /// unlinked from the routing table in lockstep with being evicted
    /// from our map.
    router: OnceLock<StdWeak<Router>>,
    /// Cached `/dev/dri` enumeration from startup. Used at spawn time to
    /// translate per-plugin `gpu_drm_dev = "<major>:<minor>"` settings into
    /// a `render_node` path injected into `Init.settings`. Empty vec if
    /// `attach_gpus` was never called (test stub).
    gpus: OnceLock<Arc<Vec<crate::gpu::GpuInfo>>>,
    /// Dead-renderer signals queue here (from reader-thread exit or
    /// a send_control hitting EPIPE). A single background reaper task
    /// drains the channel and runs the async `evict` — routing it
    /// through a channel keeps `mark_dead` synchronous, which breaks
    /// the async-Send inference cycle between `send_control` and
    /// `router::unregister_renderer → reconcile_lifecycle → send_control`.
    reap_tx: tokio::sync::mpsc::UnboundedSender<RendererId>,
    reap_rx: StdMutex<Option<tokio::sync::mpsc::UnboundedReceiver<RendererId>>>,
}

struct Inner {
    renderers: HashMap<RendererId, Arc<RendererHandle>>,
}

impl RendererManager {
    pub fn new(registry: RendererRegistry) -> Self {
        let (reap_tx, reap_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            inner: TokioMutex::new(Inner {
                renderers: HashMap::new(),
            }),
            registry,
            router: OnceLock::new(),
            gpus: OnceLock::new(),
            reap_tx,
            reap_rx: StdMutex::new(Some(reap_rx)),
        }
    }

    /// Hand the manager the startup `/dev/dri` snapshot so spawn-time can
    /// resolve `gpu_drm_dev` selections into `render_node` paths.
    /// Idempotent: further calls are no-ops.
    pub fn attach_gpus(&self, gpus: Arc<Vec<crate::gpu::GpuInfo>>) {
        let _ = self.gpus.set(gpus);
    }

    /// Wire the manager to the router. Must be called once after both
    /// sides have been constructed. Idempotent: further calls are
    /// no-ops.
    pub fn attach_router(&self, router: StdWeak<Router>) {
        let _ = self.router.set(router);
    }

    /// Start the background reaper task that drains `mark_dead`
    /// signals and runs the async eviction. Must be called from
    /// inside a tokio runtime context. No-op if already started or
    /// if the channel receiver was already taken.
    pub fn start_reaper(self: &Arc<Self>) {
        let rx = match self.reap_rx.lock() {
            Ok(mut g) => g.take(),
            Err(_) => return,
        };
        let Some(mut rx) = rx else { return };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(id) = rx.recv().await {
                this.evict(&id).await;
            }
        });
    }

    /// Test-only convenience: construct a manager whose registry has a
    /// single "scene" renderer pointed at `$WAYWALLEN_RENDERER_BIN`. If
    /// that env var is unset the registry is empty and any spawn call
    /// will fail with "no renderer registered for type 'scene'".
    pub fn new_default() -> Self {
        let mut registry = RendererRegistry::new();
        if let Some(bin) = std::env::var_os("WAYWALLEN_RENDERER_BIN") {
            registry.register(RendererDef {
                name: "test-scene".to_string(),
                bin: PathBuf::from(bin),
                types: vec!["scene".to_string()],
                priority: 100,
                version: "v0.0.0".to_string(),
                spawn_version: None,
                extras: Vec::new(),
                settings: Default::default(),
                events: Vec::new(),
            });
        }
        Self::new(registry)
    }

    /// Access the renderer registry (for HTTP introspection endpoints).
    pub fn registry(&self) -> &RendererRegistry {
        &self.registry
    }

    /// Spawn a fresh renderer-host subprocess, wait for its `Ready`
    /// event, and return its id. Fails (and cleans up the child) if the
    /// host doesn't come online within `timeout`.
    pub async fn spawn(&self, mut req: SpawnRequest) -> Result<RendererId> {
        let id: RendererId = Uuid::new_v4().to_string();

        // Create a listening UDS at a temp path; the child connects to
        // it shortly after exec().
        let sock_path = temp_sock_path(&id);
        let _ = std::fs::remove_file(&sock_path);
        let listener = tokio::net::UnixListener::bind(&sock_path)
            .with_context(|| format!("bind {}", sock_path.display()))?;

        // Best-effort cleanup of the socket file at the end of spawn —
        // the connection survives unlink(2).
        let _cleanup = TempUnlink(sock_path.clone());

        let renderer_def = match req.renderer_name.as_deref() {
            Some(name) => self
                .registry
                .resolve_by_name(name)
                .ok_or_else(|| Error::RendererNotFound(name.to_string()))?
                .clone(),
            None => self
                .registry
                .resolve(&req.wp_type)
                .ok_or_else(|| Error::NoRendererForType(req.wp_type.clone()))?
                .clone(),
        };

        // Translate the user's GPU choice into a render_node path before
        // it reaches the subprocess: plugin settings persist
        // `gpu_drm_dev = "<major>:<minor>"` (mirroring drm_render_major/
        // minor on the wire); the subprocess contract consumes
        // `render_node` (a path). On a hit we inject render_node and
        // strip gpu_drm_dev from the kv we ship. On a miss (defensive —
        // startup reconcile should have already cleared it) we leave
        // both out and let the subprocess pick a default device.
        if let Some(raw) = req.settings.remove(crate::gpu::GPU_DRM_DEV_KEY) {
            if let Some((major, minor)) = crate::gpu::parse_drm_dev(&raw) {
                let resolved = self
                    .gpus
                    .get()
                    .and_then(|gs| gs.iter().find(|g| g.matches_render(major, minor)))
                    .and_then(|g| g.render_node.as_ref())
                    .and_then(|p| p.to_str().map(str::to_string));
                if let Some(path) = resolved {
                    req.settings
                        .insert(crate::gpu::RENDER_NODE_KEY.to_string(), path);
                } else {
                    log::warn!(
                        "spawn: gpu_drm_dev={raw} not in /dev/dri enumeration; \
                         dropping selection and letting renderer pick default"
                    );
                }
            } else {
                log::warn!("spawn: gpu_drm_dev={raw:?} not parseable as <major>:<minor>");
            }
        }

        // Build the Init message *before* spawning the child (no
        // orphan socket file lingering past TempUnlink if anything
        // goes wrong later).
        let init_msg = build_init_msg(&req, &renderer_def);

        let mut cmd = Command::new(&renderer_def.bin);
        cmd.arg("--ipc").arg(&sock_path);
        // SPAWN_VERSION 3: extras (canonical `path` + plugin-specific
        // keys like `assets`/`workshop_id`) ride as `--<key> <value>`
        // CLI argv. Sorted for spawn-command determinism.
        let mut extra_keys: Vec<&String> = req.extras.keys().collect();
        extra_keys.sort();
        for k in extra_keys {
            cmd.arg(format!("--{k}")).arg(&req.extras[k]);
        }
        cmd.kill_on_drop(true)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", renderer_def.bin.display()))?;
        let child_pid = child.id();

        // Accept, with a bound to avoid hanging forever on a broken host.
        let accept = listener.accept();
        let (tokio_stream, _addr) = tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .map_err(|_| {
                let _ = child.start_kill();
                Error::RendererSpawnFailed(
                    "timed out waiting for waywallen-renderer to connect back".into(),
                )
            })?
            .context("accept")?;

        // Convert to a blocking std UnixStream for the rest of the
        // lifecycle: the ipc::uds helpers use nix sendmsg/recvmsg which
        // need a real blocking fd.
        let std_stream = tokio_stream.into_std().context("UnixStream::into_std")?;
        std_stream
            .set_nonblocking(false)
            .context("clear O_NONBLOCK on accepted stream")?;

        // Step 1 of the renderer-Init refactor: emit the typed Init
        // message right after accept(). The legacy CLI argv block above
        // is still in place; renderers that have not yet been switched
        // to consume Init simply ignore it. Send + Ready/InitNack recv
        // is factored into `run_init_handshake` so the unit test can
        // drive it over a socketpair without going through spawn().
        // (`init_msg` was built above before spawn so a schema error
        // fails before the child process is created.)
        let handshake_stream = std_stream
            .try_clone()
            .context("try_clone for Init handshake")?;
        let gpu =
            tokio::task::spawn_blocking(move || run_init_handshake(&handshake_stream, &init_msg))
                .await
                .context("init handshake join")?
                .map_err(|e| {
                    let _ = child.start_kill();
                    e
                })?;
        log::info!(
            "renderer {id}: Ready (drm_render={}:{})",
            gpu.major,
            gpu.minor
        );

        // Now wire up the permanent reader thread and store the handle.
        let (events_tx, _events_rx) = broadcast::channel::<EventMsg>(256);
        let bind_snapshot: Arc<StdMutex<Option<BindSnapshot>>> = Arc::new(StdMutex::new(None));
        let sync_fds: Arc<StdMutex<std::collections::VecDeque<(u64, OwnedFd)>>> =
            Arc::new(StdMutex::new(std::collections::VecDeque::new()));
        let release_syncobj: Arc<StdMutex<Option<OwnedFd>>> = Arc::new(StdMutex::new(None));
        let format_caps: Arc<StdMutex<Option<crate::dma::negotiate::PeerCaps>>> =
            Arc::new(StdMutex::new(None));
        let pending_configure: Arc<StdMutex<Option<u32>>> = Arc::new(StdMutex::new(None));
        let clear_rgba: Arc<StdMutex<[f32; 4]>> =
            Arc::new(StdMutex::new([0.0, 0.0, 0.0, 1.0]));

        let sock = Arc::new(StdMutex::new(std_stream));
        let reader_sock = sock.clone();
        let reader_events = events_tx.clone();
        let reader_snapshot = bind_snapshot.clone();
        let reader_sync_fds = sync_fds.clone();
        let reader_release_syncobj = release_syncobj.clone();
        let reader_format_caps = format_caps.clone();
        let reader_pending = pending_configure.clone();
        let reader_clear_rgba = clear_rgba.clone();
        let reader_id = id.clone();
        let reader_reap_tx = self.reap_tx.clone();
        thread::spawn(move || {
            run_reader(
                reader_id,
                reader_sock,
                reader_events,
                reader_snapshot,
                reader_sync_fds,
                reader_release_syncobj,
                reader_format_caps,
                reader_pending,
                reader_clear_rgba,
                reader_reap_tx,
            );
        });

        // Per-renderer reaper: drains FrameRecords, waits on consumer
        // signals, transfers fences onto the producer's release
        // timeline. Channel sender lives on the handle; receiver is
        // moved into the spawned task. Dropping the handle (renderer
        // evicted) closes the channel and the reaper exits cleanly.
        // We don't fail spawn if the DRM device can't open — the
        // renderer is still useful for acquire-only flows; the reaper
        // just won't run.
        let (frame_tx, frame_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::sync::FrameRecord>();
        let frame_record_tx = match crate::sync::drm_device() {
            Ok(_) => Some(frame_tx),
            Err(e) => {
                log::warn!(
                    "renderer {id}: no DRM render node ({e}); release-syncobj reaper disabled"
                );
                None
            }
        };

        let handle = Arc::new(RendererHandle {
            id: id.clone(),
            wp_type: req.wp_type.clone(),
            width: req.width,
            height: req.height,
            extent_mode: req.extent_mode,
            extras: req.extras.clone(),
            name: renderer_def.name.clone(),
            pid: child_pid,
            gpu,
            sock,
            events: events_tx,
            bind_snapshot,
            sync_fds,
            release_syncobj,
            format_caps,
            last_dispatched_scheme: Arc::new(StdMutex::new(None)),
            frame_record_tx,
            pending_configure,
            child: Arc::new(TokioMutex::new(Some(child))),
            events_subscribed: Arc::new(renderer_def.events.clone()),
            clear_rgba,
        });

        if handle.frame_record_tx.is_some() {
            // SAFETY: drm_device() returned Ok above; it caches the
            // device and is idempotent.
            let drm = crate::sync::drm_device().expect("checked above");
            // Pass only the renderer id and a clone of the
            // release_syncobj Arc — NOT Arc<RendererHandle>. The handle
            // owns the channel's Sender; if the reaper held an Arc to
            // it, the channel would never close (self-referential
            // cycle), the reaper task would leak, and pending buckets
            // would tie up DRM syncobjs forever.
            crate::sync::spawn_reaper(
                drm,
                id.clone(),
                Arc::clone(&handle.release_syncobj),
                frame_rx,
            );
        }

        {
            let mut inner = self.inner.lock().await;
            inner.renderers.insert(id.clone(), handle);
        }
        log::info!("spawned renderer {id} ({}x{})", req.width, req.height);
        Ok(id)
    }

    /// Find an already-running renderer whose **identity** matches
    /// `req` and the resolved manifest schema, ignoring runtime-tunable
    /// (`identity = false`) settings. Returns the id plus a delta of
    /// runtime-only metadata that differs from the live renderer's
    /// current `runtime_settings` cache, plus an optional new fps when
    /// the manifest declares fps as a runtime setting (or for reusable
    /// mismatches against the typed `req.fps`, see below).
    ///
    /// Reuse a live renderer when:
    ///   - structural: `wp_type` / `width` / `height` / `extent_mode` /
    ///     resolved renderer plugin name all match.
    ///   - per-spawn: `extras` matches (different `path` ⇒ different
    ///     wallpaper ⇒ different renderer process).
    ///
    /// Plugin settings live in `Settings::plugin(&name)` and are
    /// pushed live to all renderers by `SettingsSet`. The renderer
    /// applies what it can; whatever it can't apply live takes effect
    /// on the next spawn (via the fresh `Init.settings`). Returns
    /// `None` when no live renderer matches.
    pub async fn find_reusable(&self, req: &SpawnRequest) -> Option<RendererId> {
        let def = match req.renderer_name.as_deref() {
            Some(name) => self.registry.resolve_by_name(name)?.clone(),
            None => self.registry.resolve(&req.wp_type)?.clone(),
        };

        let inner = self.inner.lock().await;
        for (id, h) in inner.renderers.iter() {
            if h.wp_type != req.wp_type
                || h.width != req.width
                || h.height != req.height
                || h.extent_mode != req.extent_mode
                || h.name != def.name
            {
                continue;
            }
            if h.extras != req.extras {
                continue;
            }
            return Some(id.clone());
        }
        None
    }

    pub async fn get(&self, id: &str) -> Option<Arc<RendererHandle>> {
        let inner = self.inner.lock().await;
        inner.renderers.get(id).cloned()
    }

    /// Locate a live renderer whose `extras["path"]` matches the given
    /// resource. Used by WallpaperPropertySet to route the kv to the
    /// right child.
    pub async fn find_by_resource(&self, resource: &str) -> Option<Arc<RendererHandle>> {
        let inner = self.inner.lock().await;
        inner.renderers.values().find_map(|h| {
            (h.extras.get("path").map(String::as_str) == Some(resource)).then(|| h.clone())
        })
    }

    pub async fn list(&self) -> Vec<RendererId> {
        let inner = self.inner.lock().await;
        inner.renderers.keys().cloned().collect()
    }

    /// Fire-and-forget control send. Returns an error if the renderer
    /// is unknown or the underlying socket write fails. On EPIPE /
    /// ECONNRESET / ENOTCONN the handle is enqueued for eviction via
    /// `mark_dead` before the error is returned so follow-up calls
    /// don't keep re-hitting a dead peer.
    pub async fn send_control(&self, id: &str, msg: ControlMsg) -> Result<()> {
        let handle = self
            .get(id)
            .await
            .ok_or_else(|| Error::RendererNotFound(id.to_string()))?;
        let sock = handle.sock.clone();
        let codec_res: Result<std::result::Result<(), CodecError>> =
            tokio::task::spawn_blocking(move || {
                let guard = sock.lock().map_err(|e| {
                    Error::RendererControlFailed(format!("sock mutex poisoned: {e}"))
                })?;
                Ok(send_control(&*guard, &msg, &[]))
            })
            .await
            .context("send_control join")?;
        match codec_res? {
            Ok(()) => Ok(()),
            Err(e) => {
                if is_peer_gone(&e) {
                    log::warn!("renderer {id}: peer gone on send_control ({e}), evicting");
                    self.mark_dead(id);
                }
                Err(Error::RendererControlFailed(format!("send_control: {e}")))
            }
        }
    }

    /// Modifier-negotiation v2 dispatch — replaces the deleted
    /// `send_configure_buffers`.
    /// Idempotent: returns Ok without sending if `scheme` matches the
    /// last-dispatched scheme cached on the renderer handle.
    pub async fn send_negotiate_buffers(
        &self,
        id: &str,
        scheme: crate::dma::negotiate::NegotiatedScheme,
    ) -> Result<()> {
        let handle = self
            .get(id)
            .await
            .ok_or_else(|| Error::RendererNotFound(id.to_string()))?;
        // Idempotence: skip if we've already dispatched this exact scheme.
        if let Ok(guard) = handle.last_dispatched_scheme.lock() {
            if guard.as_ref() == Some(&scheme) {
                return Ok(());
            }
        }
        log::info!(
            "renderer {id}: NegotiateBuffers fourcc=0x{:08x} modifier=0x{:x} \
             plane_count={} sync=0x{:x} color=0x{:x} mem_hint=0x{:x} \
             count={} path={:?} mem_source={:?}",
            scheme.fourcc,
            scheme.modifier,
            scheme.plane_count,
            scheme.sync_mode,
            scheme.color,
            scheme.mem_hint,
            scheme.count,
            scheme.path,
            scheme.mem_source,
        );
        let msg = ControlMsg::NegotiateBuffers {
            fourcc: scheme.fourcc,
            modifier: scheme.modifier,
            plane_count: scheme.plane_count,
            sync_mode: scheme.sync_mode,
            color: scheme.color,
            mem_hint: scheme.mem_hint,
            count: scheme.count,
            path: scheme.path.as_u32(),
            mem_source: scheme.mem_source.as_u32(),
        };
        self.send_control(id, msg).await?;
        if let Ok(mut guard) = handle.last_dispatched_scheme.lock() {
            *guard = Some(scheme);
        }
        Ok(())
    }

    /// Push a `setting_changed` event to a live renderer. `settings` is
    /// the delta the caller already filtered to runtime-only keys
    /// (identity-tagged settings would force respawn, not hot-reload).
    /// `fps == None` is the no-fps-change signal; `Some(0)` is treated
    /// as "no change" too — the wire format uses 0 as the unset
    /// sentinel and a fps of 0 makes no physical sense for a renderer.
    ///
    /// On success the renderer's `runtime_settings` cache is merged
    /// with `settings` so the next reuse comparison sees the post-apply
    /// state. No idempotence cache for now; each call sends.
    pub async fn send_setting_changed(
        &self,
        id: &str,
        settings: Vec<(String, String)>,
        fps: Option<u32>,
    ) -> Result<()> {
        let handle = self
            .get(id)
            .await
            .ok_or_else(|| Error::RendererNotFound(id.to_string()))?;
        // setting_changed is a pure kv list. fps is just one of the kv
        // keys (when the manifest declares it), not a typed scalar.
        // Fold the legacy `fps_change` arg into the kv list before
        // dispatch.
        let mut settings = settings;
        if let Some(f) = fps {
            if f != 0 {
                settings.retain(|(k, _)| k != "fps");
                settings.push(("fps".to_string(), f.to_string()));
            }
        }
        let msg = ControlMsg::SettingChanged {
            settings: settings.clone(),
        };
        log::info!(
            "renderer {id}: setting_changed keys={:?}",
            settings.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        );
        self.send_control(id, msg).await?;
        let _ = handle;
        Ok(())
    }

    /// Forward a pointer-motion event to a live renderer. Silently
    /// drops when the renderer's manifest didn't declare
    /// `events = ["pointer"]` — this is the expected gating point for
    /// any inbound pointer family event.
    pub async fn send_pointer_motion(
        &self,
        id: &str,
        x: f32,
        y: f32,
        timestamp_us: u64,
        modifiers: u32,
    ) -> Result<()> {
        if !self.subscribed_to(id, "pointer").await {
            return Ok(());
        }
        self.send_control(
            id,
            ControlMsg::PointerMotion {
                x,
                y,
                timestamp_us,
                modifiers,
            },
        )
        .await
    }

    /// Forward a pointer-button event. Same gating as
    /// [`Self::send_pointer_motion`].
    pub async fn send_pointer_button(
        &self,
        id: &str,
        x: f32,
        y: f32,
        button: u32,
        state: u32,
        timestamp_us: u64,
        modifiers: u32,
    ) -> Result<()> {
        if !self.subscribed_to(id, "pointer").await {
            return Ok(());
        }
        self.send_control(
            id,
            ControlMsg::PointerButton {
                x,
                y,
                button,
                state,
                timestamp_us,
                modifiers,
            },
        )
        .await
    }

    /// Forward a pointer-axis (scroll) event. Same gating as
    /// [`Self::send_pointer_motion`].
    pub async fn send_pointer_axis(
        &self,
        id: &str,
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
        source: u32,
        timestamp_us: u64,
        modifiers: u32,
    ) -> Result<()> {
        if !self.subscribed_to(id, "pointer").await {
            return Ok(());
        }
        self.send_control(
            id,
            ControlMsg::PointerAxis {
                x,
                y,
                delta_x,
                delta_y,
                source,
                timestamp_us,
                modifiers,
            },
        )
        .await
    }

    /// Returns `true` when the renderer is alive and its manifest
    /// declared `events = [..., kind, ...]`. Unknown id ⇒ `false`
    /// (caller treats that as "drop on floor"; it's the same handling
    /// `send_*` use for unsubscribed renderers).
    async fn subscribed_to(&self, id: &str, kind: &str) -> bool {
        match self.get(id).await {
            Some(h) => h.events_subscribed.iter().any(|e| e == kind),
            None => false,
        }
    }

    /// Enqueue a renderer for eviction. Synchronous (cheap channel
    /// send); the actual cleanup happens on the reaper task started
    /// by `start_reaper`. Safe to call from anywhere, including non-
    /// async contexts (e.g. the reader thread's drop guard). Multiple
    /// signals for the same id are fine — `evict` is idempotent.
    pub fn mark_dead(&self, id: &str) {
        if self.reap_tx.send(id.to_string()).is_err() {
            log::warn!("renderer {id}: mark_dead dropped (reaper channel closed)");
        }
    }

    /// Actual eviction: remove from map, unregister from router, kill
    /// child. Called only by the reaper task. Idempotent: a second
    /// call with the same id is a no-op.
    async fn evict(self: &Arc<Self>, id: &str) {
        let handle = {
            let mut inner = self.inner.lock().await;
            inner.renderers.remove(id)
        };
        let Some(handle) = handle else { return };
        log::warn!("renderer {id}: evicting");

        if let Some(router) = self.router.get().and_then(|w| w.upgrade()) {
            router.unregister_renderer(id).await;
        }

        let mut child_guard = handle.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
    }

    /// Send Shutdown, wait for the child to exit gracefully, escalate
    /// to SIGKILL only if it doesn't. Removes from the map.
    ///
    /// The graceful path is critical for cross-process Vulkan
    /// correctness: the renderer's exit sequence drains its device
    /// (`ww_bridge_pool_destroy` calls `ops->wait_idle` before tearing
    /// slots down), which lets the acquire dma_fence it exported to
    /// consumers signal cleanly. SIGKILL skips that drain — the kernel
    /// then force-cancels the dma_fence, and on NVIDIA the consumer's
    /// pending `vkWaitForFences` returns DEVICE_LOST.
    pub async fn kill(&self, id: &str) -> Result<()> {
        let handle = {
            let mut inner = self.inner.lock().await;
            inner.renderers.remove(id)
        }
        .ok_or_else(|| Error::RendererNotFound(id.to_string()))?;

        // Send Shutdown over the bridge socket.
        let sock = handle.sock.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Ok(guard) = sock.lock() {
                let _ = send_control(&*guard, &ControlMsg::Shutdown, &[]);
            }
        })
        .await;

        let mut child_guard = handle.child.lock().await;
        if let Some(mut child) = child_guard.take() {
            // 5 s: comfortably above any plausible vkDeviceWaitIdle
            // under load (image renderer is microseconds; mpv/wescene
            // can spike to hundreds of ms during heavy frames).
            match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                Ok(_) => {
                    log::info!("renderer {id}: graceful shutdown");
                }
                Err(_) => {
                    log::warn!(
                        "renderer {id}: Shutdown timeout (5s), escalating to SIGKILL"
                    );
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(
                        Duration::from_secs(1),
                        child.wait(),
                    )
                    .await;
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reader thread
// ---------------------------------------------------------------------------

fn run_reader(
    id: RendererId,
    sock: Arc<StdMutex<StdUnixStream>>,
    events: broadcast::Sender<EventMsg>,
    bind_snapshot: Arc<StdMutex<Option<BindSnapshot>>>,
    sync_fds: Arc<StdMutex<std::collections::VecDeque<(u64, OwnedFd)>>>,
    release_syncobj: Arc<StdMutex<Option<OwnedFd>>>,
    format_caps: Arc<StdMutex<Option<crate::dma::negotiate::PeerCaps>>>,
    pending_configure: Arc<StdMutex<Option<u32>>>,
    clear_rgba: Arc<StdMutex<[f32; 4]>>,
    reap_tx: tokio::sync::mpsc::UnboundedSender<RendererId>,
) {
    // Any exit path from this thread — clean EOF, recvmsg error, or
    // panic — enqueues the renderer for eviction so stale ids don't
    // leak out through find_reusable or bind_snapshot.
    let _reap = ReaperOnDrop {
        id: id.clone(),
        tx: reap_tx,
    };

    // Hold the stream by dup'ing the raw fd so the blocking recv is not
    // contending with sends on the same mutex. recvmsg on an AF_UNIX
    // stream socket is safe to call from a different fd referencing the
    // same open file description.
    let read_stream = {
        let guard = match sock.lock() {
            Ok(g) => g,
            Err(_) => {
                log::error!("renderer {id}: sock mutex poisoned, reader exiting");
                return;
            }
        };
        match guard.try_clone() {
            Ok(s) => s,
            Err(e) => {
                log::error!("renderer {id}: try_clone failed: {e}");
                return;
            }
        }
    };

    loop {
        let received = match recv_event(&read_stream) {
            Ok(ok) => ok,
            Err(e) => {
                log::info!("renderer {id}: reader exit: {e}");
                return;
            }
        };
        let (msg, fds) = received;

        // Cache every BindBuffers with its fds. The renderer assigns the
        // generation; subsequent bind_buffers (post-ConfigureBuffers
        // re-export) replace the snapshot and retire prior acquire
        // fences. Validates monotonicity defensively.
        if let EventMsg::BindBuffers {
            generation,
            flags,
            count,
            fourcc,
            width,
            height,
            modifier,
            planes_per_buffer,
            ref stride,
            ref plane_offset,
            ref size,
        } = msg
        {
            // Validate the parallel-array invariant up-front. The wire
            // event is symmetric in every per-plane field, so any
            // length mismatch means the renderer mis-encoded.
            let expected = (count as usize) * (planes_per_buffer as usize);
            if stride.len() != expected
                || plane_offset.len() != expected
                || size.len() != expected
                || fds.len() != expected
            {
                log::warn!(
                    "renderer {id}: BindBuffers length mismatch \
                     count={count} planes={planes_per_buffer} expected={expected} \
                     stride={} offset={} size={} fds={}; dropping",
                    stride.len(),
                    plane_offset.len(),
                    size.len(),
                    fds.len()
                );
            } else if fds.is_empty() {
                log::warn!("renderer {id}: BindBuffers arrived without fds");
            } else {
                let prev_gen = bind_snapshot
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.generation));
                if let Some(prev) = prev_gen {
                    if generation <= prev {
                        log::warn!(
                            "renderer {id}: BindBuffers gen={generation} not > prev {prev}; \
                             accepting anyway but display protocol expects monotonicity"
                        );
                    }
                }
                let snap = BindSnapshot {
                    generation,
                    flags,
                    count,
                    fourcc,
                    width,
                    height,
                    modifier,
                    planes_per_buffer,
                    stride: stride.clone(),
                    plane_offset: plane_offset.clone(),
                    size: size.clone(),
                    fds,
                };
                if let Ok(mut guard) = bind_snapshot.lock() {
                    *guard = Some(snap);
                    log::info!(
                        "renderer {id}: BindBuffers cached (gen={generation}, flags=0x{flags:x})"
                    );
                }
                // A rebind retires any pending acquire fences — they
                // belong to the previous buffer_generation and cannot
                // be waited on against the new textures.
                if let Ok(mut guard) = sync_fds.lock() {
                    guard.clear();
                }
                // Clear any in-flight ConfigureBuffers. We always clear,
                // even if the renderer's `flags` differ from what we
                // asked for — some renderers (mpv-via-GBM, wescene's
                // ExSwapchain) only support the HOST_VISIBLE/LINEAR
                // path and physically can't downgrade to DEVICE_LOCAL.
                // Leaving pending_configure set after such a "best
                // effort" answer would just keep `reconcile_buffer_flags`
                // skipping the renderer forever. A warn log makes the
                // mismatch visible.
                if let Ok(mut guard) = pending_configure.lock() {
                    if let Some(want) = guard.take() {
                        if want != flags {
                            log::warn!(
                                "renderer {id}: ConfigureBuffers asked for \
                                 flags=0x{want:x} but renderer answered \
                                 with flags=0x{flags:x}; accepting"
                            );
                        }
                    }
                }
            }
        } else if let EventMsg::FrameReady { seq, .. } = msg {
            // frame_ready always carries exactly one sync_fd: the codec
            // enforced expected_fds() == 1 before handing us `fds`.
            let mut taken = fds;
            let fd = taken.remove(0);
            if let Ok(mut guard) = sync_fds.lock() {
                while guard.len() >= SYNC_FD_RETENTION {
                    guard.pop_front();
                }
                guard.push_back((seq, fd));
            }
        } else if let EventMsg::ReleaseSyncobj = msg {
            // Producer's exported timeline drm_syncobj. Exactly one fd;
            // the codec enforced expected_fds() == 1.
            let mut taken = fds;
            let fd = taken.remove(0);
            if let Ok(mut guard) = release_syncobj.lock() {
                if guard.is_some() {
                    log::warn!(
                        "renderer {id}: ReleaseSyncobj received twice; \
                         replacing previous fd"
                    );
                }
                *guard = Some(fd);
                log::info!("renderer {id}: ReleaseSyncobj imported");
            }
        } else if let EventMsg::FormatCaps {
            ref fourccs,
            ref mod_counts,
            ref modifiers,
            ref plane_counts,
            ref device_uuid,
            ref driver_uuid,
            drm_render_major,
            drm_render_minor,
            mem_hints,
            sync_caps,
            color_caps,
            extent_max_w,
            extent_max_h,
        } = msg
        {
            let drm = DrmNode {
                major: drm_render_major,
                minor: drm_render_minor,
            };
            match crate::dma::negotiate::unflatten_caps(
                fourccs,
                mod_counts,
                modifiers,
                plane_counts,
                device_uuid,
                driver_uuid,
                drm,
                sync_caps,
                color_caps,
                mem_hints,
                (extent_max_w, extent_max_h),
            ) {
                Ok(caps) => {
                    if let Ok(mut guard) = format_caps.lock() {
                        if guard.is_some() {
                            log::warn!(
                                "renderer {id}: FormatCaps received twice; \
                                 replacing previous caps"
                            );
                        }
                        let prefix = format!("renderer {id}: format_caps");
                        log::info!(
                            "{prefix}: imported {} fourcc{}",
                            caps.formats.by_fourcc.len(),
                            if caps.formats.by_fourcc.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                        );
                        caps.log_dump(&prefix);
                        *guard = Some(caps);
                    }
                }
                Err(e) => {
                    log::warn!("renderer {id}: FormatCaps malformed: {e:?}");
                }
            }
        } else if let EventMsg::BindFailed {
            fourcc,
            modifier,
            reason,
            ref message,
        } = msg
        {
            // Iter 5 wires the daemon-side blacklist + retry. For now
            // surface the failure for debugging.
            log::warn!(
                "renderer {id}: BindFailed fourcc=0x{fourcc:08x} \
                 modifier=0x{modifier:x} reason={reason} msg={message:?}"
            );
        } else if let EventMsg::ReportState { ref state } = msg {
            // Recognised keys are stashed on the handle; unknown keys
            // are ignored. Currently only `clear_color` is consumed.
            for (k, v) in state.iter() {
                if k == "clear_color" {
                    if let Some(rgba) = parse_clear_color(v) {
                        if let Ok(mut g) = clear_rgba.lock() {
                            *g = rgba;
                        }
                    } else {
                        log::warn!(
                            "renderer {id}: ReportState clear_color={v:?} unparseable, ignored"
                        );
                    }
                }
            }
        } else if !fds.is_empty() {
            log::warn!("renderer {id}: unexpected fds on event {msg:?}, dropping");
        }

        // Broadcast to any subscribers. No subscribers means no error:
        // SendError is only returned when receivers drop, which is fine.
        let _ = events.send(msg);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True when a `send_control` / `recv_event` error indicates the peer
/// is gone (renderer crashed, closed its UDS, etc.). Callers use this
/// to trigger `mark_dead` instead of just surfacing the error.
fn is_peer_gone(err: &CodecError) -> bool {
    use nix::errno::Errno;
    matches!(
        err,
        CodecError::PeerClosed
            | CodecError::Nix(Errno::EPIPE | Errno::ECONNRESET | Errno::ENOTCONN)
    )
}

/// RAII guard that enqueues the renderer for eviction when the reader
/// thread drops it — any exit path (EOF, recvmsg error, panic) ends
/// up here so the manager's map and the router's routing table stay
/// in sync with the actual set of live renderer children.
struct ReaperOnDrop {
    id: RendererId,
    tx: tokio::sync::mpsc::UnboundedSender<RendererId>,
}

impl Drop for ReaperOnDrop {
    fn drop(&mut self) {
        let id = std::mem::take(&mut self.id);
        let _ = self.tx.send(id);
    }
}

fn temp_sock_path(id: &str) -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = runtime_dir.join("waywallen");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("renderer-{id}.sock"))
}

struct TempUnlink(PathBuf);
impl Drop for TempUnlink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Build the typed `Init` control message the daemon emits right
/// after a renderer subprocess connects back.
///
/// SPAWN_VERSION 3: Init carries only what's needed before
/// `advertise_caps` (extent triple) plus the resolved settings kv.
/// Resource path + extras (assets, workshop_id, …) ride on the
/// renderer's CLI argv instead — see `spawn`'s argv builder.
///
/// `req.settings` is taken as authoritative — bound-checking and
/// default-filling happen at the settings-store boundary
/// (`Settings::reconcile` on startup, `coerce_and_validate` in the
/// `SettingsSet` RPC), so spawn-time re-validation would be
/// redundant defense. The typed `test_pattern` flag is injected
/// last and overrides whatever was in `settings`, matching the
/// apply-path contract where it is the canonical source for that
/// key. `fps` is plain settings — callers put it in `req.settings`.
///
/// `spawn_version` is read from the manifest if set, otherwise the
/// daemon's compile-time `SPAWN_VERSION` constant.
pub(crate) fn build_init_msg(req: &SpawnRequest, def: &RendererDef) -> ControlMsg {
    let spawn_version = def.spawn_version.unwrap_or(SPAWN_VERSION);

    let mut settings_kv: HashMap<String, String> = req.settings.clone();

    if def.settings.contains_key("test_pattern") && req.test_pattern {
        settings_kv.insert("test_pattern".to_string(), "1".to_string());
    }

    let mut settings: Vec<(String, String)> = settings_kv.into_iter().collect();
    settings.sort_by(|a, b| a.0.cmp(&b.0));

    ControlMsg::Init {
        spawn_version,
        extent_w: req.width,
        extent_h: req.height,
        extent_mode: req.extent_mode,
        settings,
        user_properties: req.user_properties_json.clone().unwrap_or_default(),
    }
}

/// Run the post-accept handshake on a blocking std `UnixStream`:
/// send the typed `Init` request, then read exactly one event. On
/// `Ready` return the renderer's `DrmNode`; on `InitNack` surface a
/// readable error; any other event is treated as a protocol violation
/// (caller is expected to kill the child).
///
/// Factored out of `RendererManager::spawn` so unit tests can drive
/// it directly over a `UnixStream::pair()` without booting a child
/// process.
pub(crate) fn run_init_handshake(sock: &StdUnixStream, init: &ControlMsg) -> Result<DrmNode> {
    send_control(sock, init, &[])
        .map_err(|e| Error::RendererSpawnFailed(format!("send Init: {e}")))?;
    let (evt, fds) =
        recv_event(sock).map_err(|e| Error::RendererSpawnFailed(format!("recv Ready: {e}")))?;
    match evt {
        EventMsg::Ready {
            drm_render_major,
            drm_render_minor,
        } => {
            if !fds.is_empty() {
                log::warn!("Ready unexpectedly carried {} fds; dropping", fds.len());
            }
            Ok(DrmNode {
                major: drm_render_major,
                minor: drm_render_minor,
            })
        }
        EventMsg::InitNack {
            received_spawn_version,
            supported_spawn_version,
            reason,
        } => Err(Error::RendererSpawnFailed(format!(
            "renderer rejected Init: {reason} (received spawn_version={received_spawn_version}, \
             supported={supported_spawn_version})"
        ))),
        other => Err(Error::RendererSpawnFailed(format!(
            "host emitted {other:?} before Ready; aborting spawn"
        ))),
    }
}

#[allow(dead_code)]
fn _assert_path_ok<P: AsRef<std::path::Path>>(_p: P) {} // compile-time shim

/// Parse a `"r,g,b,a"` clear-color value. Components clamped to
/// `[0, 1]`. Returns `None` when the string is malformed (wrong
/// component count, non-numeric, NaN). Whitespace around components
/// is permitted.
fn parse_clear_color(s: &str) -> Option<[f32; 4]> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return None;
    }
    let mut out = [0.0f32; 4];
    for (i, p) in parts.iter().enumerate() {
        let v: f32 = p.parse().ok()?;
        if !v.is_finite() {
            return None;
        }
        out[i] = v.clamp(0.0, 1.0);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Test stubs
// ---------------------------------------------------------------------------

#[cfg(test)]
impl RendererHandle {
    /// Test-only: inject a `PeerCaps` so router-level negotiation
    /// tests can pretend the renderer shipped a `FormatCaps` event.
    /// Replaces whatever was there.
    pub fn test_set_format_caps(&self, caps: crate::dma::negotiate::PeerCaps) {
        if let Ok(mut g) = self.format_caps.lock() {
            *g = Some(caps);
        }
    }

    /// Test-only: read the producer's blacklist length. Lets a
    /// router-side test assert that `on_renderer_bind_failed`
    /// actually inserted into the producer caps.
    pub fn test_blacklist_len(&self) -> usize {
        self.format_caps
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|c| c.blacklist.len()))
            .unwrap_or(0)
    }
}

impl RendererHandle {
    /// Construct a `RendererHandle` with no running child process.
    /// Used by routing-table unit tests AND by the runtime self_test
    /// (`waywallen --test`) which drives a stub renderer through the
    /// production endpoint via [`Self::push_self_test_event`] etc.
    pub fn test_stub(id: &str, wp_type: &str) -> Arc<Self> {
        let (a, _b) = StdUnixStream::pair().expect("UnixStream pair");
        let (events_tx, _) = broadcast::channel::<EventMsg>(8);
        Arc::new(Self {
            id: id.into(),
            wp_type: wp_type.into(),
            width: 1920,
            height: 1080,
            extent_mode: 0,
            extras: HashMap::new(),
            name: "test-stub".into(),
            pid: None,
            gpu: DrmNode::UNKNOWN,
            sock: Arc::new(StdMutex::new(a)),
            events: events_tx,
            bind_snapshot: Arc::new(StdMutex::new(None)),
            sync_fds: Arc::new(StdMutex::new(std::collections::VecDeque::new())),
            release_syncobj: Arc::new(StdMutex::new(None)),
            format_caps: Arc::new(StdMutex::new(None)),
            last_dispatched_scheme: Arc::new(StdMutex::new(None)),
            frame_record_tx: None,
            pending_configure: Arc::new(StdMutex::new(None)),
            child: Arc::new(TokioMutex::new(None)),
            events_subscribed: Arc::new(Vec::new()),
            clear_rgba: Arc::new(StdMutex::new([0.0, 0.0, 0.0, 1.0])),
        })
    }

    /// self_test (runtime `--test`): broadcast an event as if the
    /// renderer subprocess had emitted it. The router task subscribed
    /// in `register_renderer` picks it up and runs the same hooks
    /// (`on_renderer_bind` / `on_renderer_frame`) it would for a real
    /// renderer.
    pub fn push_self_test_event(&self, ev: EventMsg) {
        let _ = self.events.send(ev);
    }

    /// self_test (runtime `--test`): stash a per-frame acquire sync_fd
    /// the way the manager's reader thread does for production
    /// renderers. The display endpoint dups one out of the deque on
    /// each `forward_frame_ready` via [`clone_sync_fd`].
    pub fn push_self_test_sync_fd(&self, seq: u64, fd: OwnedFd) {
        if let Ok(mut g) = self.sync_fds.lock() {
            g.push_back((seq, fd));
            while g.len() > SYNC_FD_RETENTION {
                g.pop_front();
            }
        }
    }
}

impl RendererManager {
    /// Insert a pre-built handle into the manager's map without
    /// spawning a child process. Used by routing-table unit tests
    /// AND the runtime self_test (`waywallen --test`).
    pub async fn register_test_handle(&self, handle: Arc<RendererHandle>) {
        let mut inner = self.inner.lock().await;
        inner.renderers.insert(handle.id.clone(), handle);
    }
}

#[cfg(test)]
mod init_handshake_tests {
    use super::*;
    use crate::ipc::uds::send_event;
    use crate::plugin::renderer_registry::{SettingDef, SettingType};
    use std::path::PathBuf;
    use std::thread;

    fn def_legacy(name: &str) -> RendererDef {
        // Legacy (no-schema) manifest: build_init_msg falls back to
        // the hard-coded primary-key priority list.
        RendererDef {
            name: name.to_string(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["scene".to_string()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: None,
            extras: Vec::new(),
            settings: Default::default(),
            events: Vec::new(),
        }
    }

    fn def_scene_schema() -> RendererDef {
        RendererDef {
            name: "wescene-renderer".into(),
            bin: PathBuf::from("/dev/null"),
            types: vec!["scene".into()],
            priority: 100,
            version: "v0.0.0".into(),
            spawn_version: Some(1),
            extras: vec!["assets".into(), "workshop_id".into()],
            settings: Default::default(),
            events: Vec::new(),
        }
    }

    fn def_mpv_schema() -> RendererDef {
        let mut ps = HashMap::new();
        ps.insert(
            "loop_file".to_string(),
            SettingDef::new(
                SettingType::String,
                toml::Value::String("inf".into()),
                false,
            ),
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
            events: Vec::new(),
        }
    }

    // Legacy build_init_msg tests (resource_primary / resource_extras /
    // typed fps) were removed when the wire shape was slimmed down for
    // SPAWN_VERSION 3. Phase 6 will add a fresh test pass for the new
    // shape (extent + settings kv only) once `resolve_active_settings`
    // lands.

    #[test]
    fn slim_init_carries_extent_and_settings_kv() {
        // SPAWN_VERSION 3 sanity: extent triple + settings kv come
        // through verbatim. The caller is responsible for sourcing a
        // reconciled settings map (the daemon pulls it from
        // `Settings::plugin(&name)`); build_init_msg does not refill
        // defaults or filter unknown keys.
        let mut settings_in = HashMap::new();
        settings_in.insert("loop_file".to_string(), "inf".to_string());
        let req = SpawnRequest {
            extras: HashMap::new(),
            wp_type: "video".into(),
            settings: settings_in,
            width: 1920,
            height: 1080,
            extent_mode: 0,
            test_pattern: false,
            renderer_name: None,
            user_properties_json: None,
        };
        let msg = build_init_msg(&req, &def_mpv_schema());
        match msg {
            ControlMsg::Init {
                spawn_version,
                extent_w,
                extent_h,
                extent_mode,
                settings,
                user_properties,
            } => {
                assert_eq!(spawn_version, 1); // pulled from def_mpv_schema
                assert_eq!(extent_w, 1920);
                assert_eq!(extent_h, 1080);
                assert_eq!(extent_mode, 0);
                assert_eq!(settings, vec![("loop_file".to_string(), "inf".to_string())]);
                assert_eq!(user_properties, "");
            }
            other => panic!("expected ControlMsg::Init, got {other:?}"),
        }
    }

    #[test]
    fn spawn_handshake_init_nack_aborts() {
        // Daemon side ↔ renderer side over a socketpair: we drive
        // `run_init_handshake` from the daemon side and have a tiny
        // peer thread reply with an InitNack on the renderer side.
        let (daemon, renderer) = StdUnixStream::pair().expect("UnixStream::pair");
        daemon
            .set_nonblocking(false)
            .expect("set_nonblocking(false) on daemon side");
        renderer
            .set_nonblocking(false)
            .expect("set_nonblocking(false) on renderer side");

        let peer = thread::spawn(move || {
            // Receive the Init then immediately reply with InitNack.
            let (got, _fds) = crate::ipc::uds::recv_control(&renderer).expect("renderer recv Init");
            assert!(matches!(got, ControlMsg::Init { .. }));
            send_event(
                &renderer,
                &EventMsg::InitNack {
                    received_spawn_version: 999,
                    supported_spawn_version: SPAWN_VERSION,
                    reason: "unsupported spawn_version".into(),
                },
                &[],
            )
            .expect("renderer send InitNack");
        });

        let mut settings = HashMap::new();
        settings.insert("scene".to_string(), "/tmp/scene.pkg".to_string());
        let req = SpawnRequest {
            extras: HashMap::new(),
            wp_type: "scene".into(),
            settings,
            width: 800,
            height: 600,
            extent_mode: 0,
            test_pattern: false,
            renderer_name: None,
            user_properties_json: None,
        };
        let init = build_init_msg(&req, &def_legacy("wescene-renderer"));
        let err =
            run_init_handshake(&daemon, &init).expect_err("InitNack must abort the handshake");
        let s = err.to_string();
        assert!(
            s.contains("renderer rejected Init"),
            "unexpected error: {s}"
        );
        assert!(
            s.contains("unsupported spawn_version"),
            "unexpected error: {s}"
        );

        peer.join().expect("peer thread");
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::*;
    use crate::plugin::renderer_registry::{
        RendererDef, RendererRegistry, SettingDef, SettingType,
    };
    use std::path::PathBuf;

    fn def_mpv() -> RendererDef {
        let mut ps = HashMap::new();
        ps.insert(
            "loop_file".to_string(),
            SettingDef::new(
                SettingType::String,
                toml::Value::String("inf".into()),
                false,
            ),
        );
        ps.insert(
            "hwdec".to_string(),
            SettingDef::new(
                SettingType::String,
                toml::Value::String("auto".into()),
                false,
            ),
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
            events: Vec::new(),
        }
    }

    /// Construct a live mpv handle stub with the given extras dict.
    /// Mirrors `RendererHandle::test_stub` but lets the test pin
    /// `extras` (the per-spawn identity differentiator).
    fn live_mpv_handle(id: &str, extras: HashMap<String, String>) -> Arc<RendererHandle> {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (events_tx, _) = tokio::sync::broadcast::channel::<EventMsg>(8);
        Arc::new(RendererHandle {
            id: id.into(),
            wp_type: "video".into(),
            width: 1920,
            height: 1080,
            extent_mode: 0,
            extras,
            name: "waywallen-mpv".into(),
            pid: None,
            gpu: DrmNode::UNKNOWN,
            sock: Arc::new(StdMutex::new(a)),
            events: events_tx,
            bind_snapshot: Arc::new(StdMutex::new(None)),
            sync_fds: Arc::new(StdMutex::new(std::collections::VecDeque::new())),
            release_syncobj: Arc::new(StdMutex::new(None)),
            format_caps: Arc::new(StdMutex::new(None)),
            last_dispatched_scheme: Arc::new(StdMutex::new(None)),
            frame_record_tx: None,
            pending_configure: Arc::new(StdMutex::new(None)),
            child: Arc::new(TokioMutex::new(None)),
            events_subscribed: Arc::new(Vec::new()),
            clear_rgba: Arc::new(StdMutex::new([0.0, 0.0, 0.0, 1.0])),
        })
    }

    fn req_with_extras(extras: HashMap<String, String>) -> SpawnRequest {
        SpawnRequest {
            extras,
            wp_type: "video".into(),
            settings: HashMap::new(),
            width: 1920,
            height: 1080,
            extent_mode: 0,
            test_pattern: false,
            renderer_name: None,
            user_properties_json: None,
        }
    }

    #[tokio::test]
    async fn find_reusable_hits_when_extras_match() {
        let mut registry = RendererRegistry::new();
        registry.register(def_mpv());
        let mgr = RendererManager::new(registry);

        let mut extras = HashMap::new();
        extras.insert("path".into(), "/clip.mp4".into());
        let h = live_mpv_handle("h1", extras.clone());
        mgr.register_test_handle(h).await;

        let req = req_with_extras(extras);
        let id = mgr.find_reusable(&req).await.expect("reuse hit expected");
        assert_eq!(id, "h1");
    }

    #[tokio::test]
    async fn find_reusable_misses_on_different_path() {
        let mut registry = RendererRegistry::new();
        registry.register(def_mpv());
        let mgr = RendererManager::new(registry);

        let mut h_extras = HashMap::new();
        h_extras.insert("path".into(), "/clip.mp4".into());
        mgr.register_test_handle(live_mpv_handle("h1", h_extras))
            .await;

        let mut req_extras = HashMap::new();
        req_extras.insert("path".into(), "/other.mp4".into());
        let req = req_with_extras(req_extras);
        assert!(
            mgr.find_reusable(&req).await.is_none(),
            "different path must miss reuse",
        );
    }

    #[tokio::test]
    async fn send_setting_changed_writes_wire_and_updates_cache() {
        // Direct end-to-end: spawn a socketpair, plug one side into a
        // RendererHandle's sock, call send_setting_changed, drain the
        // wire on the other side, assert the kv arrived.
        let mut registry = RendererRegistry::new();
        registry.register(def_mpv());
        let mgr = RendererManager::new(registry);

        let (daemon_side, renderer_side) = std::os::unix::net::UnixStream::pair().unwrap();
        daemon_side.set_nonblocking(false).unwrap();
        renderer_side.set_nonblocking(false).unwrap();

        let (events_tx, _) = tokio::sync::broadcast::channel::<EventMsg>(8);
        let h = Arc::new(RendererHandle {
            id: "h1".into(),
            wp_type: "video".into(),
            width: 1920,
            height: 1080,
            extent_mode: 0,
            extras: HashMap::new(),
            name: "waywallen-mpv".into(),
            pid: None,
            gpu: DrmNode::UNKNOWN,
            sock: Arc::new(StdMutex::new(daemon_side)),
            events: events_tx,
            bind_snapshot: Arc::new(StdMutex::new(None)),
            sync_fds: Arc::new(StdMutex::new(std::collections::VecDeque::new())),
            release_syncobj: Arc::new(StdMutex::new(None)),
            format_caps: Arc::new(StdMutex::new(None)),
            last_dispatched_scheme: Arc::new(StdMutex::new(None)),
            frame_record_tx: None,
            pending_configure: Arc::new(StdMutex::new(None)),
            child: Arc::new(TokioMutex::new(None)),
            events_subscribed: Arc::new(Vec::new()),
            clear_rgba: Arc::new(StdMutex::new([0.0, 0.0, 0.0, 1.0])),
        });
        mgr.register_test_handle(Arc::clone(&h)).await;

        // Renderer-side reader running in a thread to drain the wire.
        let peer = std::thread::spawn(move || {
            let (req, _fds) = crate::ipc::uds::recv_control(&renderer_side).expect("recv");
            req
        });

        mgr.send_setting_changed("h1", vec![("loop_file".into(), "no".into())], None)
            .await
            .expect("send_setting_changed ok");

        let got = peer.join().expect("peer joined");
        match got {
            ControlMsg::SettingChanged { settings } => {
                assert_eq!(settings, vec![("loop_file".into(), "no".into())]);
            }
            other => panic!("expected ApplySettings, got {other:?}"),
        }
    }
}
