use std::io::{BufRead, BufReader};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use ash::vk;
use serde::Deserialize;

use crate::display::endpoint;
use crate::ipc::proto::EventMsg;
use crate::renderer_manager::{BindSnapshot, RendererHandle, RendererManager};
use crate::routing::Router;

use super::super::report::Fanout;
use super::super::spawn::{spawn, ChildSpec};
use super::super::vk::cmd;
use super::super::vk::device::VkDevice;
use super::super::vk::image::{create_with_modifiers, export_dmabuf};
use super::super::vk::sync::{create_binary_sync_fd_exportable, export_signaled_sync_fd};

const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const FOURCC_AB24: u32 = 0x34324241;
const WIDTH: u32 = 256;
const HEIGHT: u32 = 256;
const FRAMES: u32 = 60;
const NUM_DISPLAYS: u32 = 2;
const DISPLAY_REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_PACE: Duration = Duration::from_millis(16);

#[derive(Debug, Deserialize)]
struct ChildStatus {
    #[allow(dead_code)]
    role: String,
    slot: u32,
    frames: u64,
    ok: u64,
    mismatch: u64,
    clean_exit: bool,
    fatal: Option<String>,
}

pub fn run_orchestrator(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    vkd: &VkDevice,
    dev_meta: &super::super::vk::instance::DeviceMeta,
    cross_gpu: bool,
) -> Result<Fanout> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for fanout")?;
    rt.block_on(run_async(instance, phys, vkd, dev_meta, cross_gpu))
}

async fn run_async(
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    vkd: &VkDevice,
    dev_meta: &super::super::vk::instance::DeviceMeta,
    cross_gpu: bool,
) -> Result<Fanout> {
    log::info!("fanout: bringing up production endpoint + Router");

    let mgr = Arc::new(RendererManager::new_default());
    let router = Router::new(Arc::clone(&mgr));
    let renderer = RendererHandle::test_stub("self_test_fanout", "image");
    mgr.register_test_handle(Arc::clone(&renderer)).await;
    router.register_renderer(Arc::clone(&renderer)).await;

    let modifier = pick_modifier(vkd, instance, phys, cross_gpu)?;
    log::info!(
        "fanout: using modifier {:#x} ({})",
        modifier,
        super::super::vk::modifier::format_modifier(modifier)
    );
    let img0 = create_with_modifiers(
        vkd,
        WIDTH,
        HEIGHT,
        FORMAT,
        vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST,
        &[modifier],
        cross_gpu,
    )
    .context("alloc slot 0")?;
    let img1 = create_with_modifiers(
        vkd,
        WIDTH,
        HEIGHT,
        FORMAT,
        vk::ImageUsageFlags::COLOR_ATTACHMENT
            | vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::TRANSFER_DST,
        &[modifier],
        cross_gpu,
    )
    .context("alloc slot 1")?;
    let cmdbuf = cmd::create(vkd)?;
    cmd::transition_to_general(vkd, &cmdbuf, &[img0.image, img1.image])?;

    let fd0 = export_dmabuf(vkd, &img0).context("export slot 0 dma-buf")?;
    let fd1 = export_dmabuf(vkd, &img1).context("export slot 1 dma-buf")?;

    let snap = BindSnapshot {
        generation: 1,
        flags: 0,
        count: 2,
        fourcc: FOURCC_AB24,
        width: WIDTH,
        height: HEIGHT,
        modifier: img0.modifier,
        planes_per_buffer: 1,
        stride: vec![
            u32::try_from(img0.plane0_stride).unwrap_or(u32::MAX),
            u32::try_from(img1.plane0_stride).unwrap_or(u32::MAX),
        ],
        plane_offset: vec![
            u32::try_from(img0.plane0_offset).unwrap_or(0),
            u32::try_from(img1.plane0_offset).unwrap_or(0),
        ],
        size: vec![img0.plane0_size, img1.plane0_size],
        fds: vec![fd0, fd1],
    };
    *renderer.bind_snapshot().lock().unwrap() = Some(snap);

    let sock_dir = make_socket_dir().context("tempdir for endpoint socket")?;
    let sock = sock_dir.join("display.sock");
    let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(8);
    let serve_handle = {
        let r = Arc::clone(&router);
        let s = sock.clone();
        tokio::spawn(async move {
            let _ = endpoint::serve_with_shutdown(&s, r, events_tx, sd_rx).await;
        })
    };
    wait_for_sock_ready(&sock, Duration::from_secs(5)).await?;

    let mut children: Vec<ChildHandle> = Vec::with_capacity(NUM_DISPLAYS as usize);
    for slot in 0..NUM_DISPLAYS {
        let mut spec = ChildSpec {
            role: "display",
            socket: sock.clone(),
            vk_uuid: dev_meta.uuid,
            slot,
            display_name: Some(format!("self-test-display-{slot}")),
            instance_id: Some(format!("self-test-{slot}-{}", std::process::id())),
            max_frames: Some(FRAMES as u64),
            capture_stdout: true,
        };
        let child = spawn(&spec).context("spawn display child")?;
        spec.role = "display"; // keep clippy happy with the borrow
        children.push(ChildHandle::new(child, slot));
    }

    // Display children connect, register, and the router auto-links each
    // to our stub renderer (it's the only one). We wait until every
    // display is bound to the renderer's current generation — only then
    // are FrameReady events fanned out (see Router::on_renderer_frame).
    renderer.push_self_test_event(make_bind_event(&renderer));
    if let Err(e) = wait_for_displays_bound(&router, NUM_DISPLAYS).await {
        log::warn!("fanout: displays did not all register: {e}");
    }

    let mut report = Fanout {
        frames: FRAMES,
        ok: 0,
        display_kill_at: None,
        kill_recovered_ms: None,
        refcount_leaks: 0,
    };

    let imgs = [img0.image, img1.image];
    for n in 0..FRAMES {
        let slot = (n & 1) as usize;
        let (color_f, _) = super::render_loop::color_for(n);

        let signal_sem = create_binary_sync_fd_exportable(vkd)?;
        unsafe {
            vkd.device
                .reset_command_buffer(cmdbuf.buf, vk::CommandBufferResetFlags::empty())?;
            vkd.device.begin_command_buffer(
                cmdbuf.buf,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            vkd.device.cmd_clear_color_image(
                cmdbuf.buf,
                imgs[slot],
                vk::ImageLayout::GENERAL,
                &vk::ClearColorValue { float32: color_f },
                &[vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1)],
            );
            vkd.device.end_command_buffer(cmdbuf.buf)?;
            let sigs = [signal_sem];
            let bufs = [cmdbuf.buf];
            vkd.device.queue_submit(
                vkd.queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&bufs)
                    .signal_semaphores(&sigs)],
                vk::Fence::null(),
            )?;
        }
        let sync_fd = export_signaled_sync_fd(vkd, signal_sem)
            .context("export signaled SYNC_FD")?;
        renderer.push_self_test_sync_fd(n as u64, sync_fd);

        renderer.push_self_test_event(EventMsg::FrameReady {
            image_index: slot as u32,
            seq: n as u64,
            ts_ns: 0,
            release_point: (n as u64) + 1,
        });

        unsafe {
            vkd.device.queue_wait_idle(vkd.queue)?;
            vkd.device.destroy_semaphore(signal_sem, None);
        }

        // Pace frames so the consumer has a chance to drain its socket
        // and signal release_syncobj before we push the next frame.
        // Production renderers run at 60 Hz; matching that is fine
        // here.
        tokio::time::sleep(FRAME_PACE).await;
    }

    log::info!("fanout: producer pushed {FRAMES} frames; waiting for displays");

    // Give the children up to 3 seconds to drain the last frames and exit.
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < drain_deadline {
        if children.iter_mut().all(|c| c.poll_exited()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = sd_tx.send(true);
    let _ = serve_handle.await;

    // Now collect statuses (this also reaps remaining children).
    let mut total_ok = 0u64;
    let mut total_mismatch = 0u64;
    let mut total_frames = 0u64;
    let mut had_fatal = false;
    for c in &mut children {
        let status = c.collect_status();
        log::info!(
            "fanout: display#{} frames={} ok={} mismatch={} clean={} fatal={:?}",
            c.slot, status.frames, status.ok, status.mismatch, status.clean_exit, status.fatal
        );
        total_ok += status.ok;
        total_mismatch += status.mismatch;
        total_frames += status.frames;
        if status.fatal.is_some() {
            had_fatal = true;
        }
    }

    let expected_total = NUM_DISPLAYS as u64 * FRAMES as u64;
    report.refcount_leaks = u32::try_from(expected_total.saturating_sub(total_frames))
        .unwrap_or(u32::MAX);
    let frames_per_display_ok = total_ok / NUM_DISPLAYS as u64;
    report.ok = u32::try_from(frames_per_display_ok).unwrap_or(u32::MAX);
    if had_fatal {
        log::warn!("fanout: at least one display child reported fatal");
    }
    if total_mismatch > 0 {
        log::warn!("fanout: total color mismatches across displays: {total_mismatch}");
    }

    unsafe {
        let _ = vkd.device.device_wait_idle();
        vkd.device.free_memory(img0.memory, None);
        vkd.device.free_memory(img1.memory, None);
        vkd.device.destroy_image(img0.image, None);
        vkd.device.destroy_image(img1.image, None);
    }
    cmd::destroy(vkd, cmdbuf);

    Ok(report)
}

fn pick_modifier(
    vkd: &VkDevice,
    instance: &ash::Instance,
    phys: vk::PhysicalDevice,
    cross_gpu: bool,
) -> Result<u64> {
    if cross_gpu {
        return Ok(0);
    }
    let entries = super::super::vk::modifier::query_supported(instance, phys, FORMAT)?;
    let _ = vkd;
    if let Some(e) = entries
        .iter()
        .find(|e| e.modifier != 0 && super::super::vk::modifier::supports_clear_and_export(e))
    {
        return Ok(e.modifier);
    }
    Ok(0)
}

fn make_bind_event(renderer: &RendererHandle) -> EventMsg {
    let snap = renderer.bind_snapshot();
    let g = snap.lock().unwrap();
    let s = g.as_ref().expect("bind_snapshot set above");
    EventMsg::BindBuffers {
        generation: s.generation,
        flags: s.flags,
        count: s.count,
        fourcc: s.fourcc,
        width: s.width,
        height: s.height,
        modifier: s.modifier,
        planes_per_buffer: s.planes_per_buffer,
        stride: s.stride.clone(),
        plane_offset: s.plane_offset.clone(),
        size: s.size.clone(),
    }
}

async fn wait_for_sock_ready(path: &std::path::Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(s) => {
                drop(s);
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(anyhow!("socket {} never bound: {e}", path.display())),
        }
    }
}

async fn wait_for_displays_bound(router: &Arc<Router>, n: u32) -> Result<()> {
    let deadline = Instant::now() + DISPLAY_REGISTER_TIMEOUT;
    loop {
        let snap = router.snapshot_displays().await;
        if snap.len() >= n as usize && snap.iter().all(|d| !d.links.is_empty()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "only {}/{n} display(s) bound after {:?}",
                snap.len(),
                DISPLAY_REGISTER_TIMEOUT
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct ChildHandle {
    child: Option<Child>,
    slot: u32,
    status: Option<ChildStatus>,
}

impl ChildHandle {
    fn new(child: Child, slot: u32) -> Self {
        Self {
            child: Some(child),
            slot,
            status: None,
        }
    }

    fn poll_exited(&mut self) -> bool {
        let Some(c) = self.child.as_mut() else {
            return true;
        };
        match c.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(_) => true,
        }
    }

    fn collect_status(&mut self) -> ChildStatus {
        let Some(mut child) = self.child.take() else {
            return self.status.clone().unwrap_or(ChildStatus {
                role: "display".into(),
                slot: self.slot,
                frames: 0,
                ok: 0,
                mismatch: 0,
                clean_exit: false,
                fatal: Some("no child".into()),
            });
        };
        // Read stdout to EOF so we can parse the JSON status line. The
        // child closes stdout on exit, so this also waits for it.
        let stdout = child.stdout.take();
        let status_from_lines = stdout.and_then(|s| {
            let reader = BufReader::new(s);
            let mut last: Option<ChildStatus> = None;
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    continue;
                }
                if let Ok(s) = serde_json::from_str::<ChildStatus>(trimmed) {
                    last = Some(s);
                }
            }
            last
        });
        let _ = child.wait();
        let s = status_from_lines.unwrap_or(ChildStatus {
            role: "display".into(),
            slot: self.slot,
            frames: 0,
            ok: 0,
            mismatch: 0,
            clean_exit: false,
            fatal: Some("child emitted no status".into()),
        });
        self.status = Some(s.clone());
        s
    }
}

impl Clone for ChildStatus {
    fn clone(&self) -> Self {
        Self {
            role: self.role.clone(),
            slot: self.slot,
            frames: self.frames,
            ok: self.ok,
            mismatch: self.mismatch,
            clean_exit: self.clean_exit,
            fatal: self.fatal.clone(),
        }
    }
}

fn make_socket_dir() -> Result<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("XDG_RUNTIME_DIR is not set"))?;
    let dir = std::path::PathBuf::from(runtime).join(format!(
        "waywallen-test-{}-fanout",
        std::process::id()
    ));
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).context("create fanout endpoint dir")?;
    Ok(dir)
}
