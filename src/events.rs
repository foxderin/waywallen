//! Process-wide event bus.
//!
//! Two complementary surfaces:
//!
//!   - `bus`: a `tokio::sync::broadcast` of [`GlobalEvent`] for
//!     fan-out notifications. Late subscribers miss historical events;
//!     consumers that need a "did this happen?" answer should use the
//!     latched flags instead.
//!   - `sources_ready` / `display_ready` / `daemon_ready`:
//!     `tokio::sync::watch<bool>` channels acting as one-shot phase
//!     markers. Once flipped to `true`, late subscribers see the
//!     latched value via `wait_for(|v| *v)`. This is what the restore
//!     coordinator awaits — it does not care if it missed the original
//!     publish. `daemon_ready` is also surfaced over the wire as
//!     `StatusSync.phase` so reconnecting UIs derive truth from a
//!     snapshot rather than counting events.
//!
//! Adding a new latched phase marker means a new `watch<bool>`
//! field; adding a new transient event means a new variant on
//! [`GlobalEvent`].

use tokio::sync::{broadcast, watch};

const DEFAULT_BUS_CAPACITY: usize = 64;

/// Transient process-wide notifications.
#[derive(Debug, Clone)]
pub enum GlobalEvent {
    /// Source plugins finished loading and the initial DB sync ran.
    /// `playlist::ids` and `source_manager.list()` are now populated;
    /// callers that touch wallpapers can proceed.
    SourcesReady,
    /// At least one display has registered with the router and is
    /// reachable for `relink_all_displays_to`. Downstream restore /
    /// auto-apply paths should gate on this so renderers don't spawn
    /// into an empty audience.
    DisplayReady,
    /// The startup-restore task succeeded. Carries the wallpaper id
    /// that was applied, if any (`None` when no `last_wallpaper` was
    /// recorded or `--no-restore` is in effect).
    RestoreApplied(Option<String>),
    /// The startup-restore task failed at some stage. The string is
    /// the formatted error so log subscribers don't need a typed
    /// error variant.
    RestoreFailed(String),
    /// Core services are up (WS bound, DBus published). Latched so
    /// late subscribers (UIs that connect well after boot) can still
    /// observe the phase via `is_daemon_ready`. The wire surface is
    /// `StatusSync.phase`, not a transient event — receivers should
    /// react to the next `StatusSync` snapshot.
    DaemonReady,
    /// A wallpaper sync finished successfully; `count` is the total
    /// entry count after the swap. Sync start is observable via
    /// `StatusSync.scan_in_progress` — no separate started event.
    SyncFinished { count: usize },
    /// A wallpaper sync failed; the string is the formatted error.
    /// Maps to the same wire message as `SyncFinished` with `error`
    /// populated.
    SyncFailed(String),
    /// One or more libraries were just added — manually via
    /// `LibraryAdd` (single path) or via `LibraryAutoDetect` (one or
    /// more). UI mirrors this through `Notify` and surfaces a toast.
    /// `paths` is the absolute library root list of the additions.
    LibrariesAdded { paths: Vec<String> },
    /// Some piece of daemon-side runtime state changed. Carries no
    /// payload — receivers re-snapshot via the `StatusSync` builder
    /// in `ws_server`. Used by the closed-loop UI status binding;
    /// transient notifications (`ScanStarted`/`ScanCompleted`) are
    /// for one-shot reactions like toasts.
    StatusChanged,
    /// The persisted settings table just changed (either via
    /// `SettingsSet` RPC or via startup reconciliation that filled
    /// defaults / dropped unknown keys). Carries no payload — the
    /// `ws_server` re-snapshots `state.settings` to build the
    /// outgoing `SettingsChanged` event so all subscribers see the
    /// same merged truth.
    SettingsChanged,
    /// External display client failed handshake on the UDS endpoint
    /// (bad protocol name or unsupported version). UI mirrors this
    /// through `Notify` and surfaces a toast.
    DisplayConnectionFailed {
        client_name: String,
        client_protocol_version: u32,
        error_code: u32,
        reason: String,
    },
}

pub struct EventBus {
    bus: broadcast::Sender<GlobalEvent>,
    sources_ready: watch::Sender<bool>,
    display_ready: watch::Sender<bool>,
    daemon_ready: watch::Sender<bool>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }
}

impl EventBus {
    pub fn with_capacity(cap: usize) -> Self {
        let (bus, _) = broadcast::channel(cap);
        let (sources_ready, _) = watch::channel(false);
        let (display_ready, _) = watch::channel(false);
        let (daemon_ready, _) = watch::channel(false);
        Self {
            bus,
            sources_ready,
            display_ready,
            daemon_ready,
        }
    }

    /// Publish a transient event AND latch any phase marker the
    /// variant implies. Idempotent for phase markers — re-publishing
    /// `SourcesReady` after it's already latched is a no-op.
    pub fn publish(&self, e: GlobalEvent) {
        // `send_replace` instead of `send` — the latter fails when no
        // receivers exist (we drop the initial receiver in
        // `with_capacity`), and we don't care about the old value.
        match &e {
            GlobalEvent::SourcesReady => {
                self.sources_ready.send_replace(true);
            }
            GlobalEvent::DisplayReady => {
                self.display_ready.send_replace(true);
            }
            GlobalEvent::DaemonReady => {
                self.daemon_ready.send_replace(true);
            }
            _ => {}
        }
        let _ = self.bus.send(e);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GlobalEvent> {
        self.bus.subscribe()
    }

    /// Clone of the broadcast sender for callers that need to publish
    /// transient (non-phase-marker) events from sites that don't have a
    /// reference to `AppState` — e.g. the display endpoint, which only
    /// gets `Router` + a shutdown rx today.
    pub fn sender(&self) -> broadcast::Sender<GlobalEvent> {
        self.bus.clone()
    }

    pub fn watch_sources_ready(&self) -> watch::Receiver<bool> {
        self.sources_ready.subscribe()
    }

    pub fn watch_display_ready(&self) -> watch::Receiver<bool> {
        self.display_ready.subscribe()
    }

    pub fn watch_daemon_ready(&self) -> watch::Receiver<bool> {
        self.daemon_ready.subscribe()
    }

    pub fn is_sources_ready(&self) -> bool {
        *self.sources_ready.borrow()
    }

    pub fn is_display_ready(&self) -> bool {
        *self.display_ready.borrow()
    }

    pub fn is_daemon_ready(&self) -> bool {
        *self.daemon_ready.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn phase_marker_is_latched_for_late_subscribers() {
        let bus = EventBus::default();
        bus.publish(GlobalEvent::SourcesReady);
        let mut rx = bus.watch_sources_ready();
        // Late subscribe still sees the latched value immediately —
        // wait_for returns the borrowed-ref or, if the value already
        // satisfies the predicate, returns immediately.
        let v = tokio::time::timeout(Duration::from_millis(50), rx.wait_for(|v| *v))
            .await
            .expect("late subscribe blocked")
            .expect("watch closed");
        assert!(*v);
    }

    #[tokio::test]
    async fn transient_event_visible_to_subscribers_only_after_subscribe() {
        let bus = EventBus::default();
        // Subscribe first so we don't miss anything.
        let mut rx = bus.subscribe();
        bus.publish(GlobalEvent::RestoreApplied(Some("abc".into())));
        let evt = tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv error");
        match evt {
            GlobalEvent::RestoreApplied(Some(s)) => assert_eq!(s, "abc"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn republish_phase_marker_is_idempotent() {
        let bus = EventBus::default();
        bus.publish(GlobalEvent::DisplayReady);
        bus.publish(GlobalEvent::DisplayReady);
        assert!(bus.is_display_ready());
    }

    #[tokio::test]
    async fn daemon_ready_is_latched_for_late_subscribers() {
        let bus = EventBus::default();
        assert!(!bus.is_daemon_ready());
        bus.publish(GlobalEvent::DaemonReady);
        assert!(bus.is_daemon_ready());
        let mut rx = bus.watch_daemon_ready();
        let v = tokio::time::timeout(Duration::from_millis(50), rx.wait_for(|v| *v))
            .await
            .expect("late subscribe blocked")
            .expect("watch closed");
        assert!(*v);
    }
}
