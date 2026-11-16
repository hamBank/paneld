//! `type = "media"`: PNG and GIF files in a folder, as images in the store
//! (DESIGN.md §6 item 6). An `image` widget shows one with `key`.
//!
//! ```toml
//! [sources.photos]
//! type = "media"
//! dir = "media/photos"     # relative to the config file
//! rotate_s = 30            # optional: photos:current cycles through them
//! ```
//!
//! # Keys
//! Prefixed with the source's name (`[sources.photos]` writes `photos:*`):
//! - `<name>:<file stem>`: the image, e.g. `photos:beach` for `beach.png`.
//!   GIFs keep all their frames and animate.
//! - `<name>:count`: how many images are loaded.
//! - `<name>:current`, with `rotate_s`: one of the images, moving to the next
//!   (in file name order) every `rotate_s` seconds.
//!
//! # Files
//! - Only the folder itself, not subfolders. Extensions `.png` and `.gif`,
//!   any case; other files are ignored without a word.
//! - The stem must make a valid key name (letters, digits, `_`, `.`, `-`)
//!   and must not be `count` or `current`; other files are skipped with a
//!   warning.
//! - At most [`MAX_FILES`] images and [`MAX_TOTAL_BYTES`] of decoded pixels
//!   per source (by file name order); the rest are skipped with a warning.
//!   Each file has the `image` widget's limits (16 MiB, 4096 px a side).
//! - Images whose longer side exceeds `max_px` are scaled down when loaded,
//!   so the render loop only ever scales small images to their zone.
//!
//! # Scanning
//! The folder is scanned at start and every `interval_s`. A file is decoded
//! only when it is new or its size or modification time changed; removed
//! files remove their keys. Decoding runs on tokio's blocking pool. Each scan
//! is one [`StoreWriter::apply`](crate::store::StoreWriter::apply) batch.
//!
//! # Health
//! An unreadable folder counts an error (the old images stay). A file that
//! cannot be used is logged once per file and problem, and is not an error
//! of the source.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::{Source, SourceCtx, SourceFuture};
use crate::config::MediaSourceConfig;
use crate::media::{Media, MAX_FILE_BYTES};
use crate::store::{Change, Value};

/// Most images one source loads.
pub const MAX_FILES: usize = 256;
/// Most decoded pixel bytes one source holds (after `max_px`).
pub const MAX_TOTAL_BYTES: usize = 256 * 1024 * 1024;

/// What identifies a file's content without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct Loaded {
    id: FileId,
    media: Arc<Media>,
}

/// The result of one scan.
#[derive(Debug, Default)]
pub struct Scan {
    /// Stem to image, in name order.
    images: BTreeMap<String, Loaded>,
    /// One message per file that was skipped.
    pub problems: Vec<String>,
}

impl Scan {
    pub fn names(&self) -> Vec<&str> {
        self.images.keys().map(String::as_str).collect()
    }

    pub fn get(&self, stem: &str) -> Option<&Arc<Media>> {
        self.images.get(stem).map(|l| &l.media)
    }
}

/// Scan `dir`, reusing images from `prev` whose files did not change.
/// Blocking file I/O and decoding. `Err` only when the folder itself cannot
/// be read.
pub fn scan(name: &str, dir: &Path, max_px: u32, prev: &Scan) -> std::io::Result<Scan> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut out = Scan::default();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("png" | "gif")) || !path.is_file() {
            continue;
        }
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            out.problems.push(format!("{file}: the name is not UTF-8"));
            continue;
        };
        let valid = !stem.is_empty()
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && crate::condition::is_valid_key(&format!("{name}:{stem}"));
        if !valid {
            out.problems.push(format!(
                "{file}: skipped, the name before the extension must be letters, digits, '_', '.' and '-'"
            ));
        } else if matches!(stem, "count" | "current") {
            out.problems.push(format!(
                "{file}: skipped, `{stem}` is reserved for the {name}:{stem} key"
            ));
        } else if files.iter().any(|(s, _)| s == stem) {
            out.problems.push(format!(
                "{file}: skipped, another file is also called {stem:?}"
            ));
        } else {
            files.push((stem.to_owned(), path));
        }
    }
    files.sort();

    let mut total = 0usize;
    for (stem, path) in files {
        let file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if out.images.len() >= MAX_FILES {
            out.problems.push(format!(
                "{file}: skipped, at most {MAX_FILES} images per source"
            ));
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            out.problems.push(format!("{file}: cannot read file"));
            continue;
        };
        if meta.len() > MAX_FILE_BYTES {
            out.problems.push(format!(
                "{file}: skipped, {} bytes is over the {MAX_FILE_BYTES} byte limit",
                meta.len()
            ));
            continue;
        }
        let id = FileId {
            len: meta.len(),
            modified: meta.modified().ok(),
        };
        let loaded = match prev.images.get(&stem) {
            Some(l) if l.id == id => l.clone(),
            _ => match Media::load(&path) {
                Ok(m) => Loaded {
                    id,
                    media: Arc::new(m.downscaled(max_px)),
                },
                Err(e) => {
                    out.problems.push(format!("{file}: {e:#}"));
                    continue;
                }
            },
        };
        let bytes = loaded.media.decoded_bytes();
        if total + bytes > MAX_TOTAL_BYTES {
            out.problems.push(format!(
                "{file}: skipped, the source already holds {MAX_TOTAL_BYTES} bytes of images (lower max_px?)"
            ));
            continue;
        }
        total += bytes;
        out.images.insert(stem, loaded);
    }
    Ok(out)
}

/// Store changes from `prev` to `next`: set new and changed images, remove
/// gone ones, and update `count`.
pub fn diff(name: &str, prev: &Scan, next: &Scan) -> Vec<Change> {
    let key = |stem: &str| format!("{name}:{stem}");
    let mut out = Vec::new();
    for stem in prev.images.keys() {
        if !next.images.contains_key(stem) {
            out.push(Change::Remove(key(stem)));
        }
    }
    for (stem, l) in &next.images {
        let same = prev
            .images
            .get(stem)
            .is_some_and(|p| Arc::ptr_eq(&p.media, &l.media));
        if !same {
            out.push(Change::Set(key(stem), Value::Image(l.media.clone())));
        }
    }
    out.push(Change::Set(
        key("count"),
        Value::Number {
            value: next.images.len() as f64,
            unit: None,
        },
    ));
    out
}

/// Which image `<name>:current` shows, by name, so a rescan keeps the
/// rotation where it was.
#[derive(Debug, Default)]
pub struct Rotation {
    current: Option<String>,
}

impl Rotation {
    /// Keep the current image if it still exists, else move to the next
    /// name after it (or the first). Returns the change for `current`.
    pub fn settle(&mut self, name: &str, scan: &Scan) -> Change {
        let names = scan.names();
        let pick = match &self.current {
            Some(c) if names.contains(&c.as_str()) => Some(c.clone()),
            Some(c) => names
                .iter()
                .find(|n| **n > c.as_str())
                .or(names.first())
                .map(|n| (*n).to_owned()),
            None => names.first().map(|n| (*n).to_owned()),
        };
        self.current = pick;
        self.change(name, scan)
    }

    /// Move to the next image (wrapping). Returns the change for `current`.
    pub fn advance(&mut self, name: &str, scan: &Scan) -> Change {
        let names = scan.names();
        self.current = match &self.current {
            Some(c) => names
                .iter()
                .find(|n| **n > c.as_str())
                .or(names.first())
                .map(|n| (*n).to_owned()),
            None => names.first().map(|n| (*n).to_owned()),
        };
        self.change(name, scan)
    }

    fn change(&self, name: &str, scan: &Scan) -> Change {
        let key = format!("{name}:current");
        match self.current.as_deref().and_then(|c| scan.get(c)) {
            Some(m) => Change::Set(key, Value::Image(m.clone())),
            None => Change::Remove(key),
        }
    }
}

pub struct MediaFolderSource {
    name: String,
    dir: PathBuf,
    interval: Duration,
    rotate: Option<Duration>,
    max_px: u32,
}

impl MediaFolderSource {
    pub fn new(name: &str, cfg: &MediaSourceConfig) -> Self {
        MediaFolderSource {
            name: name.to_owned(),
            dir: cfg.dir().to_path_buf(),
            interval: Duration::from_secs_f64(cfg.interval_s.clamp(1.0, 86_400.0)),
            rotate: cfg
                .rotate_s
                .map(|r| Duration::from_secs_f64(r.clamp(0.001, 86_400.0))),
            max_px: cfg.max_px,
        }
    }
}

impl Source for MediaFolderSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            let MediaFolderSource {
                name,
                dir,
                interval,
                rotate,
                max_px,
            } = *self;
            let mut current = Arc::new(Scan::default());
            let mut rotation = Rotation::default();
            let mut logged: HashSet<String> = HashSet::new();
            let mut dir_failed = false;
            let now = Instant::now();
            let mut next_scan = now;
            let mut next_rotate = rotate.map(|r| now + r);
            loop {
                let now = Instant::now();
                if now >= next_scan {
                    next_scan = now + interval;
                    let (n, d, prev) = (name.clone(), dir.clone(), current.clone());
                    let result =
                        tokio::task::spawn_blocking(move || scan(&n, &d, max_px, &prev)).await;
                    match result {
                        Ok(Ok(next)) => {
                            for p in &next.problems {
                                if logged.insert(p.clone()) {
                                    tracing::warn!(source = %name, "media: {p}");
                                }
                            }
                            let mut changes = diff(&name, &current, &next);
                            if rotate.is_some() {
                                changes.push(rotation.settle(&name, &next));
                            }
                            ctx.store.apply(changes);
                            current = Arc::new(next);
                            dir_failed = false;
                            ctx.ok();
                        }
                        Ok(Err(e)) => {
                            if !dir_failed {
                                tracing::warn!(source = %name, dir = %dir.display(), "media: cannot read the folder: {e}");
                                dir_failed = true;
                            }
                            ctx.error();
                        }
                        Err(e) => {
                            tracing::error!(source = %name, "media: scan failed: {e}");
                            ctx.error();
                        }
                    }
                }
                if let (Some(r), Some(at)) = (rotate, next_rotate) {
                    if now >= at {
                        next_rotate = Some(now + r);
                        ctx.store.apply(vec![rotation.advance(&name, &current)]);
                    }
                }
                let wake = next_rotate.map_or(next_scan, |r| r.min(next_scan));
                let wait = wake.saturating_duration_since(Instant::now());
                if !ctx.shutdown.sleep(wait).await {
                    return Ok(());
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
