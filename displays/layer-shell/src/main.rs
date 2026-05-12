//! waywallen-display-layer-shell — Wayland layer-shell wallpaper client.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_callback::{self, WlCallback},
    wl_compositor::WlCompositor,
    wl_output::{self, Transform, WlOutput},
    wl_registry::WlRegistry,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::{self, WpFractionalScaleManagerV1},
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use waywallen::display::proto::{
    codec, Event as ProtoEvent, Request as ProtoRequest, PROTOCOL_NAME, PROTOCOL_VERSION,
};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Args {
    socket: PathBuf,
    name_prefix: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: waywallen-display-layer-shell [--socket PATH] [--name STR]\n\
         \n\
         Environment:\n\
           WAYWALLEN_SOCKET   fallback UDS path when --socket is omitted\n\
           WAYLAND_DISPLAY    required — picks the compositor to attach to"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut socket: Option<PathBuf> = None;
    let mut name_prefix = String::from("waywallen-layer-shell");
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => {
                socket = it.next().map(PathBuf::from);
                if socket.is_none() {
                    eprintln!("--socket requires a value");
                    usage();
                }
            }
            "--name" => {
                name_prefix = it.next().unwrap_or_else(|| {
                    eprintln!("--name requires a value");
                    usage();
                });
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let socket = socket
        .or_else(|| std::env::var_os("WAYWALLEN_SOCKET").map(PathBuf::from))
        .unwrap_or_else(default_socket_path);
    Args {
        socket,
        name_prefix,
    }
}

fn default_socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    runtime.join("waywallen").join("display.sock")
}

// ---------------------------------------------------------------------------
// Per-output state
// ---------------------------------------------------------------------------

/// Shared surface + protocol proxies a single output's UDS worker needs
/// to attach frames. All proxies in wayland-client 0.31 are `Send + Sync`,
/// so the worker thread can invoke requests freely; writes are serialized
/// through the shared `Connection` and flushed explicitly.
struct OutputBinding {
    display_name: String,
    surface: WlSurface,
    dmabuf: ZwpLinuxDmabufV1,
    conn: Connection,
    /// QueueHandle used for child proxies created from the worker
    /// thread (frame callbacks, dmabuf params). Clone of the main
    /// thread's queue handle.
    qh: QueueHandle<App>,
    /// Global name of the owning `wl_output`. Used as user-data when
    /// requesting frame callbacks so the main-thread Dispatch routes
    /// the `Done` event back to the right `frame_pending` flag.
    output_name: u32,
    /// Physical buffer size (logical × integer_scale) the daemon must
    /// render at for 1:1 mapping on HiDPI. Populated on the first
    /// layer_surface Configure; worker advertises this as the display
    /// size when registering with the daemon.
    configured_size: Mutex<Option<(u32, u32)>>,
    /// Logical surface size (from `zwlr_layer_surface_v1::configure`).
    /// Used as the viewport destination so the compositor maps the
    /// physical-size buffer onto the correct surface extent.
    logical_size: Mutex<Option<(u32, u32)>>,
    /// Integer output scale from `wl_output::scale`. Defaults to 1;
    /// updated before worker spawns (we roundtrip after bind so
    /// output metadata has landed).
    scale: std::sync::atomic::AtomicI32,
    /// Preferred fractional scale from `wp_fractional_scale_v1` in
    /// 1/120 units. `0` means the protocol either isn't bound or
    /// hasn't delivered `preferred_scale` yet — fall back to integer
    /// `scale`. Computing physical = round(logical × scale / 120)
    /// avoids the ceil-rounding error that produces 4096×2304 for a
    /// 2560×1440 monitor at 1.25× (integer scale = 2 → over-allocates).
    fractional_scale_120: AtomicU32,
    /// Optional `wp_viewport` — when bound, gives us explicit
    /// source-rect/dest-rect mapping between buffer and surface
    /// (handles HiDPI + `SetConfig` crop). Absent → fall back to
    /// `wl_surface::set_buffer_scale`.
    viewport: Option<WpViewport>,
    /// Set to `true` when the corresponding `wl_output` is removed at
    /// runtime (hot-unplug). The worker checks before reconnect; the
    /// main thread also `shutdown(2)`s the active stream so any
    /// blocking `recv_event` returns immediately.
    closed: AtomicBool,
    /// Most-recent live UDS connection. Worker stashes it after a
    /// successful `connect`; cleared on session exit. Main thread
    /// reads + shutdowns it on hot-unplug.
    stream: RwLock<Option<Arc<UnixStream>>>,
    /// `true` while a `wl_callback::done` is outstanding. Set after
    /// commit + frame(); cleared by the `WlCallback` Dispatch impl.
    /// Gates whether the worker commits a new buffer (throttles to
    /// compositor vblank) — `BufferRelease` is always sent so the
    /// daemon keeps producing.
    frame_pending: AtomicBool,
    /// Per-buffer-index FIFO of release_syncobj fds we've received
    /// from the daemon and not yet signaled. Worker pushes on each
    /// `frame_ready`; the main thread pops + SIGNALs in the
    /// `WlBuffer::Release` Dispatch handler. Indexed by `buffer_index`
    /// so a fan-out renderer (mpv/wescene) signals the right fence
    /// when the compositor releases a specific slot. Vec is grown to
    /// `count` when `bind_buffers` arrives.
    pending_release_fds: Mutex<Vec<VecDeque<OwnedFd>>>,
    /// Live (fourcc → set of modifier) view of what the wlroots
    /// compositor advertised over `zwp_linux_dmabuf_v1` v3
    /// `format`/`modifier` events. Shared with the main thread; the
    /// worker reads a snapshot when it sends `consumer_caps`. We
    /// don't subscribe to feedback v4 yet — v3 broadcasts the full
    /// table on bind, which is delivered before we open a UDS.
    dmabuf_caps: Arc<Mutex<BTreeMap<u32, BTreeSet<u64>>>>,
    /// Set once the worker finished `RegisterDisplay` + `ConsumerCaps`.
    /// Gates the main thread from sending `UpdateDisplay` mid-init.
    registered: AtomicBool,
    /// Serializes wire writes — once the worker is past init the only
    /// other writer is the main thread pushing `UpdateDisplay` on
    /// `Configure`, but holding this protects against any future
    /// concurrent senders too.
    send_lock: Mutex<()>,
    /// Last `(width, height)` we successfully pushed via either
    /// `RegisterDisplay` or `UpdateDisplay`. Skips no-op resends when
    /// `Configure` repeats the same dims.
    last_pushed_size: Mutex<Option<(u32, u32)>>,
    /// Compositor's main DRM render-node, sampled from
    /// `App.compositor_drm_*` at binding creation. Reported in
    /// `RegisterDisplay` so the daemon's DMA-BUF picker can match
    /// renderer GPU == consumer GPU and take the optimized path.
    drm_render_major: u32,
    drm_render_minor: u32,
}

/// One logical output — wl_output plus the layer_surface/UDS worker
/// set we attached to it.
struct OutputEntry {
    wl_output: WlOutput,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    viewport: Option<WpViewport>,
    binding: Option<Arc<OutputBinding>>,
    worker_started: bool,
    /// Latest integer scale from `wl_output::scale`. Sampled into the
    /// binding on first configure. `1` when the event hasn't fired.
    scale: i32,
    /// Per-surface `wp_fractional_scale_v1`, when the compositor
    /// advertised the manager global. Drops on hot-unplug.
    fractional_scale: Option<WpFractionalScaleV1>,
    /// Latest `preferred_scale` in 1/120 units. `0` = not delivered
    /// yet; the configure path falls back to integer `scale` until
    /// the event arrives.
    fractional_scale_120: u32,
}

struct App {
    compositor: Option<WlCompositor>,
    layer_shell: Option<ZwlrLayerShellV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// Optional `wp_viewporter` — if the compositor exposes it, each
    /// surface gets a viewport and we set explicit source/dest rects
    /// every commit. Older compositors without it fall back to
    /// `wl_surface::set_buffer_scale`.
    viewporter: Option<WpViewporter>,
    /// Optional `wp_fractional_scale_manager_v1` — when present we
    /// request a `wp_fractional_scale_v1` per surface and size the
    /// buffer with the exact preferred scale instead of the integer
    /// ceiling reported by `wl_output::scale`. Requires viewporter to
    /// be useful (the buffer is sized in physical pixels and the
    /// viewport maps it back onto the logical surface extent).
    fractional_scale_mgr: Option<WpFractionalScaleManagerV1>,
    /// Default `zwp_linux_dmabuf_feedback_v1` (dmabuf v4+). Kept
    /// alive so its `main_device` events keep landing if the
    /// compositor reassigns mid-session. Decoded GPU is stored below.
    dmabuf_feedback: Option<ZwpLinuxDmabufFeedbackV1>,
    /// Compositor's main DRM render-node, decoded from the dmabuf
    /// feedback `main_device` event. `(0, 0)` = unknown — either the
    /// compositor advertises dmabuf < v4 or the event hasn't fired
    /// yet. Sampled into each `OutputBinding` at construction and
    /// reported in `RegisterDisplay`, so the daemon's DMA-BUF picker
    /// can detect that producer + consumer share a GPU and take the
    /// `OptimizedSameDevice` path (native tiling + DEVICE_LOCAL)
    /// instead of `CompatLinear` (LINEAR + cross-device pessimism).
    compositor_drm_major: u32,
    compositor_drm_minor: u32,
    /// Format table delivered by `wp_linux_dmabuf_feedback_v1`. The
    /// compositor writes a memfd containing 16-byte records:
    /// `{u32 fourcc; u8 _pad[4]; u64 modifier}`. We read it once on
    /// `format_table` and index into it from each `tranche_formats`
    /// event. Empty when no feedback has been delivered (dmabuf < v4
    /// or the compositor hasn't sent the table yet).
    dmabuf_format_table: Vec<(u32, u64)>,
    /// Keyed by `wl_output` global name (u32). The same key is used as
    /// Dispatch user-data for every per-output child proxy so events
    /// find their owning entry in O(1).
    outputs: HashMap<u32, OutputEntry>,
    uds_sock: PathBuf,
    name_prefix: String,
    /// (fourcc → modifier set) accumulated from
    /// `zwp_linux_dmabuf_v1` v3 `format`/`modifier` events. Shared
    /// with every `OutputBinding` so worker threads encode the
    /// compositor's actual modifier set into `consumer_caps` instead
    /// of a hardcoded LINEAR-only fallback.
    dmabuf_caps: Arc<Mutex<BTreeMap<u32, BTreeSet<u64>>>>,
}

impl App {
    fn new(uds_sock: PathBuf, name_prefix: String) -> Self {
        Self {
            compositor: None,
            layer_shell: None,
            dmabuf: None,
            viewporter: None,
            fractional_scale_mgr: None,
            dmabuf_feedback: None,
            compositor_drm_major: 0,
            compositor_drm_minor: 0,
            dmabuf_format_table: Vec::new(),
            outputs: HashMap::new(),
            uds_sock,
            name_prefix,
            dmabuf_caps: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Create the `wl_surface` + layer_surface for a specific output.
    /// Idempotent — skips outputs that already have their surface up.
    fn bring_up_surface(&mut self, output_name: u32, qh: &QueueHandle<App>) {
        let Some(entry) = self.outputs.get_mut(&output_name) else {
            return;
        };
        if entry.surface.is_some() {
            return;
        }
        let (Some(comp), Some(shell)) = (self.compositor.as_ref(), self.layer_shell.as_ref())
        else {
            return;
        };
        let surface = comp.create_surface(qh, output_name);
        let layer_surface = shell.get_layer_surface(
            &surface,
            Some(&entry.wl_output),
            Layer::Background,
            "waywallen-wallpaper".to_string(),
            qh,
            output_name,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.set_size(0, 0);
        // If the compositor advertises wp_viewporter, attach a viewport
        // to this surface so we can map arbitrary buffer regions to
        // arbitrary surface extents (needed for HiDPI + SetConfig).
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&surface, qh, output_name));
        // wp_fractional_scale_v1 is per-surface — request it before
        // commit so `preferred_scale` is delivered alongside the first
        // configure (avoids one round of mis-sizing on startup).
        let fractional_scale = self
            .fractional_scale_mgr
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, qh, output_name));
        surface.commit();
        entry.surface = Some(surface);
        entry.layer_surface = Some(layer_surface);
        entry.viewport = viewport;
        entry.fractional_scale = fractional_scale;
        log::info!("output {output_name}: layer_surface committed, waiting for configure");
    }

    /// Spawn the per-output UDS worker once the compositor has
    /// configured its layer_surface.
    fn maybe_spawn_worker(&mut self, output_name: u32) {
        let Some(entry) = self.outputs.get_mut(&output_name) else {
            return;
        };
        if entry.worker_started {
            return;
        }
        let Some(binding) = entry.binding.as_ref() else {
            return;
        };
        if binding.configured_size.lock().unwrap().is_none() {
            return;
        }
        entry.worker_started = true;
        let binding = Arc::clone(binding);
        let sock = self.uds_sock.clone();
        log::info!(
            "output {output_name}: spawning UDS worker ('{}')",
            binding.display_name
        );
        thread::spawn(move || uds_worker_loop(sock, binding));
    }
}

// --- Dispatch impls -------------------------------------------------------

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_registry::Event;
        match event {
            Event::Global {
                name,
                interface,
                version,
            } => {
                // Runtime hot-plug: only `wl_output` is interesting —
                // compositor / dmabuf / layer_shell singletons don't
                // appear post-startup in any sane setup.
                if interface == "wl_output" {
                    if state.outputs.contains_key(&name) {
                        return;
                    }
                    let wl_output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, name);
                    state.outputs.insert(
                        name,
                        OutputEntry {
                            wl_output,
                            surface: None,
                            layer_surface: None,
                            viewport: None,
                            binding: None,
                            worker_started: false,
                            scale: 1,
                            fractional_scale: None,
                            fractional_scale_120: 0,
                        },
                    );
                    log::info!("hot-plug: wl_output name={name} added; bringing up surface");
                    state.bring_up_surface(name, qh);
                }
            }
            Event::GlobalRemove { name } => {
                if let Some(entry) = state.outputs.remove(&name) {
                    log::info!("hot-unplug: wl_output name={name} removed");
                    // Tear down the worker thread cooperatively:
                    //   1. flip `closed` so the reconnect loop exits
                    //      after its current session.
                    //   2. if the worker is mid-session (blocked on
                    //      `recv_event`), shutdown its UnixStream —
                    //      the kernel unblocks the read and the
                    //      session returns with an error.
                    if let Some(binding) = entry.binding.as_ref() {
                        binding.closed.store(true, Ordering::SeqCst);
                        if let Some(stream) = binding.stream.read().unwrap().clone() {
                            let _ = stream.shutdown(Shutdown::Both);
                        }
                    }
                    drop(entry);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for App {
    fn event(
        _state: &mut Self,
        _p: &WlCompositor,
        _e: wayland_client::protocol::wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, u32> for App {
    fn event(
        _state: &mut Self,
        _p: &WlSurface,
        _e: wayland_client::protocol::wl_surface::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Enter/Leave and surface scale events ignored — the compositor
        // drives layer-surface sizing via configure, and we don't care
        // which seats hover us.
    }
}

impl Dispatch<WlBuffer, (u32, u32)> for App {
    fn event(
        state: &mut Self,
        buffer: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        data: &(u32, u32),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let (output_name, buffer_index) = *data;
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            log::trace!(
                "wl_buffer {} (out={output_name} idx={buffer_index}) released",
                buffer.id()
            );
            // Pop the next pending release_syncobj fd for this slot
            // and SIGNAL it. The daemon's reaper is waiting on this
            // fd; once signaled, it TRANSFERs the fence onto the
            // producer's release timeline so the producer can reuse
            // the buffer for its next submit.
            let Some(binding) = state
                .outputs
                .get(&output_name)
                .and_then(|e| e.binding.as_ref())
            else {
                return;
            };
            let fd = {
                let mut guard = binding.pending_release_fds.lock().unwrap();
                guard
                    .get_mut(buffer_index as usize)
                    .and_then(|q| q.pop_front())
            };
            if let Some(fd) = fd {
                if let Err(e) = signal_release_syncobj(fd) {
                    log::warn!(
                        "[{}] signal release_syncobj on Release(idx={buffer_index}) failed: {e}",
                        binding.display_name
                    );
                }
            } else {
                // Either we received Release before any frame_ready
                // (unlikely but legal — daemon may bind without
                // immediately producing) or our fd queue ran dry
                // because the daemon stopped producing. Either way
                // there's nothing to signal.
                log::trace!(
                    "[{}] Release(idx={buffer_index}) with empty pending fd queue",
                    binding.display_name
                );
            }
        }
    }
}

impl Dispatch<WlCallback, u32> for App {
    fn event(
        state: &mut Self,
        _cb: &WlCallback,
        event: wl_callback::Event,
        data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Compositor signalled that it presented the last commit; it's
        // safe to commit another buffer now. user_data carries the
        // owning wl_output name so we target the right binding.
        if let wl_callback::Event::Done { .. } = event {
            let output_name = *data;
            if let Some(binding) = state
                .outputs
                .get(&output_name)
                .and_then(|e| e.binding.as_ref())
            {
                binding.frame_pending.store(false, Ordering::SeqCst);
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for App {
    fn event(
        state: &mut Self,
        _p: &WlOutput,
        event: wl_output::Event,
        data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Track the integer buffer scale so HiDPI outputs get a
        // physically-sized buffer + viewporter mapping.
        if let wl_output::Event::Scale { factor } = event {
            let output_name = *data;
            if let Some(entry) = state.outputs.get_mut(&output_name) {
                entry.scale = factor.max(1);
                if let Some(binding) = entry.binding.as_ref() {
                    binding.scale.store(factor.max(1), Ordering::SeqCst);
                }
            }
        }
        // Output metadata (Name/Geometry/Mode/Done) is informational;
        // the layer_surface's own Configure event is authoritative.
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for App {
    fn event(
        state: &mut Self,
        _p: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // dmabuf v4 deprecates the v1.format / v1.modifier broadcasts
        // and delivers the format/modifier set + GPU identity via this
        // feedback object instead. Compositors built on smithay
        // (COSMIC) stop emitting v3 events once a client binds at v4,
        // so we MUST ingest everything from here. Events:
        //   - main_device(dev): the compositor's main render-node.
        //   - format_table(fd, size): one-shot memfd of (fourcc, mod)
        //     records. We index into this from tranche_formats.
        //   - tranche_target_device(dev): which GPU the next
        //     tranche_formats applies to. We accept all tranches —
        //     the daemon's picker filters at negotiation time.
        //   - tranche_formats(indices): u16 indices into format_table.
        //   - tranche_flags(uint): scanout/etc; ignored for our path.
        //   - tranche_done / done: terminators; informational.
        match event {
            zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
                if device.len() < 8 {
                    log::warn!(
                        "dmabuf_feedback: main_device {} bytes (want >=8); ignoring",
                        device.len()
                    );
                    return;
                }
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&device[..8]);
                let dev = u64::from_ne_bytes(buf);
                // glibc dev_t encoding (gnu_dev_major / gnu_dev_minor):
                //   major = ((dev >> 8) & 0xfff) | ((dev >> 32) & ~0xfff)
                //   minor = (dev & 0xff)         | ((dev >> 12) & ~0xff)
                let major = (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff_u64)) as u32;
                let minor = ((dev & 0xff) | ((dev >> 12) & !0xff_u64)) as u32;
                log::info!(
                    "dmabuf_feedback: main_device dev_t=0x{dev:x} → DRM render-node {major}:{minor}"
                );
                state.compositor_drm_major = major;
                state.compositor_drm_minor = minor;
            }
            zwp_linux_dmabuf_feedback_v1::Event::FormatTable { fd, size } => {
                let size = size as usize;
                let mut bytes = vec![0u8; size];
                // Read at absolute offset 0 (`pread`), NOT a sequential
                // `read` — the compositor sends one memfd to every
                // client via SCM_RIGHTS, and they all share the same
                // `f_pos`. If a prior client (or the compositor's own
                // write) left the cursor at EOF, sequential reads
                // return zero bytes → tranche_formats fires before any
                // table data lands → fallback to LINEAR. Positional
                // reads are unaffected.
                let file = std::fs::File::from(fd);
                if let Err(e) = file.read_exact_at(&mut bytes, 0) {
                    log::warn!("dmabuf_feedback: format_table read failed: {e}");
                    return;
                }
                if size % 16 != 0 {
                    log::warn!(
                        "dmabuf_feedback: format_table size={size} is not a multiple of 16 \
                         (record size); truncating"
                    );
                }
                let entries: Vec<(u32, u64)> = bytes
                    .chunks_exact(16)
                    .map(|c| {
                        let fourcc = u32::from_ne_bytes(c[0..4].try_into().unwrap());
                        // bytes 4..8 are padding
                        let modifier = u64::from_ne_bytes(c[8..16].try_into().unwrap());
                        (fourcc, modifier)
                    })
                    .collect();
                log::info!(
                    "dmabuf_feedback: format_table loaded {} entries",
                    entries.len()
                );
                state.dmabuf_format_table = entries;
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheFormats { indices } => {
                if state.dmabuf_format_table.is_empty() {
                    log::warn!(
                        "dmabuf_feedback: tranche_formats before format_table; dropping {} bytes",
                        indices.len()
                    );
                    return;
                }
                let table = &state.dmabuf_format_table;
                let table_len = table.len();
                let Ok(mut caps) = state.dmabuf_caps.lock() else {
                    return;
                };
                let mut added = 0usize;
                let mut oor = 0usize;
                for chunk in indices.chunks_exact(2) {
                    let idx = u16::from_ne_bytes([chunk[0], chunk[1]]) as usize;
                    let Some(&(fourcc, modifier)) = table.get(idx) else {
                        oor += 1;
                        continue;
                    };
                    if caps.entry(fourcc).or_default().insert(modifier) {
                        added += 1;
                    }
                }
                log::debug!(
                    "dmabuf_feedback: tranche added {added} new (fourcc,mod) pairs \
                     ({} indices, {oor} out-of-range, table_len={table_len})",
                    indices.len() / 2
                );
            }
            zwp_linux_dmabuf_feedback_v1::Event::TrancheTargetDevice { .. }
            | zwp_linux_dmabuf_feedback_v1::Event::TrancheFlags { .. }
            | zwp_linux_dmabuf_feedback_v1::Event::TrancheDone => {
                // Informational. We accept formats from every tranche.
            }
            zwp_linux_dmabuf_feedback_v1::Event::Done => {
                let count: usize = state
                    .dmabuf_caps
                    .lock()
                    .map(|g| g.values().map(|v| v.len()).sum())
                    .unwrap_or(0);
                let fourccs = state
                    .dmabuf_caps
                    .lock()
                    .map(|g| g.len())
                    .unwrap_or(0);
                log::info!(
                    "dmabuf_feedback: done — caps now hold {fourccs} fourccs, \
                     {count} (fourcc,mod) entries"
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _state: &mut Self,
        _p: &WpFractionalScaleManagerV1,
        _e: wp_fractional_scale_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wp_fractional_scale_manager_v1 emits no events.
    }
}

impl Dispatch<WpFractionalScaleV1, u32> for App {
    fn event(
        state: &mut Self,
        _p: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            let output_name = *data;
            let Some(entry) = state.outputs.get_mut(&output_name) else {
                return;
            };
            entry.fractional_scale_120 = scale;
            let Some(binding) = entry.binding.as_ref() else {
                // No layer_surface configure yet — the configure path
                // will read entry.fractional_scale_120 when it runs.
                log::info!(
                    "output {output_name}: preferred_scale={scale}/120 (cached, pre-configure)"
                );
                return;
            };
            binding.fractional_scale_120.store(scale, Ordering::SeqCst);
            // If we've already been configured, recompute physical and
            // push UpdateDisplay so the daemon resizes its buffer pool.
            let logical = *binding.logical_size.lock().unwrap();
            let Some((lw, lh)) = logical else {
                return;
            };
            let physical = if entry.viewport.is_some() {
                let f = scale as u64;
                (
                    ((lw as u64 * f + 60) / 120) as u32,
                    ((lh as u64 * f + 60) / 120) as u32,
                )
            } else {
                let s = entry.scale.max(1) as u32;
                (lw.saturating_mul(s), lh.saturating_mul(s))
            };
            let prev = *binding.configured_size.lock().unwrap();
            if prev == Some(physical) {
                return;
            }
            *binding.configured_size.lock().unwrap() = Some(physical);
            log::info!(
                "output {output_name}: preferred_scale={scale}/120 → physical {}x{}",
                physical.0,
                physical.1
            );
            let arc_binding = binding.clone();
            if let Err(e) = push_resize_if_registered(&arc_binding, physical) {
                log::warn!("output {output_name}: push update_display failed: {e}");
            }
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for App {
    fn event(
        _state: &mut Self,
        _p: &ZwlrLayerShellV1,
        _e: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for App {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        data: &u32,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let output_name = *data;
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                log::info!("output {output_name}: layer_surface configure {width}x{height}");
                // Ensure the per-output OutputBinding exists, then
                // record the size and kick the worker.
                let Some(entry) = state.outputs.get_mut(&output_name) else {
                    log::warn!("configure for unknown output_name={output_name}");
                    return;
                };
                let binding = entry.binding.get_or_insert_with(|| {
                    let surface = entry
                        .surface
                        .clone()
                        .expect("configure before surface created");
                    let dmabuf = state.dmabuf.clone().expect("configure before dmabuf bind");
                    Arc::new(OutputBinding {
                        display_name: format!("{}-{}", state.name_prefix, output_name),
                        surface,
                        dmabuf,
                        conn: conn.clone(),
                        qh: qh.clone(),
                        output_name,
                        configured_size: Mutex::new(None),
                        logical_size: Mutex::new(None),
                        scale: std::sync::atomic::AtomicI32::new(entry.scale.max(1)),
                        fractional_scale_120: AtomicU32::new(entry.fractional_scale_120),
                        viewport: entry.viewport.clone(),
                        closed: AtomicBool::new(false),
                        stream: RwLock::new(None),
                        frame_pending: AtomicBool::new(false),
                        pending_release_fds: Mutex::new(Vec::new()),
                        dmabuf_caps: state.dmabuf_caps.clone(),
                        registered: AtomicBool::new(false),
                        send_lock: Mutex::new(()),
                        last_pushed_size: Mutex::new(None),
                        drm_render_major: state.compositor_drm_major,
                        drm_render_minor: state.compositor_drm_minor,
                    })
                });
                // `width` / `height` from `configure` are in *logical*
                // (surface-local) coordinates. Compute the physical
                // buffer size:
                //   * If wp_fractional_scale_v1 has delivered a
                //     preferred_scale AND we have viewporter to map the
                //     buffer back, use `logical × scale/120` (rounded).
                //     This matches the compositor's actual fractional
                //     scale — e.g. 2048×1152 logical @ 1.25× → 2560×1440.
                //   * Otherwise fall back to `logical × integer_scale`.
                //     This ceil-rounds (1.25× → 2) and over-allocates,
                //     but it's the only safe option without viewporter.
                let scale = entry.scale.max(1);
                binding.scale.store(scale, Ordering::SeqCst);
                let f120 = entry.fractional_scale_120;
                binding.fractional_scale_120.store(f120, Ordering::SeqCst);
                let physical = if f120 > 0 && entry.viewport.is_some() {
                    let f = f120 as u64;
                    (
                        ((width as u64 * f + 60) / 120) as u32,
                        ((height as u64 * f + 60) / 120) as u32,
                    )
                } else {
                    (
                        width.saturating_mul(scale as u32),
                        height.saturating_mul(scale as u32),
                    )
                };
                *binding.logical_size.lock().unwrap() = Some((width, height));
                *binding.configured_size.lock().unwrap() = Some(physical);
                if physical != (width, height) {
                    log::info!(
                        "output {output_name}: logical {width}x{height} → physical {}x{} (fractional_scale_120={f120}, integer_scale={scale})",
                        physical.0,
                        physical.1
                    );
                }
                // If the worker is already past RegisterDisplay, push
                // the new physical size to the daemon so fillmode/align
                // recompute under the new disp dims. First Configure
                // (worker not yet spawned) is handled by the upcoming
                // RegisterDisplay carrying these same dims.
                let arc_binding = binding.clone();
                if let Err(e) = push_resize_if_registered(&arc_binding, physical) {
                    log::warn!("output {output_name}: push update_display failed: {e}");
                }
                state.maybe_spawn_worker(output_name);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                log::warn!("output {output_name}: layer_surface closed by compositor");
                if let Some(entry) = state.outputs.get_mut(&output_name) {
                    entry.surface = None;
                    entry.layer_surface = None;
                    entry.binding = None;
                    entry.worker_started = false;
                    entry.fractional_scale = None;
                    entry.fractional_scale_120 = 0;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for App {
    fn event(
        state: &mut Self,
        _p: &ZwpLinuxDmabufV1,
        e: zwp_linux_dmabuf_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // v3 advertises every (fourcc, modifier) the compositor
        // accepts via two event streams:
        //   - `format { format }` — a fourcc supported with implicit
        //     modifier only.
        //   - `modifier { format, modifier_hi, modifier_lo }` —
        //     explicit modifier table; sent for every supported
        //     (fourcc, modifier) including LINEAR.
        // We accumulate both into `dmabuf_caps`. Implicit-only fourccs
        // get a synthetic LINEAR entry so the modifier list is never
        // empty for a fourcc the compositor mentioned at all.
        match e {
            zwp_linux_dmabuf_v1::Event::Format { format } => {
                if let Ok(mut g) = state.dmabuf_caps.lock() {
                    g.entry(format)
                        .or_default()
                        .insert(0 /* DRM_FORMAT_MOD_LINEAR */);
                }
            }
            zwp_linux_dmabuf_v1::Event::Modifier {
                format,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = ((modifier_hi as u64) << 32) | (modifier_lo as u64);
                if let Ok(mut g) = state.dmabuf_caps.lock() {
                    g.entry(format).or_default().insert(modifier);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for App {
    fn event(
        _state: &mut Self,
        _p: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwp_linux_buffer_params_v1::Event::Failed = event {
            log::error!("zwp_linux_buffer_params_v1 Failed: dmabuf import rejected");
        }
    }
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(
        _state: &mut Self,
        _p: &WpViewporter,
        _e: wp_viewporter::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wp_viewporter has no events.
    }
}

impl Dispatch<WpViewport, u32> for App {
    fn event(
        _state: &mut Self,
        _p: &WpViewport,
        _e: wp_viewport::Event,
        _data: &u32,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wp_viewport has no events.
    }
}

// ---------------------------------------------------------------------------
// UDS worker — one per output, each an independent daemon display.
// ---------------------------------------------------------------------------

fn uds_worker_loop(sock: PathBuf, binding: Arc<OutputBinding>) {
    loop {
        if binding.closed.load(Ordering::SeqCst) {
            log::info!("[{}] output closed; worker exiting", binding.display_name);
            return;
        }
        let res = run_uds_session(&sock, &binding);
        // Always clear the active stream slot on session exit so the
        // hot-unplug path doesn't shutdown a stale fd on the next
        // connection.
        binding.stream.write().unwrap().take();
        binding.registered.store(false, Ordering::SeqCst);
        binding.last_pushed_size.lock().unwrap().take();
        match res {
            Ok(()) => log::info!("[{}] UDS session ended cleanly", binding.display_name),
            Err(e) => log::warn!("[{}] UDS session error: {e:#}", binding.display_name),
        }
        if binding.closed.load(Ordering::SeqCst) {
            log::info!(
                "[{}] output closed; worker exiting after session end",
                binding.display_name
            );
            return;
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Main-thread call from `Configure`: if the worker has finished
/// `RegisterDisplay`, send `UpdateDisplay` with the new physical dims.
/// Skips if the worker hasn't connected yet, isn't past register, or
/// the size matches what was last pushed. Worker spawn time + the
/// post-handshake catch-up in `run_uds_session` cover the gap when
/// `registered` is still false.
fn push_resize_if_registered(binding: &Arc<OutputBinding>, physical: (u32, u32)) -> Result<()> {
    if !binding.registered.load(Ordering::SeqCst) {
        return Ok(());
    }
    {
        let last = binding.last_pushed_size.lock().unwrap();
        if *last == Some(physical) {
            return Ok(());
        }
    }
    let stream = match binding.stream.read().unwrap().as_ref() {
        Some(s) => s.clone(),
        None => return Ok(()),
    };
    let _g = binding.send_lock.lock().unwrap();
    codec::send_request(
        &stream,
        &ProtoRequest::UpdateDisplay {
            width: physical.0,
            height: physical.1,
            properties: Vec::new(),
        },
        &[],
    )
    .map_err(|e| anyhow!("send update_display: {e}"))?;
    *binding.last_pushed_size.lock().unwrap() = Some(physical);
    log::info!(
        "[{}] pushed update_display {}x{}",
        binding.display_name,
        physical.0,
        physical.1
    );
    Ok(())
}

fn run_uds_session(sock: &Path, binding: &OutputBinding) -> Result<()> {
    let stream =
        Arc::new(UnixStream::connect(sock).with_context(|| format!("connect {}", sock.display()))?);
    // Publish the live stream so the main thread can `shutdown(2)` it
    // on hot-unplug — that unblocks the blocking `recv_event` below.
    *binding.stream.write().unwrap() = Some(stream.clone());
    let stream: &UnixStream = &stream;
    log::info!(
        "[{}] UDS worker connected to {}",
        binding.display_name,
        sock.display()
    );

    {
        let _g = binding.send_lock.lock().unwrap();
        codec::send_request(
            &stream,
            &ProtoRequest::Hello {
                protocol: PROTOCOL_NAME.to_string(),
                client_name: binding.display_name.clone(),
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                client_protocol_version: PROTOCOL_VERSION,
            },
            &[],
        )
        .map_err(|e| anyhow!("send hello: {e}"))?;
    }
    let (welcome, _) = codec::recv_event(&stream).map_err(|e| anyhow!("recv welcome: {e}"))?;
    match welcome {
        ProtoEvent::Welcome { features, .. } => {
            if !features.iter().any(|s| s == "explicit_sync_fd") {
                bail!("server missing explicit_sync_fd feature");
            }
        }
        other => bail!("expected welcome, got opcode {}", other.opcode()),
    }

    let (width, height) = binding
        .configured_size
        .lock()
        .unwrap()
        .expect("worker started before configure");

    {
        let _g = binding.send_lock.lock().unwrap();
        codec::send_request(
            &stream,
            &ProtoRequest::RegisterDisplay {
                name: binding.display_name.clone(),
                // layer-shell is a "system" backend with no per-DE
                // persistent storage of its own; it always sends empty
                // and the daemon falls back to keying settings by name.
                instance_id: String::new(),
                width,
                height,
                refresh_mhz: 60_000,
                // Sourced from `wp_linux_dmabuf_feedback_v1::main_device`
                // (v4+). `(0, 0)` when the compositor advertises dmabuf
                // < v4 — the daemon then falls back to its UNKNOWN path
                // (CompatLinear + HOST_VISIBLE), same behavior as before
                // this fix.
                drm_render_major: binding.drm_render_major,
                drm_render_minor: binding.drm_render_minor,
                properties: Vec::new(),
            },
            &[],
        )
        .map_err(|e| anyhow!("send register_display: {e}"))?;
    }
    *binding.last_pushed_size.lock().unwrap() = Some((width, height));

    let display_id =
        match codec::recv_event(&stream).map_err(|e| anyhow!("recv display_accepted: {e}"))? {
            (ProtoEvent::DisplayAccepted { display_id }, _) => display_id,
            (other, _) => bail!("expected display_accepted, got opcode {}", other.opcode()),
        };

    // Modifier-negotiation v2 caps. layer-shell hands each dma-buf
    // to the wayland compositor; the compositor imports on whatever
    // GPU it owns, so the authoritative per-modifier feedback is
    // what the compositor advertised over `zwp_linux_dmabuf_v1` v3.
    // We snapshot the live (fourcc → modifier set) map captured by
    // the App's dmabuf Dispatch impl and forward the whole thing.
    //
    // Empty map means the compositor exposed dmabuf v3 but never
    // delivered any format/modifier event — extremely unusual but
    // possible if the roundtrip ordering misses them. Fall back to
    // the legacy ABGR/XRGB + LINEAR pair so the daemon still has a
    // viable cross-vendor scheme.
    //
    // device_uuid stays zeros; the picker falls back to the DRM node
    // (also zero — see register_display above), so this consumer is
    // treated as "unknown device" and forces HOST_VISIBLE on every
    // renderer. Wlroots dmabuf-feedback v4 would expose a main_device
    // we could thread through here — left as a follow-up.
    {
        use waywallen::dma::negotiate as N;
        // Flatten dmabuf_caps directly into the wire-format parallel
        // arrays under a single lock acquisition. usages/plane_counts
        // are constant per-modifier here, so bulk-fill them with
        // vec![v; n] instead of pushing in the inner loop.
        let flat = {
            let guard = binding.dmabuf_caps.lock().unwrap();
            if guard.is_empty() {
                None
            } else {
                let n_fourccs = guard.len();
                let total_mods: usize = guard.values().map(|s| s.len()).sum();
                let mut fourccs: Vec<u32> = Vec::with_capacity(n_fourccs);
                let mut mod_counts: Vec<u32> = Vec::with_capacity(n_fourccs);
                let mut modifiers: Vec<u64> = Vec::with_capacity(total_mods);
                for (fourcc, mods) in guard.iter() {
                    fourccs.push(*fourcc);
                    mod_counts.push(mods.len() as u32);
                    modifiers.extend(mods.iter().copied());
                }
                Some((fourccs, mod_counts, modifiers, total_mods))
            }
        };
        let (fourccs, mod_counts, modifiers, plane_counts) = match flat {
            None => {
                log::warn!(
                    "[{}] zwp_linux_dmabuf_v1 exposed no formats — \
                     falling back to ABGR/XRGB + LINEAR consumer_caps",
                    binding.display_name
                );
                (
                    vec![N::DRM_FORMAT_ABGR8888, N::DRM_FORMAT_XRGB8888],
                    vec![1u32, 1],
                    vec![N::DRM_FORMAT_MOD_LINEAR, N::DRM_FORMAT_MOD_LINEAR],
                    vec![1u32, 1],
                )
            }
            Some((fourccs, mod_counts, modifiers, total_mods)) => {
                log::info!(
                    "[{}] consumer_caps: {} fourccs, {} (fourcc,modifier) entries from compositor",
                    binding.display_name,
                    fourccs.len(),
                    modifiers.len()
                );
                let plane_counts = vec![1u32; total_mods];
                (fourccs, mod_counts, modifiers, plane_counts)
            }
        };
        let _g = binding.send_lock.lock().unwrap();
        codec::send_request(
            &stream,
            &ProtoRequest::ConsumerCaps {
                fourccs,
                mod_counts,
                modifiers,
                plane_counts,
                // Vulkan device UUID isn't exposed by the dmabuf v4
                // feedback protocol (`main_device` carries dev_t, not a
                // UUID). Leave the UUID empty and let the picker's
                // `same_device` check fall through to DRM major:minor
                // matching — which now works because we populate
                // drm_render_* below.
                device_uuid: vec![0, 0, 0, 0],
                driver_uuid: vec![0, 0, 0, 0],
                // Critical: the daemon's negotiator builds the consumer
                // `PeerCaps` from THIS message, not `RegisterDisplay`.
                // Reporting 0:0 here defeats the picker's same-device
                // check (DrmNode::UNKNOWN) and forces CompatLinear even
                // when both peers are on the same physical GPU.
                drm_render_major: binding.drm_render_major,
                drm_render_minor: binding.drm_render_minor,
                // The helper hands the dmabuf fd straight to the
                // Wayland compositor; we never touch the memory
                // ourselves. Advertise both so the picker can pick
                // DEVICE_LOCAL when the renderer also supports it —
                // see `pick_mem_hint_same_dev` (negotiate.rs:580).
                // HOST_VISIBLE forces UMA-bandwidth-heavy transfers
                // every frame on iGPUs; DEVICE_LOCAL keeps the buffer
                // in tiled GPU memory across producer→compositor.
                mem_hints: N::MEM_HINT_DEVICE_LOCAL | N::MEM_HINT_HOST_VISIBLE,
                sync_caps: N::SYNC_SYNCOBJ_TIMELINE | N::SYNC_SYNCOBJ_BINARY,
                color_caps: N::DEFAULT_COLOR,
                extent_max_w: 7680,
                extent_max_h: 4320,
            },
            &[],
        )
        .map_err(|e| anyhow!("send consumer_caps: {e}"))?;
    }

    log::info!(
        "[{}] registered as display_id={display_id} ({width}x{height})",
        binding.display_name
    );

    binding.registered.store(true, Ordering::SeqCst);

    // Reconcile: a Configure may have arrived with a new size while we
    // were finishing the handshake; the main thread's push would have
    // been dropped because `registered` was still false. Diff against
    // last_pushed_size and emit one UpdateDisplay if needed.
    let latest = *binding.configured_size.lock().unwrap();
    if let Some(latest) = latest {
        if latest != (width, height) {
            let _g = binding.send_lock.lock().unwrap();
            codec::send_request(
                &stream,
                &ProtoRequest::UpdateDisplay {
                    width: latest.0,
                    height: latest.1,
                    properties: Vec::new(),
                },
                &[],
            )
            .map_err(|e| anyhow!("send catch-up update_display: {e}"))?;
            *binding.last_pushed_size.lock().unwrap() = Some(latest);
            log::info!(
                "[{}] post-handshake size catch-up: {}x{}",
                binding.display_name,
                latest.0,
                latest.1
            );
        }
    }

    let mut gen: Option<u64> = None;
    let mut pool: Vec<WlBuffer> = Vec::new();
    let mut buf_width: u32 = width;
    let mut buf_height: u32 = height;
    let mut frames_presented: u64 = 0;
    // Latest SetConfig values, applied on each FrameReady commit.
    // Units are buffer pixels (source) / surface logical pixels (dest) /
    // wl_output::Transform enum index (transform).
    let mut cfg_source: Option<(f32, f32, f32, f32)> = None;
    let mut cfg_dest_size: Option<(f32, f32)> = None;
    let mut cfg_transform: u32 = 0;
    // Set once on first SetConfig (or first FrameReady with defaults)
    // so we only call `set_buffer_transform` when it actually changes.
    let mut transform_dirty: bool = true;

    loop {
        let (evt, mut fds) = codec::recv_event(&stream).map_err(|e| anyhow!("recv event: {e}"))?;
        match evt {
            ProtoEvent::BindBuffers {
                buffer_generation,
                count,
                width: bw,
                height: bh,
                fourcc,
                modifier,
                planes_per_buffer,
                stride,
                plane_offset,
                ..
            } => {
                let expected = (count * planes_per_buffer) as usize;
                if fds.len() != expected {
                    bail!("bind_buffers expected {} fds, got {}", expected, fds.len());
                }
                if stride.len() != expected || plane_offset.len() != expected {
                    bail!(
                        "bind_buffers stride/offset arrays size mismatch (expected {}, stride={}, offset={})",
                        expected,
                        stride.len(),
                        plane_offset.len()
                    );
                }
                let new_pool = import_dmabufs(
                    binding,
                    count,
                    planes_per_buffer,
                    bw,
                    bh,
                    fourcc,
                    modifier,
                    &stride,
                    &plane_offset,
                    fds,
                )
                .context("import DMA-BUFs")?;
                pool = new_pool;
                gen = Some(buffer_generation);
                buf_width = bw;
                buf_height = bh;
                // Reset the per-slot release_syncobj fd queues. Any
                // pending fds from a prior generation belong to retired
                // wl_buffers the compositor will never Release; drop
                // them now (kernel DESTROY) so we don't keep them
                // around forever. The producer's release timeline is
                // also a per-renderer object, so dropped points are
                // benign — its next submit waits on the new
                // generation's points only.
                {
                    let mut g = binding.pending_release_fds.lock().unwrap();
                    g.clear();
                    g.resize_with(count as usize, VecDeque::new);
                }
                log::info!(
                    "[{}] imported {} wl_buffers for generation {} ({}x{} fourcc=0x{:08x})",
                    binding.display_name,
                    pool.len(),
                    buffer_generation,
                    bw,
                    bh,
                    fourcc
                );
            }
            ProtoEvent::SetConfig {
                source_rect,
                dest_rect,
                transform,
                ..
            } => {
                cfg_source = Some((source_rect.x, source_rect.y, source_rect.w, source_rect.h));
                cfg_dest_size = Some((dest_rect.w, dest_rect.h));
                if cfg_transform != transform {
                    cfg_transform = transform;
                    transform_dirty = true;
                }
                log::debug!(
                    "[{}] set_config src=({:.0},{:.0} {:.0}x{:.0}) dest_size=({:.0}x{:.0}) xform={}",
                    binding.display_name,
                    source_rect.x,
                    source_rect.y,
                    source_rect.w,
                    source_rect.h,
                    dest_rect.w,
                    dest_rect.h,
                    transform
                );
            }
            ProtoEvent::FrameReady {
                buffer_generation: g,
                buffer_index,
                seq,
            } => {
                // fds = [acquire_sync_fd, release_syncobj_fd]
                //   - acquire fence: dropped here (close); the compositor's
                //     own zwp_linux_drm_syncobj integration handles real
                //     acquire sync (or it's implicit via dma-fence on
                //     the buffer).
                //   - release syncobj: queued under `buffer_index`. The
                //     `WlBuffer::Release` Dispatch handler will pop +
                //     SIGNAL when the compositor reports it's done
                //     reading. This is the correct semantic point — the
                //     producer's reuse of this slot now waits on
                //     "compositor finished" rather than the racy
                //     "client received frame_ready".
                if fds.len() == 2 {
                    let release_fd = fds.remove(1);
                    let mut q = binding.pending_release_fds.lock().unwrap();
                    if let Some(slot) = q.get_mut(buffer_index as usize) {
                        slot.push_back(release_fd);
                    } else {
                        // Buffer index out of range for current
                        // generation — daemon protocol bug or rebind
                        // race. Signal immediately so the reaper
                        // doesn't deadlock waiting on a syncobj that
                        // will never fire.
                        log::warn!(
                            "[{}] frame_ready idx={buffer_index} out of range \
                             ({}); signaling release_syncobj eagerly",
                            binding.display_name,
                            q.len(),
                        );
                        drop(q);
                        if let Err(e) = signal_release_syncobj(release_fd) {
                            log::warn!(
                                "[{}] eager signal release_syncobj failed: {e}",
                                binding.display_name
                            );
                        }
                    }
                }
                drop(fds);

                if Some(g) != gen {
                    log::warn!(
                        "[{}] stray frame_ready gen={g}, current={:?}",
                        binding.display_name,
                        gen
                    );
                } else if let Some(buffer) = pool.get(buffer_index as usize) {
                    // Throttle commits to compositor vblank: if the
                    // last frame_callback hasn't fired yet, skip this
                    // commit (but always ack BufferRelease below so
                    // the daemon isn't starved). The compositor will
                    // redraw from whatever buffer is currently
                    // attached.
                    if binding.frame_pending.load(Ordering::SeqCst) {
                        log::trace!(
                            "[{}] skip commit: frame callback pending",
                            binding.display_name
                        );
                    } else {
                        binding.surface.attach(Some(buffer), 0, 0);

                        // Map buffer → surface via wp_viewporter when
                        // available. Source defaults to the full buffer;
                        // SetConfig can crop. Destination defaults to
                        // the logical surface size; SetConfig can shrink.
                        let src =
                            cfg_source.unwrap_or((0.0, 0.0, buf_width as f32, buf_height as f32));
                        let logical = binding
                            .logical_size
                            .lock()
                            .unwrap()
                            .unwrap_or((buf_width, buf_height));
                        let dest = cfg_dest_size.unwrap_or((logical.0 as f32, logical.1 as f32));

                        if let Some(vp) = binding.viewport.as_ref() {
                            // wayland-scanner maps `fixed` args to f64.
                            vp.set_source(src.0 as f64, src.1 as f64, src.2 as f64, src.3 as f64);
                            vp.set_destination(dest.0 as i32, dest.1 as i32);
                        } else {
                            // Fallback: tell the compositor the buffer
                            // is scale× larger than the surface.
                            let scale = binding.scale.load(Ordering::SeqCst);
                            if scale > 1 {
                                binding.surface.set_buffer_scale(scale);
                            }
                        }

                        // Transform — only re-emit when changed.
                        if transform_dirty {
                            binding
                                .surface
                                .set_buffer_transform(map_transform(cfg_transform));
                            transform_dirty = false;
                        }

                        binding
                            .surface
                            .damage_buffer(0, 0, buf_width as i32, buf_height as i32);
                        // Request a frame callback *before* committing
                        // so the callback is tied to this surface
                        // state. user_data = output_name so the
                        // Dispatch impl can find the right binding.
                        binding.surface.frame(&binding.qh, binding.output_name);
                        binding.frame_pending.store(true, Ordering::SeqCst);
                        binding.surface.commit();
                        frames_presented += 1;
                        if let Err(e) = binding.conn.flush() {
                            log::warn!("[{}] wayland flush failed: {e}", binding.display_name);
                        }
                    }
                } else {
                    log::warn!(
                        "[{}] frame_ready buffer_index {} out of range (pool {})",
                        binding.display_name,
                        buffer_index,
                        pool.len()
                    );
                }

                // TODO(release-syncobj): import the release_syncobj fd
                // from `fds[1]` as a drm_syncobj, hook into wl_buffer.release
                // for this slot, and signal the syncobj from there. v1
                // dropped the BufferRelease request — the syncobj signal
                // IS the release. For now both fds are dropped above and
                // the daemon's placeholder reaper does not block on them.
                let _ = (g, buffer_index, seq);
            }
            ProtoEvent::Unbind {
                buffer_generation: g,
            } => {
                if Some(g) == gen {
                    log::info!(
                        "[{}] unbind gen={g}; dropping {} buffers",
                        binding.display_name,
                        pool.len()
                    );
                    pool.clear();
                    gen = None;
                }
            }
            ProtoEvent::Error { code, message } => {
                bail!("server error {code}: {message}");
            }
            _ => {}
        }
    }
}

/// Turn daemon-supplied DMA-BUF fds + per-plane metadata into a pool of
/// `wl_buffer`s via `zwp_linux_buffer_params_v1::create_immed`.
/// Map the daemon's `transform` u32 (matching `wl_output::transform`
/// semantics per `protocol/waywallen_display_v1.xml`) to the
/// wayland-client enum. Unknown values fall back to `Normal` rather
/// than erroring — the daemon owns the protocol and invalid values
/// would break far bigger things.
fn map_transform(t: u32) -> Transform {
    match t {
        0 => Transform::Normal,
        1 => Transform::_90,
        2 => Transform::_180,
        3 => Transform::_270,
        4 => Transform::Flipped,
        5 => Transform::Flipped90,
        6 => Transform::Flipped180,
        7 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

fn import_dmabufs(
    binding: &OutputBinding,
    count: u32,
    planes_per_buffer: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    stride: &[u32],
    plane_offset: &[u32],
    fds: Vec<OwnedFd>,
) -> Result<Vec<WlBuffer>> {
    // Use the main thread's QueueHandle (cloned on the binding) so
    // wl_buffer events — crucially `Release` — land on the queue that
    // the main thread actually polls. A previous version created a
    // throwaway event_queue here and dropped it at function end,
    // which silently routed every `Release` event into a dead queue:
    // the helper never signaled the release_syncobj, the daemon's
    // reaper timed out every wait point ("wait point N timed out /
    // errored (Timer expired); force-signaling stragglers" at ~1 Hz),
    // and the producer was throttled to a 1 fps pipeline.
    let qh = &binding.qh;

    let mut buffers = Vec::with_capacity(count as usize);
    for b in 0..count as usize {
        let params = binding.dmabuf.create_params(qh, ());
        let mod_hi = (modifier >> 32) as u32;
        let mod_lo = (modifier & 0xffff_ffff) as u32;
        for p in 0..planes_per_buffer as usize {
            let idx = b * planes_per_buffer as usize + p;
            let fd: &OwnedFd = &fds[idx];
            params.add(
                fd.as_fd(),
                p as u32,
                plane_offset[idx],
                stride[idx],
                mod_hi,
                mod_lo,
            );
        }
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            // (output_name, buffer_index) — Dispatch::<WlBuffer> uses
            // these to route the Release event back to the right
            // binding's pending_release_fds queue.
            (binding.output_name, b as u32),
        );
        buffers.push(buffer);
    }
    drop(fds);
    Ok(buffers)
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

    let conn = Connection::connect_to_env()
        .context("connect to WAYLAND_DISPLAY — are you running under a Wayland compositor?")?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn).context("registry init")?;
    let qh: QueueHandle<App> = queue.handle();

    let mut app = App::new(args.socket, args.name_prefix);

    // Bind every global we care about. Outputs are collected into the
    // App's `outputs` map keyed by global name; every per-output child
    // proxy carries that name as Dispatch user-data.
    for g in globals.contents().clone_list() {
        match g.interface.as_str() {
            "wl_compositor" => {
                app.compositor = Some(globals.registry().bind::<WlCompositor, _, _>(
                    g.name,
                    g.version.min(6),
                    &qh,
                    (),
                ));
            }
            "zwlr_layer_shell_v1" => {
                app.layer_shell = Some(globals.registry().bind::<ZwlrLayerShellV1, _, _>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    (),
                ));
            }
            "zwp_linux_dmabuf_v1" => {
                // v4 added the feedback object that delivers
                // `main_device` — the compositor's main rendering GPU.
                // Without it we'd report DrmNode::UNKNOWN and the
                // daemon's picker would force CompatLinear (LINEAR
                // modifier, cross-device pessimism) even on single-GPU
                // systems. Bind v4 when offered; v3 still works
                // (format/modifier broadcasts unchanged) — we just lose
                // the GPU identity and stay on the slow path.
                let dmabuf = globals.registry().bind::<ZwpLinuxDmabufV1, _, _>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    (),
                );
                if dmabuf.version() >= 4 {
                    app.dmabuf_feedback = Some(dmabuf.get_default_feedback(&qh, ()));
                }
                app.dmabuf = Some(dmabuf);
            }
            "wp_viewporter" => {
                app.viewporter = Some(globals.registry().bind::<WpViewporter, _, _>(
                    g.name,
                    g.version.min(1),
                    &qh,
                    (),
                ));
            }
            "wp_fractional_scale_manager_v1" => {
                app.fractional_scale_mgr =
                    Some(globals.registry().bind::<WpFractionalScaleManagerV1, _, _>(
                        g.name,
                        g.version.min(1),
                        &qh,
                        (),
                    ));
            }
            "wl_output" => {
                let wl_output = globals.registry().bind::<WlOutput, _, _>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    g.name,
                );
                app.outputs.insert(
                    g.name,
                    OutputEntry {
                        wl_output,
                        surface: None,
                        layer_surface: None,
                        viewport: None,
                        binding: None,
                        worker_started: false,
                        scale: 1,
                        fractional_scale: None,
                        fractional_scale_120: 0,
                    },
                );
            }
            _ => {}
        }
    }

    if app.compositor.is_none() {
        bail!("compositor does not expose wl_compositor");
    }
    if app.layer_shell.is_none() {
        bail!(
            "compositor does not expose zwlr_layer_shell_v1 — \
             try a different compositor (Hyprland/Sway/KWin/new Mutter)"
        );
    }
    if app.dmabuf.is_none() {
        bail!("compositor does not expose zwp_linux_dmabuf_v1");
    }
    if app.outputs.is_empty() {
        bail!("no wl_output available");
    }
    log::info!(
        "bound globals: compositor + layer_shell + dmabuf:v{} + viewporter:{} + fractional_scale:{} + dmabuf_feedback:{} + {} output(s)",
        app.dmabuf.as_ref().map(|d| d.version()).unwrap_or(0),
        app.viewporter.is_some(),
        app.fractional_scale_mgr.is_some(),
        app.dmabuf_feedback.is_some(),
        app.outputs.len()
    );

    // Roundtrip once so every `wl_output` has delivered its initial
    // metadata (Scale / Geometry / Mode / Done) before we create
    // layer-surfaces. Without this, outputs on HiDPI compositors
    // would configure us at logical size with `scale=1` and we'd
    // advertise the wrong physical size to the daemon.
    queue
        .roundtrip(&mut app)
        .context("initial wl_output metadata roundtrip")?;

    // Create the per-output layer_surfaces up-front. The compositor will
    // emit a Configure event for each, which kicks off its UDS worker.
    let output_keys: Vec<u32> = app.outputs.keys().copied().collect();
    for name in output_keys {
        app.bring_up_surface(name, &qh);
    }

    loop {
        if let Err(e) = queue.blocking_dispatch(&mut app) {
            log::error!("wayland dispatch error: {e}");
            return Err(e.into());
        }
    }
}

/// Import the daemon-allocated binary release_syncobj fd into a handle
/// on this process's DRM device, signal it, and drop. The signal
/// unblocks the daemon's reaper which will then TRANSFER the fence
/// onto the producer's release timeline.
///
/// `fd` is consumed (closed by `OwnedFd` drop after `fd_to_handle`
/// imports it — kernel keeps a separate refcount per handle and per
/// fd, so the import is independent of the close).
fn signal_release_syncobj(fd: std::os::fd::OwnedFd) -> anyhow::Result<()> {
    use waywallen::sync::drm_device;
    let dev = drm_device().context("open DRM render node")?;
    let handle = dev
        .fd_to_handle(&fd)
        .context("DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE")?;
    dev.signal(&handle).context("DRM_IOCTL_SYNCOBJ_SIGNAL")?;
    drop(handle); // DESTROY on this process's side; consumer fd already closed via `fd` drop below
    drop(fd);
    Ok(())
}
