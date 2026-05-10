//! Two-tier background probe scheduler.
//!
//! Each tick walks one batch of items and runs:
//!
//! * **stat tier** (cheap): `fs::metadata` for size + mtime, applied to
//!   every item that's stat-stale. Keeps the DB columns fresh for *all*
//!   files, not just media ones.
//! * **media tier** (expensive): libavformat for width/height/format,
//!   applied only to items with a probable media extension whose file
//!   has changed since (or never been) media-probed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::{DatabaseConnection, TransactionTrait};
use tokio::sync::watch;

use crate::error::{Result, ResultExt};
use crate::model::repo;
use crate::probe::media::{MediaMeta, MediaProbe};
use crate::probe::stat::{self, FileStat};
use crate::tasks::now_ms;

/// How often the scheduler wakes up to drain pending items.
pub const PROBE_TICK: Duration = Duration::from_secs(300);

/// Minimum gap between two stat attempts for the same item. Cheap
/// enough to redo hourly so file mtime changes are picked up promptly.
pub const STAT_COOLDOWN: Duration = Duration::from_secs(60 * 60);

/// Minimum gap between two media-probe attempts for the same item.
/// Expensive (dlopen + parse), so the cooldown is longer; mtime changes
/// short-circuit it via the OR clause in [`repo::list_items_pending`].
pub const PROBE_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);

/// Cap used by the post-sync one-shot path so a fresh import is
/// drained quickly rather than one tick at a time.
pub const PROBE_REFRESH_BATCH: usize = 256;

/// How many probed items to accumulate before flushing them in a single
/// SQLite transaction. Trades a bit of latency-on-error for amortizing
/// WAL fsync cost across many `UPDATE`s — at WAL `synchronous=NORMAL`
/// each commit fsyncs the WAL once, so 1 commit per 32 items beats 32
/// commits.
pub const COMMIT_BATCH: usize = 32;

/// Extensions we attempt to media-probe. Lowercased, no leading dot.
pub const PROBABLE_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "tif",
    "avif",
];

fn is_probable(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            PROBABLE_EXTS.iter().any(|p| *p == lower)
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProbeStats {
    pub candidates: usize,
    pub stat_done: usize,
    pub stat_changed: usize,
    pub media_probed: usize,
    pub gained_dimensions: usize,
    pub gained_format: usize,
    pub write_errors: usize,
    pub elapsed_ms: u128,
}

/// One item's probe result, queued up to be flushed in the next batch
/// commit. `path` is kept around purely for error-log context.
struct PendingWrite {
    id: i64,
    path: String,
    stat: Option<FileStat>,
    media: Option<MediaMeta>,
}

/// Drain up to `max` candidates in one pass; `None` means drain
/// everything pending. Each candidate may run the stat tier, the media
/// tier, both, or neither (the DB query already pre-filters; an item
/// showing up means at least one tier *might* have work). Probe work
/// (stat + libavformat) happens outside the DB; the resulting `UPDATE`s
/// are buffered and flushed every [`COMMIT_BATCH`] items in one tx.
pub async fn run_pending(
    db: &DatabaseConnection,
    probe: Arc<dyn MediaProbe>,
    max: Option<usize>,
) -> Result<ProbeStats> {
    let mut stats = ProbeStats::default();
    if max == Some(0) {
        return Ok(stats);
    }
    let started = std::time::Instant::now();
    let now = now_ms();
    let stat_cutoff = now - STAT_COOLDOWN.as_millis() as i64;
    let media_cutoff = now - PROBE_COOLDOWN.as_millis() as i64;
    let sql_limit = max
        .map(|m| (m as u64).saturating_mul(4))
        .unwrap_or(u64::MAX);
    let candidates = repo::list_items_pending(db, stat_cutoff, media_cutoff, sql_limit).await?;
    stats.candidates = candidates.len();

    let mut pending: Vec<PendingWrite> = Vec::with_capacity(COMMIT_BATCH);
    let mut handled = 0usize;
    for (item, library_root) in candidates {
        if let Some(m) = max {
            if handled >= m {
                break;
            }
        }
        let abs = join_path(&library_root, &item.path);

        // Tier 1 — stat (filesystem only; DB write deferred).
        let stat_stale = item
            .stat_at
            .map(|t| t < stat_cutoff)
            .unwrap_or(true);
        let mut stat_result: Option<FileStat> = None;
        let new_modified_at = if stat_stale {
            let abs_for_blocking = abs.clone();
            let s = tokio::task::spawn_blocking(move || stat::stat_file(&abs_for_blocking))
                .await
                .with_context(|| format!("stat join id={}", item.id))?;
            match s {
                Some(s) => {
                    stat_result = Some(s);
                    Some(s.modified_at)
                }
                // Missing / unreadable file — leave columns alone.
                None => item.modified_at,
            }
        } else {
            item.modified_at
        };

        // Tier 2 — media probe (libavformat only; DB write deferred).
        let media_due = is_probable(&item.path)
            && match (item.probed_at, new_modified_at) {
                (None, _) => true,
                (Some(p), _) if p < media_cutoff => true,
                (Some(p), Some(m)) if p < m => true,
                _ => item.width.is_none() || item.height.is_none() || item.format.is_none(),
            };
        let mut media_result: Option<MediaMeta> = None;
        if media_due {
            let probe_for_blocking = probe.clone();
            let abs_for_blocking = abs.clone();
            let meta = tokio::task::spawn_blocking(move || {
                probe_for_blocking.probe_media(&abs_for_blocking)
            })
            .await
            .with_context(|| format!("probe join id={}", item.id))?;

            if meta.width.is_some() || meta.height.is_some() {
                stats.gained_dimensions += 1;
            }
            if meta.format.is_some() {
                stats.gained_format += 1;
            }
            media_result = Some(meta);
        }

        if stat_result.is_some() || media_result.is_some() {
            pending.push(PendingWrite {
                id: item.id,
                path: abs,
                stat: stat_result,
                media: media_result,
            });
            if pending.len() >= COMMIT_BATCH {
                flush_pending(db, &mut pending, &mut stats).await;
            }
        }

        handled += 1;
    }

    flush_pending(db, &mut pending, &mut stats).await;

    stats.elapsed_ms = started.elapsed().as_millis();

    log::info!(
        target: "waywallen::probe::task",
        "probe pass done: candidates={} stat_done={} stat_changed={} media_probed={} +dims={} +format={} errors={} took={}ms",
        stats.candidates,
        stats.stat_done,
        stats.stat_changed,
        stats.media_probed,
        stats.gained_dimensions,
        stats.gained_format,
        stats.write_errors,
        stats.elapsed_ms,
    );
    Ok(stats)
}

pub async fn scheduler_loop(
    db: DatabaseConnection,
    probe: Arc<dyn MediaProbe>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    log::info!(
        "probe scheduler started (tick={:?}, stat_cooldown={:?}, media_cooldown={:?})",
        PROBE_TICK,
        STAT_COOLDOWN,
        PROBE_COOLDOWN,
    );
    let mut interval = tokio::time::interval(PROBE_TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    log::info!("probe scheduler exiting (shutdown)");
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                if let Err(e) = run_pending(&db, probe.clone(), None).await {
                    log::warn!("probe scheduler tick failed: {e:#}");
                }
            }
        }
    }
}

/// Commit a buffered batch of stat/media writes in one transaction. On
/// any per-item or commit error the whole tx is rolled back and the
/// items remain candidates for the next pass; partial-batch stat
/// counters are only credited to `stats` on commit success so the log
/// line never claims writes that didn't land.
async fn flush_pending(
    db: &DatabaseConnection,
    pending: &mut Vec<PendingWrite>,
    stats: &mut ProbeStats,
) {
    if pending.is_empty() {
        return;
    }
    let n = pending.len();
    let mut delta_stat_done = 0usize;
    let mut delta_stat_changed = 0usize;
    let mut delta_media_probed = 0usize;

    let outcome: Result<()> = async {
        let tx = db.begin().await.context("begin probe tx")?;
        for pw in pending.iter() {
            if let Some(s) = &pw.stat {
                let out = repo::update_item_stat(&tx, pw.id, s)
                    .await
                    .with_context(|| format!("stat write id={} path={}", pw.id, pw.path))?;
                delta_stat_done += 1;
                if out.changed {
                    delta_stat_changed += 1;
                }
            }
            if let Some(m) = &pw.media {
                repo::update_item_media(&tx, pw.id, m)
                    .await
                    .with_context(|| format!("probe write id={} path={}", pw.id, pw.path))?;
                delta_media_probed += 1;
            }
        }
        tx.commit().await.context("commit probe tx")?;
        Ok(())
    }
    .await;

    match outcome {
        Ok(_) => {
            stats.stat_done += delta_stat_done;
            stats.stat_changed += delta_stat_changed;
            stats.media_probed += delta_media_probed;
        }
        Err(e) => {
            log::warn!("probe batch flush failed (n={n}): {e:#}");
            stats.write_errors += n;
        }
    }
    pending.clear();
}

fn join_path(root: &str, rel: &str) -> String {
    let root = root.trim_end_matches('/');
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        root.to_owned()
    } else {
        format!("{root}/{rel}")
    }
}

#[allow(dead_code)]
pub(crate) type LibraryRootMap = HashMap<i64, String>;
