//! Cheap filesystem stat. One syscall per file → size + mtime.

use std::fs;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    pub size: i64,
    /// File mtime in milliseconds since UNIX epoch.
    pub modified_at: i64,
}

pub fn stat_file(path: &str) -> Option<FileStat> {
    let md = fs::metadata(path).ok()?;
    let size = i64::try_from(md.len()).ok()?;
    let modified_at = md
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let modified_at = i64::try_from(modified_at).ok()?;
    Some(FileStat { size, modified_at })
}
