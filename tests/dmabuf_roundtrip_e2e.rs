//! End-to-end DMA-BUF sharing test for the waywallen daemon.
//!
//! ## What this test verifies
//!
//! For every `(fourcc, modifier)` pair in the intersection of the
//! image renderer's caps and the dump_display test consumer's caps,
//! the daemon should be able to:
//!
//!   1. Negotiate that pair via `negotiate::pick`.
//!   2. Have the renderer allocate a VkImage with that modifier and
//!      export a DMA-BUF fd.
//!   3. Forward `bind_buffers` (carrying the fd via SCM_RIGHTS) to
//!      dump_display.
//!   4. Forward `frame_ready` (acquire sync_fd + release_syncobj fd).
//!   5. dump_display imports the DMA-BUF, copies it back to a host
//!      buffer, dumps the bytes; signals release_syncobj so the
//!      producer can reuse the slot.
//!
//! The test then byte-compares the producer's pre-upload RGBA dump
//! against dump_display's post-readback RGBA dump. They must match
//! exactly — any modifier/stride/plane_offset/memory-placement bug
//! shows up as a first-byte mismatch with concrete diagnostics.
//!
//! ## Phasing
//!
//! - **Phase 1:** discovery + intersection. `caps_discovery_intersection_is_non_empty`
//!   spawns both binaries with `--print-caps`, parses the JSON,
//!   computes the intersection, and asserts non-empty. This is already
//!   valuable — it catches schema drift between the two `--print-caps`
//!   paths and confirms a baseline (`ABGR8888 + LINEAR`) survives
//!   Vulkan probing on this host.
//!
//! - **Phase 2:** per-pair end-to-end run (this commit lights up
//!   `per_pair_byte_roundtrip`). For each pair we stand up an
//!   in-process `RendererManager + Router + display_endpoint::serve`,
//!   spawn the C++ image renderer with `WAYWALLEN_IMAGE_DUMP_DIR`
//!   pointing at a tempdir, spawn `dump_display` with `--dump-dir`
//!   pointing at a sibling tempdir, drive a single frame, and then
//!   byte-compare the two dump files. Today this covers the
//!   `(ABGR8888, LINEAR)` cross-vendor baseline; the dump_display
//!   readback path only handles LINEAR. Tiled-modifier coverage is
//!   gated on the Vulkan import + `vkCmdCopyImageToBuffer` path that
//!   `vk_consumer.rs::import_and_dump` flags as TODO.
//!
//! ## Skip conditions
//!
//! - No `/dev/dri` (no GPU) — both binaries need a Vulkan-capable
//!   render node to produce caps.
//! - `WAYWALLEN_RENDERER_BIN` unset and the default candidate
//!   (`plugins/image/build/waywallen-image-renderer` or
//!   `../install/bin/...`) doesn't exist.
//! - `WAYWALLEN_DUMP_DISPLAY_BIN` unset and the default
//!   `target/{debug,release}/waywallen-dump-display` doesn't exist.
//! - `ui/assets/main_page.png` missing (used as the renderer input).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[path = "common/mod.rs"]
mod common;

fn fourcc_str(fc: u32) -> String {
    let b = fc.to_le_bytes();
    if b.iter().all(|&c| (0x20..=0x7e).contains(&c)) {
        format!(
            "'{}{}{}{}'",
            b[0] as char, b[1] as char, b[2] as char, b[3] as char
        )
    } else {
        format!("0x{fc:08x}")
    }
}

fn renderer_bin() -> Option<PathBuf> {
    if let Some(p) = common::cpp_renderer_bin_from_env() {
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for c in [
        manifest.join("plugins/image/build/waywallen-image-renderer"),
        manifest.join("../install/bin/waywallen-image-renderer"),
    ] {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

fn image_path() -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = manifest.join("ui/assets/main_page.png");
    candidate.exists().then_some(candidate)
}

#[test]
fn caps_discovery_intersection_is_non_empty() {
    if !common::have_vulkan_device() {
        eprintln!("skip: no /dev/dri");
        return;
    }
    let Some(rbin) = renderer_bin() else {
        eprintln!(
            "skip: waywallen-image-renderer binary not found \
             (set WAYWALLEN_RENDERER_BIN or build plugins/image)"
        );
        return;
    };
    let Some(dbin) = common::dump_display_bin_from_env() else {
        eprintln!(
            "skip: waywallen-dump-display binary not found \
             (set WAYWALLEN_DUMP_DISPLAY_BIN or run \
             `cargo build -p waywallen-dump-display`)"
        );
        return;
    };

    let renderer_caps =
        common::print_caps(&rbin).expect("image renderer --print-caps");
    let consumer_caps =
        common::print_caps(&dbin).expect("dump_display --print-caps");

    eprintln!(
        "renderer caps: {} fourccs, {} pairs",
        renderer_caps.by_fourcc.len(),
        renderer_caps.pairs().len()
    );
    eprintln!(
        "consumer caps: {} fourccs, {} pairs",
        consumer_caps.by_fourcc.len(),
        consumer_caps.pairs().len()
    );

    let pairs = common::intersect_caps(&renderer_caps, &consumer_caps);
    assert!(
        !pairs.is_empty(),
        "renderer×consumer cap intersection is EMPTY — \
         negotiate::pick would have nothing to choose. \
         renderer.by_fourcc={:?}\n consumer.by_fourcc={:?}",
        renderer_caps.by_fourcc.keys().collect::<Vec<_>>(),
        consumer_caps.by_fourcc.keys().collect::<Vec<_>>()
    );

    eprintln!("intersection ({} pair(s)):", pairs.len());
    for (fc, m) in &pairs {
        eprintln!("  {} (0x{fc:08x}) × modifier=0x{m:016x}", fourcc_str(*fc));
    }

    // Until the per-pair E2E flow lands (Phase 2), assert the
    // baseline that the negotiate picker treats as the cross-vendor
    // escape hatch.
    let abgr_linear = (waywallen::negotiate::DRM_FORMAT_ABGR8888,
                       waywallen::negotiate::DRM_FORMAT_MOD_LINEAR);
    assert!(
        pairs.contains(&abgr_linear),
        "expected ABGR8888 + LINEAR in intersection (the universal cross-vendor \
         path negotiate::pick falls back to). got: {pairs:?}"
    );
}

/// Phase 2: per-pair byte-level round-trip. For every pair in the
/// caps intersection, spin up Router + RendererManager +
/// display_endpoint inside the test, spawn the C++ image renderer
/// (with `WAYWALLEN_IMAGE_DUMP_DIR` set so it writes its pre-upload
/// RGBA8 to a producer dump), spawn `dump_display` (with `--dump-dir`
/// set so it writes its post-readback RGBA8 to a consumer dump), let
/// the daemon negotiate that exact pair, drive a single frame, then
/// byte-compare the two dumps.
#[tokio::test]
async fn per_pair_byte_roundtrip() {
    if !common::have_vulkan_device() {
        eprintln!("skip: no /dev/dri");
        return;
    }
    let Some(rbin) = renderer_bin() else {
        eprintln!("skip: renderer bin");
        return;
    };
    let Some(dbin) = common::dump_display_bin_from_env() else {
        eprintln!("skip: dump_display bin");
        return;
    };
    let Some(img) = image_path() else {
        eprintln!("skip: ui/assets/main_page.png not found");
        return;
    };

    let renderer_caps = common::print_caps(&rbin).expect("renderer --print-caps");
    let consumer_caps = common::print_caps(&dbin).expect("dump_display --print-caps");
    let pairs = common::intersect_caps(&renderer_caps, &consumer_caps);
    assert!(!pairs.is_empty(), "empty caps intersection — see Phase 1 test");

    for (fc, m) in pairs {
        let label = format!("fourcc={} modifier=0x{m:016x}", fourcc_str(fc));
        eprintln!("=== run pair: {label} ===");
        run_one_pair(&rbin, &dbin, &img, fc, m)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        eprintln!("PASS {label}");
    }
}

/// One full daemon-up / one-frame / dump-compare cycle for a single
/// `(fourcc, modifier)` pair. Returns `Err(message)` rather than
/// panicking so the caller can attach the pair label.
async fn run_one_pair(
    renderer_bin: &Path,
    dump_display_bin: &Path,
    image_path: &Path,
    fourcc: u32,
    modifier: u64,
) -> Result<(), String> {
    use waywallen::plugin::renderer_registry::{RendererDef, RendererRegistry};
    use waywallen::renderer_manager::{RendererManager, SpawnRequest};
    use waywallen::routing::Router;

    // `WAYWALLEN_DUMP_KEEP_DIR=<dir>`: instead of a tempdir that's
    // wiped after the test, write dumps into a per-pair subdir under
    // `<dir>` and skip cleanup so they can be inspected (e.g.
    // converted to PNG to eyeball-compare the image). The dir is
    // created if missing; existing files are overwritten.
    let (prod_dir, cons_dir, _keep);
    let _tmp_owner: Option<tempfile::TempDir>;
    if let Some(keep) = std::env::var_os("WAYWALLEN_DUMP_KEEP_DIR") {
        let base = PathBuf::from(keep)
            .join(format!("0x{fourcc:08x}-0x{modifier:016x}"));
        prod_dir = base.join("producer");
        cons_dir = base.join("consumer");
        _keep = base;
        _tmp_owner = None;
    } else {
        let tmp = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        prod_dir = tmp.path().join("producer");
        cons_dir = tmp.path().join("consumer");
        _keep = tmp.path().to_path_buf();
        _tmp_owner = Some(tmp);
    }
    std::fs::create_dir_all(&prod_dir).map_err(|e| format!("mkdir prod: {e}"))?;
    std::fs::create_dir_all(&cons_dir).map_err(|e| format!("mkdir cons: {e}"))?;
    if std::env::var_os("WAYWALLEN_DUMP_KEEP_DIR").is_some() {
        eprintln!(
            "  dump dir: producer={}\n           consumer={}",
            prod_dir.display(),
            cons_dir.display()
        );
    }
    let sock = common::tmp_sock("dmabuf-rt");
    let _sock_cleanup = common::SockCleanup(sock.clone());

    // Build a one-entry registry pointing at the in-tree renderer.
    let mut registry = RendererRegistry::new();
    registry.register(RendererDef {
        name: "image-test".to_string(),
        bin: renderer_bin.to_path_buf(),
        types: vec!["image".to_string()],
        priority: 100,
        version: "v0.0.0".to_string(),
        spawn_version: None,
        extras: Vec::new(),
        settings: Default::default(),
    });

    let mgr = Arc::new(RendererManager::new(registry));
    mgr.start_reaper();
    let router = Router::new(mgr.clone());
    mgr.attach_router(Arc::downgrade(&router));

    // Set the dump env var only for the duration of the spawn — the
    // child inherits it at fork(2) time so removing it after spawn
    // returns leaves no global mutation behind. Safe across this
    // test's pairs because we run sequentially. Other tests in this
    // binary (`caps_discovery_intersection_is_non_empty`) only call
    // `--print-caps` which doesn't consult this env var.
    std::env::set_var("WAYWALLEN_IMAGE_DUMP_DIR", &prod_dir);
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("image".to_string(), image_path.display().to_string());
    let req = SpawnRequest {
        wp_type: "image".to_string(),
        metadata,
        width: 640,
        height: 360,
        fps: 30,
        test_pattern: false,
        renderer_name: None,
    };
    let spawn_res = mgr.spawn(req).await;
    std::env::remove_var("WAYWALLEN_IMAGE_DUMP_DIR");
    let renderer_id = spawn_res.map_err(|e| format!("spawn renderer: {e}"))?;

    let handle = mgr
        .get(&renderer_id)
        .await
        .ok_or_else(|| "no handle after spawn".to_string())?;
    router.register_renderer(handle.clone()).await;

    // The renderer publishes FormatCaps shortly after Ready. The
    // router's reconcile_buffer_flags reads it through
    // `handle.format_caps()` (cached state, not the broadcast
    // channel), so we just have to wait for it to land before letting
    // the display register.
    if !wait_for(Duration::from_secs(5), || handle.format_caps().is_some()).await {
        let _ = mgr.kill(&renderer_id).await;
        return Err("renderer never published FormatCaps".to_string());
    }

    // Spawn the display endpoint (in-process). `serve_with_shutdown`
    // listens on `sock` and forwards client connections through the
    // router.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let endpoint_router = router.clone();
    let endpoint_sock = sock.clone();
    let endpoint_task = tokio::spawn(async move {
        if let Err(e) = waywallen::display_endpoint::serve_with_shutdown(
            &endpoint_sock,
            endpoint_router,
            shutdown_rx,
        )
        .await
        {
            log::warn!("display_endpoint exited: {e}");
        }
    });

    if !common::wait_for_sock_bind(&sock, Duration::from_secs(5)).await {
        let _ = shutdown_tx.send(true);
        let _ = endpoint_task.await;
        let _ = mgr.kill(&renderer_id).await;
        return Err("display socket never bound".to_string());
    }

    // Spawn dump_display child against the daemon socket. Restrict
    // its advertised caps to exactly this pair so `negotiate::pick`
    // has only one option — guarantees the daemon picks `(fourcc,
    // modifier)` no matter what other pairs the renderer supports.
    let advertise = format!("0x{fourcc:08x}:0x{modifier:016x}");
    let dump_child = std::process::Command::new(dump_display_bin)
        .arg("--socket")
        .arg(&sock)
        .arg("--advertise")
        .arg(&advertise)
        .arg("--dump-dir")
        .arg(&cons_dir)
        .arg("--frames")
        .arg("1")
        .env("RUST_LOG", "info")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("spawn dump_display: {e}"))?;
    let mut dump_child = common::ChildGuard(dump_child);

    // Wait for dump_display to consume one frame and exit. Run the
    // wait on a blocking thread so the tokio runtime stays free for
    // the endpoint + reaper tasks.
    let exit = tokio::task::spawn_blocking(move || {
        let res = wait_child(&mut dump_child.0, Duration::from_secs(30));
        (res, dump_child)
    })
    .await
    .map_err(|e| format!("join wait_child: {e}"))?;
    let (exit_status, _drop_guard) = exit;

    // Tear down the daemon-side bits BEFORE comparing dumps so a
    // panic in the compare doesn't leak the renderer subprocess past
    // the test boundary.
    let _ = shutdown_tx.send(true);
    let _ = mgr.kill(&renderer_id).await;
    // Endpoint task exits when its accept loop sees shutdown_rx flip.
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint_task).await;

    let exit_status = exit_status.ok_or_else(|| "dump_display timed out".to_string())?;
    if !exit_status.success() {
        return Err(format!("dump_display exited {exit_status:?}"));
    }

    // One frame, one bind ⇒ exactly one producer-*.bin and one
    // consumer-*.bin. If multiple turn up, that's a bug in the
    // dump-write paths (or stale state from a previous run leaking
    // into the same tempdir, which can't happen here).
    let prod = find_one_dump(&prod_dir, "producer-")
        .ok_or_else(|| format!("no producer-*.bin in {}", prod_dir.display()))?;
    let cons = find_one_dump(&cons_dir, "consumer-")
        .ok_or_else(|| format!("no consumer-*.bin in {}", cons_dir.display()))?;
    common::compare_rgba8_dumps(&prod, &cons).map_err(|e| {
        format!(
            "byte compare failed:\n  producer: {}\n  consumer: {}\n  err: {e}",
            prod.display(),
            cons.display()
        )
    })?;
    Ok(())
}

/// Poll `predicate` every 20 ms until it returns true or `timeout`
/// elapses. Returns `false` on timeout.
async fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// Block (on this thread) until `child` exits or `timeout` elapses.
/// `None` means timeout — caller should kill + report.
fn wait_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Locate exactly one `.bin` whose filename starts with `prefix`.
/// Returns `None` when the directory is empty or unreadable. If
/// multiple matches exist (shouldn't happen for a single-frame run),
/// pick the lexicographically first so failures are deterministic.
fn find_one_dump(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with(prefix) && s.ends_with(".bin")
        })
        .map(|e| e.path())
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort();
    Some(entries.remove(0))
}
