//! WebSocket + protobuf control plane.
//!
//! Single `/` endpoint. Each connection carries length-prefixed-by-WS-frame
//! `waywallen.control.v1.Request` / `Response` envelopes. All RPCs are
//! multiplexed via `request_id` and the `payload` oneof.

use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::control;
use crate::control_proto as pb;
use crate::error::{ok_response, Error};
use crate::events::GlobalEvent;
use crate::ipc::proto::ControlMsg;
use crate::model::repo;
use crate::playlist;
use crate::renderer_manager;
use crate::routing::{DisplaySnapshot, LibrarySnapshot, RendererSnapshot, RouterEvent};
use crate::settings::{
    FilterLogicState, SettingsStore, WallpaperAspectFilterState, WallpaperFilterRuleState,
    WallpaperFilterState, WallpaperIntFilterState, WallpaperStringFilterState,
};
use crate::tasks;
use crate::AppState;

/// Bind the WebSocket control plane and return the actual local address
/// (useful when binding to port 0 for OS-assigned ports).  The returned
/// future runs the accept loop and never returns under normal operation.
pub async fn bind(
    state: Arc<AppState>,
    addr: &str,
) -> Result<(
    std::net::SocketAddr,
    impl std::future::Future<Output = Result<()>>,
)> {
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    log::info!("ws control plane listening on {local_addr}");
    let fut = accept_loop(state, listener);
    Ok((local_addr, fut))
}

pub async fn serve(state: Arc<AppState>, addr: &str) -> Result<()> {
    let (_, fut) = bind(state, addr).await?;
    fut.await
}

fn settings_filter_state_from_pb(
    filters: &[pb::WallpaperFilterRule],
    logics: &[pb::FilterLogic],
) -> WallpaperFilterState {
    WallpaperFilterState {
        filters: filters.iter().map(settings_rule_from_pb).collect(),
        filter_logics: logics
            .iter()
            .map(|logic| FilterLogicState {
                op: logic.op,
                group_a: logic.group_a,
                group_b: logic.group_b,
            })
            .collect(),
    }
}

fn settings_rule_from_pb(rule: &pb::WallpaperFilterRule) -> WallpaperFilterRuleState {
    WallpaperFilterRuleState {
        r#type: rule.r#type,
        group: rule.group,
        string_filter: rule.payload.as_ref().and_then(|payload| match payload {
            pb::wallpaper_filter_rule::Payload::StringFilter(f) => {
                Some(WallpaperStringFilterState {
                    value: f.value.clone(),
                    condition: f.condition,
                })
            }
            _ => None,
        }),
        int_filter: rule.payload.as_ref().and_then(|payload| match payload {
            pb::wallpaper_filter_rule::Payload::IntFilter(f) => Some(WallpaperIntFilterState {
                value: f.value,
                condition: f.condition,
            }),
            _ => None,
        }),
        aspect_filter: rule.payload.as_ref().and_then(|payload| match payload {
            pb::wallpaper_filter_rule::Payload::AspectFilter(f) => {
                Some(WallpaperAspectFilterState {
                    value: f.value,
                    condition: f.condition,
                })
            }
            _ => None,
        }),
    }
}

fn pb_rule_from_settings(rule: WallpaperFilterRuleState) -> pb::WallpaperFilterRule {
    let payload = if let Some(f) = rule.string_filter {
        Some(pb::wallpaper_filter_rule::Payload::StringFilter(
            pb::WallpaperStringFilter {
                value: f.value,
                condition: f.condition,
            },
        ))
    } else if let Some(f) = rule.int_filter {
        Some(pb::wallpaper_filter_rule::Payload::IntFilter(
            pb::WallpaperIntFilter {
                value: f.value,
                condition: f.condition,
            },
        ))
    } else {
        rule.aspect_filter.map(|f| {
            pb::wallpaper_filter_rule::Payload::AspectFilter(pb::WallpaperAspectFilter {
                value: f.value,
                condition: f.condition,
            })
        })
    };
    pb::WallpaperFilterRule {
        r#type: rule.r#type,
        group: rule.group,
        payload,
    }
}

async fn accept_loop(state: Arc<AppState>, listener: TcpListener) -> Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(state, stream, peer).await {
                log::warn!("ws conn {peer} ended: {e}");
            }
        });
    }
}

async fn handle_conn(
    state: Arc<AppState>,
    stream: TcpStream,
    peer: std::net::SocketAddr,
) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    log::debug!("ws conn {peer} open");
    let (mut sink, mut src) = ws.split();

    // Subscribe to router events *before* snapshotting so no updates
    // get dropped between the snapshot and the live stream starting.
    let mut events_rx = state.router.subscribe_events();
    // Subscribe to process-wide events (scan lifecycle etc.). Lag here
    // is non-fatal — UI re-fetches on the next event.
    let mut global_rx = state.events.subscribe();
    // Task-lifecycle events feed into `StatusSync` (active task count
    // is one of its fields). Lag is non-fatal; the next push corrects.
    let mut task_rx = state.tasks.subscribe();
    {
        let snap = state.router.snapshot_displays().await;
        let evt = displays_replace_event(snap, &state.settings);
        sink.send(Message::Binary(wrap_event(evt).encode_to_vec()))
            .await?;
    }
    {
        let snap = state.router.snapshot_renderers().await;
        let evt = renderers_replace_event(snap, &state.settings);
        sink.send(Message::Binary(wrap_event(evt).encode_to_vec()))
            .await?;
    }

    {
        let snap = control::list_library_snapshots(&state.db).await;
        let evt = libraries_replace_event(snap);
        sink.send(Message::Binary(wrap_event(evt).encode_to_vec()))
            .await?;
    }
    // Initial daemon-status snapshot. Same wire shape as subsequent
    // pushes so the UI handler is uniform.
    sink.send(Message::Binary(
        wrap_event(status_sync_event(&state)).encode_to_vec(),
    ))
    .await?;

    loop {
        tokio::select! {
            msg = src.next() => {
                let Some(msg) = msg else { break };
                let msg = msg?;
                let bytes = match msg {
                    Message::Binary(b) => b,
                    Message::Text(t) => t.into_bytes(),
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    Message::Frame(_) => continue,
                };

                let req = match pb::Request::decode(&bytes[..]) {
                    Ok(r) => r,
                    Err(e) => {
                        let resp = Error::Decode(e).to_response(0);
                        sink.send(Message::Binary(wrap_response(resp).encode_to_vec())).await?;
                        continue;
                    }
                };

                let resp = dispatch(&state, req).await;
                sink.send(Message::Binary(wrap_response(resp).encode_to_vec())).await?;
            }
            gevt = global_rx.recv() => {
                match gevt {
                    Ok(e) => {
                        if let Some(pe) = global_event_to_pb(&e, &state) {
                            sink.send(Message::Binary(wrap_event(pe).encode_to_vec())).await?;
                        }
                        if matches!(e, GlobalEvent::StatusChanged) {
                            sink.send(Message::Binary(wrap_event(status_sync_event(&state)).encode_to_vec())).await?;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("ws {peer}: global event lag {n}");
                        // Resync after lag — the snapshot is the
                        // authority, transient events were the lossy
                        // notifications.
                        sink.send(Message::Binary(wrap_event(status_sync_event(&state)).encode_to_vec())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Daemon shutting down — let the router-event
                        // arm or the request arm break us out cleanly.
                    }
                }
            }
            tevt = task_rx.recv() => {
                match tevt {
                    Ok(_) => {
                        sink.send(Message::Binary(wrap_event(status_sync_event(&state)).encode_to_vec())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("ws {peer}: task event lag {n}");
                        sink.send(Message::Binary(wrap_event(status_sync_event(&state)).encode_to_vec())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }
            evt = events_rx.recv() => {
                match evt {
                    Ok(e) => {
                        let pe = router_event_to_pb(e, &state.settings);
                        sink.send(Message::Binary(wrap_event(pe).encode_to_vec())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("ws {peer}: event lag {n}; resending full snapshot");
                        let snap = state.router.snapshot_displays().await;
                        let evt = displays_replace_event(snap, &state.settings);
                        sink.send(Message::Binary(wrap_event(evt).encode_to_vec())).await?;
                        let rsnap = state.router.snapshot_renderers().await;
                        let revt = renderers_replace_event(rsnap, &state.settings);
                        sink.send(Message::Binary(wrap_event(revt).encode_to_vec())).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Router shut down; stop emitting but keep the
                        // request path alive until the client closes.
                        log::info!("ws {peer}: router event channel closed");
                        // Drain remaining requests without event select.
                        while let Some(msg) = src.next().await {
                            let msg = msg?;
                            let bytes = match msg {
                                Message::Binary(b) => b,
                                Message::Text(t) => t.into_bytes(),
                                Message::Ping(_) | Message::Pong(_) => continue,
                                Message::Close(_) => break,
                                Message::Frame(_) => continue,
                            };
                            let req = match pb::Request::decode(&bytes[..]) {
                                Ok(r) => r,
                                Err(e) => {
                                    let resp = Error::Decode(e).to_response(0);
                                    sink.send(Message::Binary(wrap_response(resp).encode_to_vec())).await?;
                                    continue;
                                }
                            };
                            let resp = dispatch(&state, req).await;
                            sink.send(Message::Binary(wrap_response(resp).encode_to_vec())).await?;
                        }
                        break;
                    }
                }
            }
        }
    }

    log::debug!("ws conn {peer} closed");
    Ok(())
}

// ---------------------------------------------------------------------------
// RouterEvent → pb::Event translation
// ---------------------------------------------------------------------------

fn display_snapshot_to_pb(s: DisplaySnapshot, settings: &SettingsStore) -> pb::DisplayInfo {
    let resolved = settings.resolved_layout(&s.name);
    let override_prefs = settings.display_prefs(&s.name).unwrap_or_default();
    pb::DisplayInfo {
        display_id: s.id,
        name: s.name,
        width: s.width,
        height: s.height,
        refresh_mhz: s.refresh_mhz,
        links: s
            .links
            .into_iter()
            .map(|l| pb::DisplayLinkInfo {
                renderer_id: l.renderer_id,
                z_order: l.z_order,
            })
            .collect(),
        effective_layout: Some(layout_prefs_to_pb_resolved(&resolved)),
        layout_override: Some(layout_override_to_pb(&override_prefs)),
    }
}

fn layout_prefs_to_pb_resolved(r: &crate::settings::ResolvedLayout) -> pb::LayoutPrefs {
    pb::LayoutPrefs {
        fillmode: fillmode_to_pb(r.fillmode) as i32,
        align: align_to_pb(r.align) as i32,
        clear_rgba: r.clear_rgba.to_vec(),
    }
}

fn layout_override_to_pb(p: &crate::settings::DisplayPrefs) -> pb::LayoutOverride {
    pb::LayoutOverride {
        fillmode_set: p.fillmode.is_some(),
        fillmode: p
            .fillmode
            .map(fillmode_to_pb)
            .unwrap_or(pb::FillMode::Unspecified) as i32,
        align_set: p.align.is_some(),
        align: p.align.map(align_to_pb).unwrap_or(pb::Align::Unspecified) as i32,
        clear_rgba_set: p.clear_rgba.is_some(),
        clear_rgba: p.clear_rgba.map(|v| v.to_vec()).unwrap_or_default(),
    }
}

fn render_size_policy_to_pb(p: crate::settings::RenderSizePolicy) -> pb::RenderSizePolicy {
    use crate::settings::RenderSizePolicy as P;
    match p {
        P::Native => pb::RenderSizePolicy::Native,
        P::OneAxisAuto => pb::RenderSizePolicy::OneAxisAuto,
        P::OneAxisWidth => pb::RenderSizePolicy::OneAxisWidth,
        P::OneAxisHeight => pb::RenderSizePolicy::OneAxisHeight,
    }
}

fn render_size_policy_from_pb(v: i32) -> crate::settings::RenderSizePolicy {
    use crate::settings::RenderSizePolicy as P;
    match pb::RenderSizePolicy::try_from(v) {
        Ok(pb::RenderSizePolicy::Native) => P::Native,
        Ok(pb::RenderSizePolicy::OneAxisWidth) => P::OneAxisWidth,
        Ok(pb::RenderSizePolicy::OneAxisHeight) => P::OneAxisHeight,
        // `OneAxisAuto` is the proto-default (tag 0) and the fallback
        // when an unknown enum value lands on a newer wire from an
        // older daemon.
        _ => P::OneAxisAuto,
    }
}

fn fillmode_to_pb(fm: crate::display_layout::FillMode) -> pb::FillMode {
    use crate::display_layout::FillMode as F;
    match fm {
        F::Stretched => pb::FillMode::Stretched,
        F::PreserveAspectFit => pb::FillMode::PreserveAspectFit,
        F::PreserveAspectCrop => pb::FillMode::PreserveAspectCrop,
        F::Tiled => pb::FillMode::Tiled,
        F::TiledOnlyHorizontally => pb::FillMode::TiledOnlyHorizontal,
        F::TiledOnlyVertically => pb::FillMode::TiledOnlyVertical,
        F::Centered => pb::FillMode::Centered,
    }
}

fn fillmode_from_pb(v: i32) -> Option<crate::display_layout::FillMode> {
    use crate::display_layout::FillMode as F;
    match pb::FillMode::try_from(v).ok()? {
        pb::FillMode::Unspecified => None,
        pb::FillMode::Stretched => Some(F::Stretched),
        pb::FillMode::PreserveAspectFit => Some(F::PreserveAspectFit),
        pb::FillMode::PreserveAspectCrop => Some(F::PreserveAspectCrop),
        pb::FillMode::Tiled => Some(F::Tiled),
        pb::FillMode::TiledOnlyHorizontal => Some(F::TiledOnlyHorizontally),
        pb::FillMode::TiledOnlyVertical => Some(F::TiledOnlyVertically),
        pb::FillMode::Centered => Some(F::Centered),
    }
}

fn align_to_pb(a: crate::display_layout::Align) -> pb::Align {
    use crate::display_layout::Align as A;
    match a {
        A::TopLeft => pb::Align::TopLeft,
        A::Top => pb::Align::Top,
        A::TopRight => pb::Align::TopRight,
        A::Left => pb::Align::Left,
        A::Center => pb::Align::Center,
        A::Right => pb::Align::Right,
        A::BottomLeft => pb::Align::BottomLeft,
        A::Bottom => pb::Align::Bottom,
        A::BottomRight => pb::Align::BottomRight,
    }
}

fn align_from_pb(v: i32) -> Option<crate::display_layout::Align> {
    use crate::display_layout::Align as A;
    match pb::Align::try_from(v).ok()? {
        pb::Align::Unspecified => None,
        pb::Align::TopLeft => Some(A::TopLeft),
        pb::Align::Top => Some(A::Top),
        pb::Align::TopRight => Some(A::TopRight),
        pb::Align::Left => Some(A::Left),
        pb::Align::Center => Some(A::Center),
        pb::Align::Right => Some(A::Right),
        pb::Align::BottomLeft => Some(A::BottomLeft),
        pb::Align::Bottom => Some(A::Bottom),
        pb::Align::BottomRight => Some(A::BottomRight),
    }
}

fn clear_rgba_from_pb(v: &[f32]) -> Option<[f32; 4]> {
    if v.len() == 4 {
        Some([v[0], v[1], v[2], v[3]])
    } else {
        None
    }
}

fn displays_replace_event(snap: Vec<DisplaySnapshot>, settings: &SettingsStore) -> pb::Event {
    pb::Event {
        payload: Some(pb::event::Payload::DisplaySnapshot(pb::DisplaySnapshot {
            displays: snap
                .into_iter()
                .map(|s| display_snapshot_to_pb(s, settings))
                .collect(),
        })),
    }
}

fn renderer_snapshot_to_pb(s: RendererSnapshot, settings: &SettingsStore) -> pb::RendererInstance {
    let fps: u32 = settings
        .plugin(&s.name)
        .and_then(|kv| kv.get("fps").and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    pb::RendererInstance {
        renderer_id: s.id,
        fps,
        status: s.status.as_str().to_string(),
        name: s.name,
        pid: s.pid,
    }
}

fn renderers_replace_event(snap: Vec<RendererSnapshot>, settings: &SettingsStore) -> pb::Event {
    pb::Event {
        payload: Some(pb::event::Payload::RendererSnapshot(pb::RendererSnapshot {
            renderers: snap
                .into_iter()
                .map(|s| renderer_snapshot_to_pb(s, settings))
                .collect(),
        })),
    }
}

fn library_instance_to_pb(s: LibrarySnapshot) -> pb::LibraryInstance {
    pb::LibraryInstance {
        id: s.id,
        path: s.path,
        plugin_name: s.plugin_name,
    }
}

fn libraries_replace_event(snap: Vec<LibrarySnapshot>) -> pb::Event {
    pb::Event {
        payload: Some(pb::event::Payload::LibrarySnapshot(pb::LibrarySnapshot {
            libraries: snap.into_iter().map(library_instance_to_pb).collect(),
        })),
    }
}

fn router_event_to_pb(e: RouterEvent, settings: &SettingsStore) -> pb::Event {
    match e {
        RouterEvent::DisplayUpsert(s) => pb::Event {
            payload: Some(pb::event::Payload::DisplayChanged(pb::DisplayChanged {
                display: Some(display_snapshot_to_pb(s, settings)),
            })),
        },
        RouterEvent::DisplayRemoved(id) => pb::Event {
            payload: Some(pb::event::Payload::DisplayRemoved(pb::DisplayRemoved {
                display_id: id,
            })),
        },
        RouterEvent::DisplaysReplace(list) => displays_replace_event(list, settings),
        RouterEvent::RendererUpsert(s) => pb::Event {
            payload: Some(pb::event::Payload::RendererChanged(pb::RendererChanged {
                renderer: Some(renderer_snapshot_to_pb(s, settings)),
            })),
        },
        RouterEvent::RendererRemoved(id) => pb::Event {
            payload: Some(pb::event::Payload::RendererRemoved(pb::RendererRemoved {
                renderer_id: id,
            })),
        },
        RouterEvent::RenderersReplace(list) => renderers_replace_event(list, settings),
        RouterEvent::LibraryUpsert(s) => pb::Event {
            payload: Some(pb::event::Payload::LibraryChanged(pb::LibraryChanged {
                library: Some(library_instance_to_pb(s)),
            })),
        },
        RouterEvent::LibraryRemoved(id) => pb::Event {
            payload: Some(pb::event::Payload::LibraryRemoved(pb::LibraryRemoved {
                id,
            })),
        },
        RouterEvent::LibrariesReplace(list) => libraries_replace_event(list),
    }
}

/// Snapshot daemon-side runtime state into a `StatusSync` server event.
/// Pushed on WS connect, on every `GlobalEvent::StatusChanged`, and on
/// any `TaskEvent`. Authoritative — the UI binds to its fields rather
/// than counting transient start/end events.
fn status_sync_event(state: &Arc<AppState>) -> pb::Event {
    use std::sync::atomic::Ordering;
    let scan_in_progress = state.scan_in_progress.load(Ordering::SeqCst);
    let active_task_count = state
        .tasks
        .list()
        .into_iter()
        .filter(|r| matches!(r.state, tasks::TaskState::Running))
        .count() as u32;
    pb::Event {
        payload: Some(pb::event::Payload::StatusSync(pb::StatusSync {
            scan_in_progress,
            active_task_count,
        })),
    }
}

/// Translate the subset of `GlobalEvent` variants the UI cares about
/// into wire `pb::Event`s. Returns `None` for events that are
/// daemon-internal (boot phase markers, restore lifecycle).
fn global_event_to_pb(e: &GlobalEvent, state: &Arc<AppState>) -> Option<pb::Event> {
    match e {
        GlobalEvent::ScanStarted => Some(pb::Event {
            payload: Some(pb::event::Payload::WallpaperScanStarted(
                pb::WallpaperScanStarted {},
            )),
        }),
        GlobalEvent::ScanCompleted { count } => Some(pb::Event {
            payload: Some(pb::event::Payload::WallpaperScanCompleted(
                pb::WallpaperScanCompleted {
                    count: *count as u32,
                    error: String::new(),
                },
            )),
        }),
        GlobalEvent::ScanFailed(msg) => Some(pb::Event {
            payload: Some(pb::event::Payload::WallpaperScanCompleted(
                pb::WallpaperScanCompleted {
                    count: 0,
                    error: msg.clone(),
                },
            )),
        }),
        GlobalEvent::LibrariesAdded { paths } => Some(pb::Event {
            payload: Some(pb::event::Payload::LibrariesAdded(pb::LibrariesAdded {
                paths: paths.clone(),
            })),
        }),
        GlobalEvent::SettingsChanged => {
            let snap = state.settings.snapshot();
            let filter_state = snap.global.wallpaper_filter.clone();
            let layout_defaults = pb::LayoutPrefs {
                fillmode: fillmode_to_pb(snap.global.layout.fillmode) as i32,
                align: align_to_pb(snap.global.layout.align) as i32,
                clear_rgba: snap.global.layout.clear_rgba.to_vec(),
            };
            Some(pb::Event {
                payload: Some(pb::event::Payload::SettingsChanged(pb::SettingsChanged {
                    global: Some(pb::GlobalSettings {
                        target_extent: snap.global.target_extent,
                        render_size_policy: render_size_policy_to_pb(snap.global.render_size_policy)
                            as i32,
                        wallpaper_filters: filter_state
                            .filters
                            .into_iter()
                            .map(pb_rule_from_settings)
                            .collect(),
                        wallpaper_filter_logics: filter_state
                            .filter_logics
                            .into_iter()
                            .map(|logic| pb::FilterLogic {
                                op: logic.op,
                                group_a: logic.group_a,
                                group_b: logic.group_b,
                            })
                            .collect(),
                        layout_defaults: Some(layout_defaults),
                    }),
                    plugins: snap
                        .plugins
                        .into_iter()
                        .map(|(k, v)| (k, pb::PluginSettings { values: v }))
                        .collect(),
                })),
            })
        }
        GlobalEvent::SourcesReady
        | GlobalEvent::DisplayReady
        | GlobalEvent::RestoreApplied(_)
        | GlobalEvent::RestoreFailed(_)
        | GlobalEvent::StatusChanged => None,
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch(state: &Arc<AppState>, req: pb::Request) -> pb::Response {
    let rid = req.request_id;
    build_response(rid, dispatch_inner(state, req).await)
}

async fn dispatch_inner(
    state: &Arc<AppState>,
    req: pb::Request,
) -> Result<pb::response::Payload, Error> {
    let payload = req
        .payload
        .ok_or(Error::UnexpectedPayload("empty request payload"))?;

    use pb::request::Payload as Req;
    use pb::response::Payload as Res;

    Ok(match payload {
        Req::Health(_) => Res::Health(pb::HealthResponse {
            service: "waywallen".into(),
            state: "healthy".into(),
        }),

        Req::RendererSpawn(r) => {
            // Low-level RPC: caller hands in a single `metadata` map.
            // Treat it as both the CLI argv extras (path / assets / …)
            // and the Init.settings kv. This is loose by design — the
            // RPC is for advanced/manual use; `WallpaperApply` is the
            // intended end-user path and sources settings cleanly from
            // the settings store.
            //
            // `r.fps` rides on `settings["fps"]` so the renderer sees
            // it via `Init.settings` — the typed scalar is gone.
            let mut settings = r.metadata.clone();
            if r.fps != 0 {
                settings.insert("fps".to_string(), r.fps.to_string());
            }
            let spawn_req = renderer_manager::SpawnRequest {
                wp_type: if r.wp_type.is_empty() {
                    "scene".into()
                } else {
                    r.wp_type
                },
                extras: r.metadata,
                settings,
                width: r.width,
                height: r.height,
                extent_mode: crate::settings::extent_mode::AS_GIVEN,
                test_pattern: false,
                renderer_name: None,
            };
            // renderer_manager returns the typed Error directly (spawn
            // produces RendererSpawnFailed/NoRendererForType/RendererNotFound
            // depending on the failure); just propagate.
            let id = state.renderer_manager.spawn(spawn_req).await?;
            if let Some(handle) = state.renderer_manager.get(&id).await {
                state.router.register_renderer(handle).await;
            }
            Res::RendererSpawn(pb::RendererSpawnResponse { renderer_id: id })
        }

        Req::RendererList(_) => {
            let ids = state.renderer_manager.list().await;
            let mut instances = Vec::with_capacity(ids.len());
            for id in &ids {
                let (name, pid) = match state.renderer_manager.get(id).await {
                    Some(h) => (h.name.clone(), h.pid.unwrap_or(0)),
                    None => (String::new(), 0),
                };
                // fps lives in the plugin section of the settings store
                // now (`Settings::reconcile` already enforces the
                // schema). 0 = unknown / unset.
                let fps: u32 = state
                    .settings
                    .plugin(&name)
                    .and_then(|kv| kv.get("fps").and_then(|v| v.parse().ok()))
                    .unwrap_or(0);
                let status = if state.router.is_paused(id).await {
                    "paused"
                } else {
                    "playing"
                };
                instances.push(pb::RendererInstance {
                    renderer_id: id.clone(),
                    fps,
                    status: status.into(),
                    name,
                    pid,
                });
            }
            Res::RendererList(pb::RendererListResponse {
                renderers: ids,
                instances,
            })
        }

        Req::RendererPlay(r) => {
            state
                .renderer_manager
                .send_control(&r.renderer_id, ControlMsg::Play)
                .await?;
            Res::RendererPlay(pb::Empty {})
        }

        Req::RendererPause(r) => {
            state
                .renderer_manager
                .send_control(&r.renderer_id, ControlMsg::Pause)
                .await?;
            Res::RendererPause(pb::Empty {})
        }

        Req::RendererMouse(r) => {
            state
                .renderer_manager
                .send_control(&r.renderer_id, ControlMsg::Mouse { x: r.x, y: r.y })
                .await?;
            Res::RendererMouse(pb::Empty {})
        }

        Req::RendererFps(r) => {
            state
                .renderer_manager
                .send_control(&r.renderer_id, ControlMsg::SetFps { fps: r.fps })
                .await?;
            Res::RendererFps(pb::Empty {})
        }

        Req::RendererKill(r) => {
            state.router.unregister_renderer(&r.renderer_id).await;
            state.renderer_manager.kill(&r.renderer_id).await?;
            Res::RendererKill(pb::Empty {})
        }

        Req::RendererPluginList(_) => {
            let registry = state.renderer_manager.registry();
            let renderers = registry
                .all_renderers()
                .iter()
                .map(|def| {
                    let mut settings: Vec<pb::SettingSchema> = def
                        .settings
                        .iter()
                        .map(|(k, v)| crate::control_proto::setting_def_to_proto(k, v))
                        .collect();
                    // Stable order so UIs can rely on deterministic
                    // layout: by manifest `order` then key name.
                    settings.sort_by(|a, b| a.order.cmp(&b.order).then(a.key.cmp(&b.key)));
                    pb::RendererPluginInfo {
                        name: def.name.clone(),
                        bin: def.bin.to_string_lossy().into_owned(),
                        types: def.types.iter().map(|t| t.to_string()).collect(),
                        priority: def.priority,
                        version: def.version.clone(),
                        settings,
                    }
                })
                .collect();
            let supported_types = registry.supported_types().into_iter().cloned().collect();
            Res::RendererPluginList(pb::RendererPluginListResponse {
                renderers,
                supported_types,
            })
        }

        Req::WallpaperList(r) => {
            // Read from the snapshot mirror (see `source_snapshot.rs`)
            // so an in-flight scan does not block this query.
            let snap = state.source_snapshot.read().await;

            // Build a lookup map: (library.path, item.path) -> item::Model
            // so we can overlay DB media-meta (size/width/height/format) onto
            // each WallpaperEntry before sending it to the UI. DB read
            // failures propagate as `Internal` (via anyhow::Error From)
            // instead of being silently dropped — the caller sees the
            // actual problem.
            let db_meta_map: std::collections::HashMap<
                (String, String),
                crate::model::entities::item::Model,
            > = {
                let libs = repo::list_libraries(&state.db).await?;
                let lib_path_by_id: std::collections::HashMap<i64, String> =
                    libs.into_iter().map(|l| (l.id, l.path)).collect();
                let items = repo::list_items_all(&state.db).await?;
                items
                    .into_iter()
                    .filter_map(|it| {
                        let lib_path = lib_path_by_id.get(&it.library_id)?.clone();
                        let item_path = it.path.clone();
                        Some(((lib_path, item_path), it))
                    })
                    .collect()
            };

            let raw_entries: Vec<&crate::wallpaper_type::WallpaperEntry> = if r.wp_type.is_empty() {
                snap.list().iter().collect()
            } else {
                snap.list_by_type(&r.wp_type)
            };

            let matched_keys = if r.filters.is_empty() {
                None
            } else {
                Some(
                    repo::list_item_keys_by_wallpaper_filters(
                        &state.db,
                        &r.filters,
                        &r.filter_logics,
                    )
                    .await?
                    .into_iter()
                    .collect::<std::collections::HashSet<(String, String)>>(),
                )
            };

            let filtered_entries: Vec<&crate::wallpaper_type::WallpaperEntry> =
                if let Some(matched_keys) = matched_keys.as_ref() {
                    raw_entries
                        .into_iter()
                        .filter(|e| {
                            crate::model::sync::relative_under_root(&e.library_root, &e.resource)
                                .map(|rel| matched_keys.contains(&(e.library_root.clone(), rel)))
                                .unwrap_or(false)
                        })
                        .collect()
                } else {
                    raw_entries
                };

            let total = filtered_entries.len() as u32;
            let page_size = r.page_size as usize;
            let (offset, take) = if page_size == 0 {
                (0usize, filtered_entries.len())
            } else {
                ((r.page as usize) * page_size, page_size)
            };

            let entries: Vec<pb::WallpaperEntry> = filtered_entries
                .into_iter()
                .skip(offset)
                .take(take)
                .map(|e| {
                    let db_meta =
                        crate::model::sync::relative_under_root(&e.library_root, &e.resource)
                            .and_then(|rel| db_meta_map.get(&(e.library_root.clone(), rel)));
                    entry_to_pb(e, db_meta)
                })
                .collect();

            Res::WallpaperList(pb::WallpaperListResponse {
                wallpapers: entries,
                count: total,
            })
        }

        Req::WallpaperScan(_) => {
            // Fire-and-forget: kick the rescan onto the TaskManager and
            // return immediately. Completion (or failure) reaches the
            // UI via the `WallpaperScanCompleted` server event, so the
            // request is just an ack. `spawn_async_unique` collapses
            // overlapping triggers (rapid clicks, library churn) under
            // one in-flight scan.
            let scan_state = state.clone();
            state.tasks.spawn_async_unique(
                tasks::TaskKind::Generic,
                "scan/refresh",
                "scan/refresh",
                async move {
                    control::refresh_sources(&scan_state)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                },
            );
            Res::WallpaperScan(pb::WallpaperScanResponse { count: 0 })
        }

        Req::SourceList(_) => {
            let snap = state.source_snapshot.read().await;
            let sources = snap
                .plugins()
                .iter()
                .cloned()
                .map(|p| pb::SourcePluginInfo {
                    name: p.name,
                    types: p.types,
                    version: p.version,
                })
                .collect();
            Res::SourceList(pb::SourceListResponse { sources })
        }

        Req::DisplayList(_) => {
            let snap = state.router.snapshot_displays().await;
            let displays = snap
                .into_iter()
                .map(|d| display_snapshot_to_pb(d, &state.settings))
                .collect();
            Res::DisplayList(pb::DisplayListResponse { displays })
        }

        Req::DisplayLayoutSet(r) => {
            let new_fillmode = if r.clear_fillmode {
                None
            } else {
                r.r#override
                    .as_ref()
                    .filter(|o| o.fillmode_set)
                    .and_then(|o| fillmode_from_pb(o.fillmode))
            };
            let new_align = if r.clear_align {
                None
            } else {
                r.r#override
                    .as_ref()
                    .filter(|o| o.align_set)
                    .and_then(|o| align_from_pb(o.align))
            };
            let new_clear_rgba = if r.clear_clear_rgba {
                None
            } else {
                r.r#override
                    .as_ref()
                    .filter(|o| o.clear_rgba_set)
                    .and_then(|o| clear_rgba_from_pb(&o.clear_rgba))
            };
            state
                .router
                .set_display_layout(
                    r.name.clone(),
                    new_fillmode,
                    new_align,
                    new_clear_rgba,
                    r.clear_fillmode,
                    r.clear_align,
                    r.clear_clear_rgba,
                )
                .await;
            // Look up the (possibly absent) DisplayInfo to return.
            let snap = state.router.snapshot_displays().await;
            let display = snap
                .into_iter()
                .find(|d| d.name == r.name)
                .map(|d| display_snapshot_to_pb(d, &state.settings));
            Res::DisplayLayoutSet(pb::DisplayLayoutSetResponse { display })
        }

        Req::WallpaperApply(r) => {
            let entry = {
                let snap = state.source_snapshot.read().await;
                snap.get(&r.wallpaper_id).cloned()
            };
            let entry = entry.ok_or_else(|| Error::WallpaperNotFound(r.wallpaper_id.clone()))?;
            if state.router.display_count().await == 0 {
                return Err(Error::NoDisplayRegistered);
            }
            // Renderer pick: empty renderer_name uses priority resolve
            // (current behaviour); explicit name must match a registered
            // renderer that supports this wallpaper's type.
            let registry = state.renderer_manager.registry();
            let plugin_name: String = if r.renderer_name.is_empty() {
                registry
                    .resolve(&entry.wp_type)
                    .map(|def| def.name.clone())
                    .ok_or_else(|| Error::NoRendererForType(entry.wp_type.clone()))?
            } else {
                let def = registry
                    .resolve_by_name(&r.renderer_name)
                    .ok_or_else(|| Error::RendererNotFound(r.renderer_name.clone()))?;
                if !def.types.iter().any(|t| t == &entry.wp_type) {
                    return Err(Error::RendererTypeMismatch {
                        renderer: r.renderer_name.clone(),
                        ty: entry.wp_type.clone(),
                    });
                }
                def.name.clone()
            };
            // Render-target hint comes from settings.global. The
            // policy translates `target_extent` into the wire-level
            // `(extent_w, extent_h, extent_mode)` triple — see
            // `crate::settings::resolve_extent`. fps is a per-plugin
            // concern: pulled out of `[plugin.<name>].fps` if present,
            // otherwise hardcoded to 30 as a safe last resort. The
            // remaining `[plugin.<name>]` keys flow into spawn metadata
            // as baseline kv; per-wallpaper metadata wins on collisions.
            let g = state.settings.global();
            let (width, height, extent_mode) =
                crate::settings::resolve_extent(g.render_size_policy, g.target_extent);

            let plugin_kv = state.settings.plugin(&plugin_name).unwrap_or_default();

            // SPAWN_VERSION 3: extras (canonical `path` + manifest
            // whitelist like `assets`/`workshop_id`) ride as CLI
            // argv. Ask the source plugin for the dict via its
            // `extras(entry)` Lua callback. Lua failures used to fall
            // back to `entry.metadata` silently with a warn; now they
            // surface as `SourceExtrasFailed` so the UI shows the real
            // problem instead of a confusing "wrong settings" follow-up.
            let extras = state
                .source_manager
                .lock()
                .await
                .call_extras(&entry.plugin_name, &entry)
                .await?;

            let spawn_req = renderer_manager::SpawnRequest {
                wp_type: entry.wp_type.clone(),
                extras,
                settings: plugin_kv,
                width,
                height,
                extent_mode,
                test_pattern: false,
                // Pin reuse + spawn to the explicit pick when the
                // request named one; otherwise let the manager fall
                // back to priority resolve (legacy behaviour).
                renderer_name: if r.renderer_name.is_empty() {
                    None
                } else {
                    Some(plugin_name.clone())
                },
            };

            // Reuse an existing renderer when wp_type / extent / plugin
            // / extras (path) all match and the handle hasn't been
            // marked stale by an identity-tagged SettingsSet. Plugin
            // settings live in the settings store and are pushed by
            // SettingsSet's own broadcast, so the reuse path doesn't
            // need to dispatch ApplySettings here.
            let renderer_id = match state.renderer_manager.find_reusable(&spawn_req).await {
                Some(existing_id) => {
                    log::info!(
                        "wallpaper_apply: reusing renderer {existing_id} for wallpaper {}",
                        entry.id
                    );
                    existing_id
                }
                None => {
                    // No reuse — a fresh renderer is about to spawn.
                    // Stop any pre-existing renderer whose every
                    // enabled display link is in the relink target
                    // set BEFORE the new one is spawned, so peak GPU
                    // memory stays at one working set's worth instead
                    // of overlapping two.
                    let target: Option<&[u64]> = if r.display_ids.is_empty() {
                        None
                    } else {
                        Some(&r.display_ids)
                    };
                    let to_stop = state.router.renderers_fully_replaced_by(target).await;
                    if !to_stop.is_empty() {
                        log::info!(
                            "wallpaper_apply: stopping {} fully-replaced renderer(s) before spawn: {:?}",
                            to_stop.len(),
                            to_stop,
                        );
                        state.router.stop_renderers(&to_stop).await;
                    }
                    let new_id = state.renderer_manager.spawn(spawn_req).await?;
                    if let Some(handle) = state.renderer_manager.get(&new_id).await {
                        state.router.register_renderer(handle).await;
                    }
                    new_id
                }
            };

            // Relink: empty display_ids means "all currently registered
            // displays" (pre-M4 behaviour). Old renderers left with
            // zero links get paused immediately and reclaimed by the
            // router's idle reaper after IDLE_KILL_TIMEOUT.
            if r.display_ids.is_empty() {
                state.router.relink_all_displays_to(&renderer_id).await;
            } else {
                state
                    .router
                    .relink_displays_to(&r.display_ids, &renderer_id)
                    .await;
            }

            // Mirror the persistence side-effects that
            // `control::apply_wallpaper_by_id` performs: locate the
            // playlist cursor and persist `last_wallpaper` so the
            // next daemon start can restore. Without this the WS
            // apply path silently bypasses persistence (D-Bus + the
            // rotator both go through `control::apply_wallpaper_by_id`
            // and don't have this hole).
            {
                let mut playlist = state.playlist.lock().await;
                playlist.locate(&entry.id);
                playlist.current = Some(entry.id.clone());
            }
            state.settings.update(|s| {
                s.global.last_wallpaper = Some(entry.id.clone());
            });
            state.settings.flush_now().await;
            // Reset the rotator deadline so a manual apply gets the
            // full quiet window before the next auto tick.
            state.rotation.kick();

            Res::WallpaperApply(pb::WallpaperApplyResponse {
                renderer_id,
                wallpaper_id: entry.id,
                wp_type: entry.wp_type,
                name: entry.name,
            })
        }

        Req::SettingsGet(_) => {
            let snap = state.settings.snapshot();
            let filter_state = snap.global.wallpaper_filter.clone();
            let layout_defaults = pb::LayoutPrefs {
                fillmode: fillmode_to_pb(snap.global.layout.fillmode) as i32,
                align: align_to_pb(snap.global.layout.align) as i32,
                clear_rgba: snap.global.layout.clear_rgba.to_vec(),
            };
            Res::SettingsGet(pb::SettingsGetResponse {
                global: Some(pb::GlobalSettings {
                    target_extent: snap.global.target_extent,
                    render_size_policy: render_size_policy_to_pb(snap.global.render_size_policy)
                        as i32,
                    wallpaper_filters: filter_state
                        .filters
                        .into_iter()
                        .map(pb_rule_from_settings)
                        .collect(),
                    wallpaper_filter_logics: filter_state
                        .filter_logics
                        .into_iter()
                        .map(|logic| pb::FilterLogic {
                            op: logic.op,
                            group_a: logic.group_a,
                            group_b: logic.group_b,
                        })
                        .collect(),
                    layout_defaults: Some(layout_defaults),
                }),
                plugins: snap
                    .plugins
                    .into_iter()
                    .map(|(k, v)| (k, pb::PluginSettings { values: v }))
                    .collect(),
            })
        }

        Req::SettingsSet(r) => {
            // Full replace. Missing `global` falls back to current
            // values so callers can update plugins alone by sending
            // None for global.
            let mut new_plugins: std::collections::HashMap<
                String,
                std::collections::HashMap<String, String>,
            > = r.plugins.into_iter().map(|(k, v)| (k, v.values)).collect();

            // Schema validation up-front. Reject the entire RPC if any
            // declared key fails type / bounds / choices — partial
            // commits would leave the toml in a state that doesn't
            // match what the caller asked for.
            {
                let registry = state.renderer_manager.registry();
                for (plugin_name, kv) in new_plugins.iter_mut() {
                    let Some(def) = registry
                        .all_renderers()
                        .into_iter()
                        .find(|d| &d.name == plugin_name)
                    else {
                        continue;
                    };
                    if def.settings.is_empty() {
                        continue;
                    }
                    for (k, v) in kv.iter_mut() {
                        let Some(schema) = def.settings.get(k) else {
                            continue;
                        };
                        let coerced =
                            crate::plugin::renderer_registry::coerce_and_validate(k, v, schema)
                                .map_err(|e| {
                                    Error::SettingsValidationFailed(format!("{plugin_name}.{e}"))
                                })?;
                        *v = coerced;
                    }
                }
            }

            // Snapshot the previous per-plugin settings so we can diff
            // against the new ones and dispatch ApplySettings to any
            // live renderer whose plugin name matches.
            let previous_plugins = state.settings.snapshot().plugins;
            let previous_filter = state.settings.snapshot().global.wallpaper_filter;
            // Snapshot pre-mutation layout defaults so we know whether
            // to re-sync display set_configs after the write.
            let prev_layout = state.settings.snapshot().global.layout.clone();
            state.settings.update(|s| {
                if let Some(g) = r.global.as_ref() {
                    s.global.target_extent = g.target_extent;
                    s.global.render_size_policy = render_size_policy_from_pb(g.render_size_policy);
                    s.global.wallpaper_filter = settings_filter_state_from_pb(
                        &g.wallpaper_filters,
                        &g.wallpaper_filter_logics,
                    );
                    if let Some(ld) = g.layout_defaults.as_ref() {
                        if let Some(fm) = fillmode_from_pb(ld.fillmode) {
                            s.global.layout.fillmode = fm;
                        }
                        if let Some(al) = align_from_pb(ld.align) {
                            s.global.layout.align = al;
                        }
                        if let Some(rgba) = clear_rgba_from_pb(&ld.clear_rgba) {
                            s.global.layout.clear_rgba = rgba;
                        }
                    }
                }
                s.plugins = new_plugins.clone();
            });
            let new_filter = state.settings.snapshot().global.wallpaper_filter.clone();
            if new_filter != previous_filter {
                log::debug!(
                    "wallpaper filter updated: old={:?}, new={:?}",
                    previous_filter,
                    new_filter
                );
            }
            let new_layout = state.settings.snapshot().global.layout.clone();
            if new_layout != prev_layout {
                state.router.resync_all_set_configs().await;
                // Push fresh DisplaySnapshot so subscribers see new
                // effective_layout values.
                let snap = state.router.snapshot_displays().await;
                state.router.emit_displays_replace_for_settings_change(snap);
            }
            // Step 4: live renderer hot-reload.
            // For each plugin that actually changed, walk live
            // renderers for that plugin name and push the delta.
            // Identity-tagged keys produce a warn log (would require
            // a respawn, which is too invasive for a settings RPC).
            let mut plugin_names_changed: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for (name, values) in &new_plugins {
                if previous_plugins.get(name) != Some(values) {
                    plugin_names_changed.insert(name.clone());
                }
            }
            for name in previous_plugins.keys() {
                if !new_plugins.contains_key(name) {
                    plugin_names_changed.insert(name.clone());
                }
            }
            // Hot-reload failures used to be `log::warn`-and-continue.
            // Now we collect every per-renderer failure during the
            // walk; if any happened, propagate one aggregate
            // `SettingsApplyFailed` after publishing the change event
            // (settings are persisted regardless — the caller still
            // needs to know hot-reload was incomplete).
            let mut apply_failures: Vec<String> = Vec::new();
            for plugin_name in plugin_names_changed {
                let def = state
                    .renderer_manager
                    .registry()
                    .all_renderers()
                    .into_iter()
                    .find(|d| d.name == plugin_name)
                    .cloned();
                let Some(def) = def else { continue };
                let new_kv = new_plugins.get(&plugin_name).cloned().unwrap_or_default();
                let old_kv = previous_plugins
                    .get(&plugin_name)
                    .cloned()
                    .unwrap_or_default();

                // Forward every key the user actually changed (within
                // the manifest schema) to all live renderers of this
                // plugin. The renderer applies what it can hot-reload
                // and ignores the rest; whatever it didn't accept will
                // take effect on its next spawn via `Init.settings`,
                // which sources the same settings store.
                let kv: Vec<(String, String)> = new_kv
                    .iter()
                    .filter(|(k, v)| def.settings.contains_key(*k) && old_kv.get(*k) != Some(v))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if kv.is_empty() {
                    continue;
                }
                let ids = state.renderer_manager.list().await;
                for id in ids {
                    let Some(handle) = state.renderer_manager.get(&id).await else {
                        continue;
                    };
                    if handle.name != plugin_name {
                        continue;
                    }
                    if let Err(e) = state
                        .renderer_manager
                        .send_apply_settings(&id, kv.clone(), None)
                        .await
                    {
                        apply_failures.push(format!("{id} ({plugin_name}): {e}"));
                    }
                }
            }
            // Push the merged post-write state to all WS subscribers so
            // a second UI bound to the same daemon stays in sync.
            state.events.publish(GlobalEvent::SettingsChanged);
            if !apply_failures.is_empty() {
                return Err(Error::SettingsApplyFailed(format!(
                    "{} renderer(s): {}",
                    apply_failures.len(),
                    apply_failures.join("; ")
                )));
            }
            Res::SettingsSet(pb::Empty {})
        }

        Req::LibraryList(_) => {
            let snap = control::list_library_snapshots(&state.db).await;
            Res::LibraryList(pb::LibraryListResponse {
                libraries: snap.into_iter().map(library_instance_to_pb).collect(),
            })
        }

        Req::LibraryAdd(r) => {
            let plugin = repo::find_plugin_by_name(&state.db, &r.plugin_name)
                .await?
                .ok_or_else(|| Error::SourcePluginNotFound(r.plugin_name.clone()))?;
            let lib = repo::add_library(&state.db, plugin.id, &r.path).await?;
            let snap = LibrarySnapshot {
                id: lib.id,
                path: lib.path,
                plugin_name: r.plugin_name,
            };
            let added_path = snap.path.clone();
            state.router.upsert_library(snap);
            state.events.publish(GlobalEvent::LibrariesAdded {
                paths: vec![added_path],
            });
            // Rescan so the new library's items flow into the
            // in-memory snapshot + DB without waiting for the
            // next daemon restart. Shares `"scan/refresh"`
            // dedup key with manual scans — rapid LibraryAdd
            // bursts collapse into a single in-flight scan.
            let rescan_state = state.clone();
            state.tasks.spawn_async_unique(
                tasks::TaskKind::Generic,
                "scan/refresh",
                "scan/refresh-after-library-add",
                async move {
                    control::refresh_sources(&rescan_state)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                },
            );
            Res::LibraryAdd(pb::Empty {})
        }

        Req::LibraryAutoDetect(_) => {
            let added = control::auto_detect_libraries(&state).await?;
            Res::LibraryAutoDetect(pb::LibraryAutoDetectResponse {
                added: added.into_iter().map(library_instance_to_pb).collect(),
            })
        }

        Req::LibraryRemove(r) => {
            repo::remove_library(&state.db, r.id).await?;
            state.router.remove_library(r.id);
            let rescan_state = state.clone();
            state.tasks.spawn_async_unique(
                tasks::TaskKind::Generic,
                "scan/refresh",
                "scan/refresh-after-library-remove",
                async move {
                    control::refresh_sources(&rescan_state)
                        .await
                        .map(|_| ())
                        .map_err(anyhow::Error::from)
                },
            );
            Res::LibraryRemove(pb::Empty {})
        }

        // ---- playlists ----------------------------------------------------
        Req::PlaylistList(_) => {
            let rows = control::list_playlists(&state).await?;
            let playlists = rows
                .into_iter()
                .map(|s| pb::PlaylistSummary {
                    id: s.id,
                    name: s.name,
                    source_kind: s.source_kind,
                    mode: mode_str_to_pb(&s.mode) as i32,
                    interval_secs: s.interval_secs.max(0) as u32,
                    item_count: s.item_count,
                })
                .collect();
            Res::PlaylistList(pb::PlaylistListResponse { playlists })
        }

        Req::PlaylistCreate(r) => {
            let mode = pb_mode_to_enum(r.mode);
            let args = repo::PlaylistCreateArgs {
                name: &r.name,
                source_kind: repo::PLAYLIST_KIND_CURATED,
                filter_json: None,
                mode: mode.as_str(),
                interval_secs: r.interval_secs as i32,
                shuffle_seed: 0,
            };
            // repo::create_playlist returns Error::PlaylistInvalid on
            // smart/curated invariant violation; let it propagate.
            let p = repo::create_playlist(&state.db, args).await?;
            if !r.item_ids.is_empty() {
                if let Err(e) = repo::set_playlist_items(&state.db, p.id, &r.item_ids).await {
                    // Best-effort cleanup so we don't leave an empty
                    // stub the user didn't ask for.
                    let _ = repo::delete_playlist(&state.db, p.id).await;
                    return Err(Error::from(e));
                }
            }
            Res::PlaylistCreate(pb::PlaylistCreateResponse { id: p.id })
        }

        Req::PlaylistDelete(r) => {
            repo::delete_playlist(&state.db, r.id).await?;
            // If the deleted playlist was active, fall back to All so
            // the rotator + step path don't keep walking a dangling
            // cursor. Failure here used to be `log::warn`-and-swallow;
            // now it propagates so the caller learns the daemon is in
            // a half-state (row deleted, rotator still pointed at it).
            let active = state.playlist.lock().await.active_id;
            if active == Some(r.id) {
                control::deactivate_playlist(&state).await?;
            }
            Res::PlaylistDelete(pb::Empty {})
        }

        Req::PlaylistRename(r) => {
            repo::rename_playlist(&state.db, r.id, &r.name).await?;
            Res::PlaylistRename(pb::Empty {})
        }

        Req::PlaylistSetItems(r) => {
            repo::set_playlist_items(&state.db, r.id, &r.item_ids).await?;
            // If this playlist is the active one, refresh its resolved
            // id list immediately so the cursor sees the new
            // membership without waiting for a rescan. Reactivation
            // failure now propagates instead of being silently warned.
            let active = state.playlist.lock().await.active_id;
            if active == Some(r.id) {
                control::activate_playlist(&state, r.id).await?;
            }
            Res::PlaylistSetItems(pb::Empty {})
        }

        Req::PlaylistSetMode(r) => {
            let mode = pb_mode_to_enum(r.mode);
            repo::set_playlist_mode(&state.db, r.id, mode.as_str()).await?;
            let active = state.playlist.lock().await.active_id;
            if active == Some(r.id) {
                // `set_mode` returns `()` — no error to propagate.
                control::set_mode(&state, mode).await;
            }
            Res::PlaylistSetMode(pb::Empty {})
        }

        Req::PlaylistSetInterval(r) => {
            repo::set_playlist_interval(&state.db, r.id, r.interval_secs as i32).await?;
            let active = state.playlist.lock().await.active_id;
            if active == Some(r.id) {
                // `set_rotation_interval` returns `()`.
                control::set_rotation_interval(&state, r.interval_secs).await;
            }
            Res::PlaylistSetInterval(pb::Empty {})
        }

        Req::PlaylistActivate(r) => {
            // playlist::resolve::activate now returns Error::PlaylistNotFound
            // directly; the typed code flows through control::activate_playlist
            // and out here without an adapter.
            control::activate_playlist(&state, r.id).await?;
            // Adopt the row's stored interval into the live rotator so
            // a rotating playlist starts ticking on activate without a
            // separate set_interval round-trip. The lookup may legitimately
            // return None mid-activation; the row's absence is advisory
            // only (activate already succeeded) — but a real DB error
            // should still surface.
            if let Some(row) = repo::find_playlist(&state.db, r.id).await? {
                control::set_rotation_interval(&state, row.interval_secs.max(0) as u32).await;
            }
            Res::PlaylistActivate(pb::Empty {})
        }

        Req::PlaylistDeactivate(_) => {
            control::deactivate_playlist(&state).await?;
            Res::PlaylistDeactivate(pb::Empty {})
        }

        Req::PlaylistStatus(_) => {
            let s = control::playlist_status(&state).await;
            Res::PlaylistStatus(pb::PlaylistStatusResponse {
                active_id: s.active_id.unwrap_or(0),
                mode: mode_str_to_pb(&s.mode) as i32,
                interval_secs: s.interval_secs,
                current_id: s.current.unwrap_or_default(),
                position: s.position.unwrap_or(0),
                count: s.count,
                is_smart: s.is_smart,
            })
        }
    })
}

/// Decode the proto enum integer into the internal `playlist::Mode`.
/// `Unspecified` (0) and any unrecognized variant default to
/// Sequential — that's what a client that forgot the field meant.
fn pb_mode_to_enum(v: i32) -> playlist::Mode {
    match pb::PlaylistMode::try_from(v).unwrap_or(pb::PlaylistMode::Unspecified) {
        pb::PlaylistMode::Shuffle => playlist::Mode::Shuffle,
        pb::PlaylistMode::Random => playlist::Mode::Random,
        _ => playlist::Mode::Sequential,
    }
}

fn mode_str_to_pb(s: &str) -> pb::PlaylistMode {
    match playlist::Mode::from_str(s) {
        Some(playlist::Mode::Sequential) => pb::PlaylistMode::Sequential,
        Some(playlist::Mode::Shuffle) => pb::PlaylistMode::Shuffle,
        Some(playlist::Mode::Random) => pb::PlaylistMode::Random,
        None => pb::PlaylistMode::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Encode a dispatch result onto the wire. Thin wrapper around
/// `Error::to_response` / `ok_response` from `crate::error`; the dispatch
/// boundary is the only place the daemon-side `Error` becomes wire bytes.
fn build_response(request_id: u64, result: Result<pb::response::Payload, Error>) -> pb::Response {
    match result {
        Ok(payload) => ok_response(request_id, payload),
        Err(e) => e.to_response(request_id),
    }
}

fn wrap_response(resp: pb::Response) -> pb::ServerFrame {
    pb::ServerFrame {
        kind: Some(pb::server_frame::Kind::Response(resp)),
    }
}

#[allow(dead_code)]
pub fn wrap_event(evt: pb::Event) -> pb::ServerFrame {
    pb::ServerFrame {
        kind: Some(pb::server_frame::Kind::Event(evt)),
    }
}

fn entry_to_pb(
    e: &crate::wallpaper_type::WallpaperEntry,
    db_meta: Option<&crate::model::entities::item::Model>,
) -> pb::WallpaperEntry {
    // Prefer DB values (freshest, written by the probe task); fall back to
    // what the Lua plugin may have pre-filled on the in-memory entry.
    let size = db_meta.and_then(|m| m.size).or(e.size).unwrap_or(0);
    let width = db_meta
        .and_then(|m| m.width)
        .map(|v| v as u32)
        .or(e.width)
        .unwrap_or(0);
    let height = db_meta
        .and_then(|m| m.height)
        .map(|v| v as u32)
        .or(e.height)
        .unwrap_or(0);
    let format = db_meta
        .and_then(|m| m.format.clone())
        .or_else(|| e.format.clone())
        .unwrap_or_default();
    let preview = db_meta
        .and_then(|m| m.preview_path.as_deref())
        .map(|rel| {
            std::path::Path::new(&e.library_root)
                .join(rel)
                .to_string_lossy()
                .into_owned()
        })
        .or_else(|| e.preview.clone().filter(|s| !s.is_empty()))
        .unwrap_or_default();

    pb::WallpaperEntry {
        id: e.id.clone(),
        name: e.name.clone(),
        wp_type: e.wp_type.clone(),
        resource: e.resource.clone(),
        preview,
        metadata: e.metadata.clone(),
        size,
        width,
        height,
        format,
    }
}

#[cfg(test)]
mod tests {
    // Wallpaper filter SQL tests live in `model::filter`.
}
