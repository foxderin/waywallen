// Merged renderer IPC integration tests.
// Originally split across ipc_renderer_handshake_cpp.rs /
// ipc_renderer_handshake_rust.rs / ipc_renderer_lifecycle.rs.

#[path = "common/mod.rs"]
mod common;

mod handshake_cpp {
    #[allow(unused_imports)]
    use super::common;
// C++ host handshake: spawn the `waywallen-renderer` host binary
// against a listening Unix-domain socket and verify the handshake.
//
// This is an *integration* test from the Rust daemon's perspective:
//   1. Create a UDS listener at a tempfile path.
//   2. Spawn `$WAYWALLEN_RENDERER_BIN --ipc <path> ...`.
//   3. Accept the host's connection.
//   4. Read one framed message and assert it parses as `EventMsg::Ready`,
//      which the host emits after `SceneWallpaper::initVulkan` succeeds.
//
// Anything past Ready (BindBuffers, FrameReady) depends on a working
// GPU/Vulkan driver *and* a valid Wallpaper Engine `.pkg`. The
// `--test-pattern` smoke test below covers the BindBuffers/FrameReady
// wire without needing a real scene.
//
// Skipped (not failed) when `WAYWALLEN_RENDERER_BIN` is unset.

use waywallen::ipc::proto::EventMsg;
use waywallen::ipc::uds::recv_event;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;


#[test]
fn hello_handshake() {
    let Some(bin) = common::cpp_renderer_bin_from_env() else {
        eprintln!(
            "skipping ipc_renderer_handshake_cpp: set WAYWALLEN_RENDERER_BIN to the path \
             of the compiled waywallen-renderer binary to run this test"
        );
        return;
    };
    assert!(
        bin.exists(),
        "WAYWALLEN_RENDERER_BIN points at nonexistent path: {}",
        bin.display()
    );

    let sock_path = common::tmp_sock("cpp-host-handshake");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).expect("bind unix listener");
    let _cleanup = common::SockCleanup(sock_path.clone());

    let child = Command::new(&bin)
        .arg("--ipc")
        .arg(&sock_path)
        .arg("--width")
        .arg("1280")
        .arg("--height")
        .arg("720")
        .arg("--fps")
        .arg("30")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {}", bin.display(), e));
    let mut guard = common::ChildGuard(child);

    listener
        .set_nonblocking(false)
        .expect("set blocking on listener");
    let (stream, _addr) = match common::accept_with_timeout(&listener, Duration::from_secs(10)) {
        Some(Ok(x)) => x,
        Some(Err(e)) => panic!("accept failed: {e}"),
        None => {
            let _ = guard.0.kill();
            panic!("timed out waiting for waywallen-renderer to connect back");
        }
    };

    let (msg, fds): (EventMsg, _) =
        recv_event(&stream).expect("recv first frame from host");
    assert!(fds.is_empty(), "ready must not carry fds");
    match msg {
        EventMsg::Ready { .. } => { /* ok */ }
        other => panic!("expected Ready, got {other:?}"),
    }
}

/// Extended smoke against the C++ host's `--test-pattern` mode.
///
/// `SceneWallpaper::loadScene` early-returns when no assets directory is
/// configured, so without a full Wallpaper Engine install there's nothing
/// to drive `redraw_callback`. The host's `--test-pattern` CLI flag pumps
/// the offscreen ExSwapchain ring directly from a host timer thread and
/// emits BindBuffers + FrameReady without any actual pixel drawing, which
/// is enough to prove the wire end-to-end.
#[test]
fn binding_and_frames_smoke() {
    let Some(bin) = common::cpp_renderer_bin_from_env() else {
        eprintln!("skipping: WAYWALLEN_RENDERER_BIN unset");
        return;
    };

    let sock_path = common::tmp_sock("cpp-host-test-pattern");
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).expect("bind");
    let _cleanup = common::SockCleanup(sock_path.clone());

    let child = Command::new(&bin)
        .arg("--ipc")
        .arg(&sock_path)
        .arg("--width")
        .arg("1280")
        .arg("--height")
        .arg("720")
        .arg("--fps")
        .arg("30")
        .arg("--test-pattern")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn host");
    let mut guard = common::ChildGuard(child);

    let (stream, _) = match common::accept_with_timeout(&listener, Duration::from_secs(10)) {
        Some(Ok(x)) => x,
        _ => {
            let _ = guard.0.kill();
            panic!("accept timed out");
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .expect("set rd timeout");

    // Drain until Ready → BindBuffers → >=5 FrameReady, or timeout.
    let mut saw_ready = false;
    let mut bind: Option<(Vec<i32>, (u32, u32, u32, u32, u64, u64))> = None;
    let mut frames = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let (msg, fds): (EventMsg, _) = match recv_event(&stream) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("recv error (expected if hung): {e}");
                break;
            }
        };
        match msg {
            EventMsg::Ready { .. } => saw_ready = true,
            EventMsg::BindBuffers {
                count,
                fourcc,
                width,
                height,
                modifier,
                planes_per_buffer,
                stride,
                plane_offset,
                size,
                ..
            } => {
                eprintln!(
                    "BindBuffers: count={} fourcc=0x{:08x} {}x{} planes={} mod=0x{:x} \
                     stride={:?} plane_offset={:?} size={:?} fds={}",
                    count,
                    fourcc,
                    width,
                    height,
                    planes_per_buffer,
                    modifier,
                    stride,
                    plane_offset,
                    size,
                    fds.len()
                );
                assert_eq!(count, 3, "expected 3 slots");
                let expected_fds = (count as usize) * (planes_per_buffer as usize);
                assert_eq!(fds.len(), expected_fds,
                    "expected {expected_fds} FDs via SCM_RIGHTS (count*planes)");
                assert!(fourcc != 0, "fourcc must be non-zero");
                assert!(u64::from(stride[0]) >= u64::from(width) * 4, "stride sanity");
                bind = Some((
                    fds.iter().map(|f| f.as_raw_fd()).collect(),
                    (count, fourcc, width, height, u64::from(stride[0]), modifier),
                ));
                std::mem::forget(fds);
            }
            EventMsg::FrameReady { .. } => {
                frames += 1;
                if frames >= 5 && bind.is_some() {
                    break;
                }
            }
            other => eprintln!("unexpected msg: {other:?}"),
        }
    }

    assert!(saw_ready, "never saw Ready event");
    let bind = bind.expect("never saw BindBuffers under --test-pattern mode");
    assert!(
        frames >= 5,
        "expected >=5 FrameReady, got {frames}; bind={bind:?}"
    );
}
}

mod handshake_rust {
    #[allow(unused_imports)]
    use super::common;
// Rust waywallen_renderer handshake: spawn the Rust `waywallen_renderer`
// binary against a listening Unix-domain socket, expect
//
//   1. `EventMsg::Ready`,
//   2. `EventMsg::BindBuffers` carrying 3 DMA-BUF FDs with the
//      fourcc/stride/modifier the renderer advertised,
//   3. clean shutdown in response to `ControlMsg::Shutdown`.
//
// Uses the binary cargo builds into `CARGO_BIN_EXE_waywallen_renderer`
// so no env var wiring is required; the test is self-contained.
//
// This asserts the M1.3b architectural contract: the Rust renderer
// stands in for the C++ host over the same IPC wire format. Actual
// per-frame rendering (M1.4) is out of scope here.

use waywallen::ipc::proto::{ControlMsg, EventMsg};
use waywallen::ipc::uds::{recv_event, send_control};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;


const DRM_FORMAT_ABGR8888: u32 = 0x34324241;

#[test]
fn waywallen_renderer_bind_handshake() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_waywallen_renderer"));
    assert!(bin.exists(), "renderer binary missing: {}", bin.display());

    let sock_path = common::tmp_sock("rust-renderer-handshake");
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path).expect("bind uds listener");
    let _cleanup = common::SockCleanup(sock_path.clone());

    let child = Command::new(&bin)
        .arg("--ipc")
        .arg(&sock_path)
        .arg("--width")
        .arg("256")
        .arg("--height")
        .arg("256")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
    let mut guard = common::ChildGuard(child);

    let (stream, _) = match common::accept_with_timeout(&listener, Duration::from_secs(15)) {
        Some(Ok(x)) => x,
        Some(Err(e)) => panic!("accept: {e}"),
        None => {
            let _ = guard.0.kill();
            panic!("timed out waiting for renderer connect");
        }
    };

    // 1. Ready, no fds. Render node fields are best-effort (driver may
    //    or may not advertise VK_EXT_physical_device_drm); we just
    //    assert the event arrived.
    let (msg, fds) = recv_event(&stream).expect("recv Ready");
    assert!(fds.is_empty(), "Ready must not carry fds");
    assert!(matches!(msg, EventMsg::Ready { .. }), "expected Ready, got {msg:?}");

    // 2. BindBuffers with 3 fds (LINEAR → planes_per_buffer = 1, so
    //    count * planes_per_buffer = 3 fds).
    let (msg, fds) = recv_event(&stream).expect("recv BindBuffers");
    match msg {
        EventMsg::BindBuffers {
            generation,
            flags,
            count,
            fourcc,
            width,
            height,
            modifier,
            planes_per_buffer,
            stride,
            plane_offset,
            size,
        } => {
            assert_eq!(generation, 1, "first BindBuffers must report gen=1");
            assert_eq!(flags, 0, "initial pool must be DEVICE_LOCAL (flags=0)");
            assert_eq!(count, 3);
            assert_eq!(
                fourcc, DRM_FORMAT_ABGR8888,
                "renderer advertised wrong fourcc 0x{fourcc:08x}"
            );
            assert_eq!(width, 256);
            assert_eq!(height, 256);
            assert_eq!(modifier, 0, "expected DRM_FORMAT_MOD_LINEAR");
            assert_eq!(planes_per_buffer, 1, "LINEAR → single plane");
            let n = (count as usize) * (planes_per_buffer as usize);
            assert_eq!(fds.len(), n, "expected count*planes={n} DMA-BUF fds");
            assert_eq!(stride.len(), n);
            assert_eq!(plane_offset.len(), n);
            assert_eq!(size.len(), n);
            for &s in &stride {
                assert!(s >= 256 * 4, "stride {s} below minimum");
            }
            for &o in &plane_offset {
                assert_eq!(o, 0);
            }
            for (i, &sz) in size.iter().enumerate() {
                assert_eq!(sz, u64::from(stride[i]) * u64::from(height));
            }
        }
        other => panic!("expected BindBuffers, got {other:?}"),
    }

    // 3. Drain 6 FrameReady events (2 full cycles) and assert that the
    //    slot index cycles 0,1,2,0,1,2 — i.e. the renderer's frame loop
    //    really is picking slots deterministically.
    let mut observed_slots = Vec::<u32>::new();
    let mut last_seq: i64 = -1;
    for _ in 0..6 {
        let (ev, fds) = recv_event(&stream).expect("recv FrameReady");
        assert_eq!(fds.len(), 1, "FrameReady must carry exactly one sync_fd");
        match ev {
            EventMsg::FrameReady {
                image_index,
                seq,
                ts_ns,
                ..
            } => {
                assert!(ts_ns > 0, "ts_ns must be monotonic");
                assert!((seq as i64) > last_seq, "seq must be monotonic");
                last_seq = seq as i64;
                observed_slots.push(image_index);
            }
            other => panic!("expected FrameReady, got {other:?}"),
        }
    }
    assert_eq!(observed_slots, vec![0, 1, 2, 0, 1, 2]);

    // Pixel-level verification via mmap is deliberately skipped: AMD
    // RADV allocates the DMA-BUFs in DEVICE_LOCAL VRAM, which isn't
    // host-visible and therefore fails mmap(MAP_SHARED). The proper
    // readback path is importing into a local Vulkan instance and
    // issuing a copy — that happens in the M2 display milestone.

    // 4. Send Shutdown and poll-wait up to 3s for the child to exit.
    send_control(&stream, &ControlMsg::Shutdown, &[]).expect("send Shutdown");
    let start = std::time::Instant::now();
    loop {
        match guard.0.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "renderer exit status {status:?}");
                return;
            }
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(3) {
                    panic!("renderer did not exit within 3s of Shutdown");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("wait: {e}"),
        }
    }
}
}

mod lifecycle {
    #[allow(unused_imports)]
    use super::common;
// RendererManager lifecycle: spawn → control → kill.
//
// Skipped (not failed) when `WAYWALLEN_RENDERER_BIN` is unset, mirroring
// the other `ipc_renderer_*` tests' contract.

use waywallen::ipc::proto::ControlMsg;
use waywallen::renderer_manager::{RendererManager, SpawnRequest};
use std::sync::Arc;
use std::time::Duration;


#[tokio::test]
async fn spawn_control_kill_roundtrip() {
    if common::cpp_renderer_bin_from_env().is_none() {
        eprintln!(
            "skipping ipc_renderer_lifecycle: set WAYWALLEN_RENDERER_BIN to the path \
             of the compiled waywallen-renderer binary to run this test"
        );
        return;
    }

    let mgr = Arc::new(RendererManager::new_default());

    // Spawn a renderer with bogus scene/assets — the host will start its
    // looper threads and emit Ready before noticing the scene is missing,
    // which is fine for this test (we only care about IPC liveness).
    let req = SpawnRequest {
        wp_type: "scene".into(),
        extras: std::collections::HashMap::new(),
        metadata: std::collections::HashMap::new(),
        width: 320,
        height: 240,
        extent_mode: 0,
        fps: 15,
        test_pattern: false,
        renderer_name: None,
    };
    let id = mgr.spawn(req).await.expect("spawn");
    assert!(!id.is_empty());

    // The renderer should be discoverable via list().
    let listed = mgr.list().await;
    assert!(listed.contains(&id), "list() should contain {id}: {listed:?}");

    // Push a few control messages. Each one is a fire-and-forget round
    // trip on the unix socket; success means the host's reader thread
    // accepted the JSON without disconnecting.
    mgr.send_control(&id, ControlMsg::Play)
        .await
        .expect("Play");
    mgr.send_control(&id, ControlMsg::Pause)
        .await
        .expect("Pause");
    mgr.send_control(&id, ControlMsg::Mouse { x: 0.5, y: 0.25 })
        .await
        .expect("Mouse");
    mgr.send_control(&id, ControlMsg::SetFps { fps: 24 })
        .await
        .expect("SetFps");

    // Tiny delay to let the host process the messages before we tear it
    // down — without this we sometimes race the kill ahead of the host's
    // reader thread observing the messages.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Kill cleans up. After kill the id should no longer list.
    mgr.kill(&id).await.expect("kill");
    let listed = mgr.list().await;
    assert!(!listed.contains(&id), "list() should not contain {id} after kill: {listed:?}");

    // send_control on a killed renderer must error.
    let err = mgr
        .send_control(&id, ControlMsg::Play)
        .await
        .expect_err("send to dead renderer should error");
    assert!(err.to_string().contains("unknown renderer"));
}
}

