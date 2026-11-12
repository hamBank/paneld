//! The data store: latest value per key, written by sources, read by the
//! render loop as cheap immutable snapshots (DESIGN.md §5.3).
//!
//! Writes copy the map and swap the `Arc` (copy-on-write). Reads clone an
//! `Arc` under a briefly held lock, so the render loop never waits on a
//! source doing I/O.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::media::Bitmap;

/// A stored value. More variants (`Json`) arrive with the widgets
/// and sources that need them.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number {
        value: f64,
        unit: Option<String>,
    },
    Text(String),
    Bool(bool),
    /// Time series, oldest first. An immutable `Arc<[..]>` rather than a
    /// `Vec`: a sparkline clones it out of the snapshot every frame, which is
    /// one refcount bump, while appending a point (rare, off the render
    /// thread) copies at most `max_len` 16-byte points.
    Series(Arc<[(SystemTime, f64)]>),
    /// A decoded image. Shared, so cloning a value or a snapshot is cheap and
    /// widgets can tell a new image from the old one by pointer.
    Image(Arc<Bitmap>),
}

/// A value plus where and when it came from, so widgets can show staleness.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub value: Value,
    pub updated_at: SystemTime,
    pub source: Arc<str>,
}

/// An immutable view of the store at one point in time.
pub type StoreSnapshot = Arc<HashMap<String, Entry>>;

#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<RwLock<StoreSnapshot>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current contents. Cheap: one `Arc` clone.
    pub fn snapshot(&self) -> StoreSnapshot {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A writer that stamps every value with `source`.
    pub fn writer(&self, source: &str) -> StoreWriter {
        StoreWriter {
            store: self.clone(),
            source: source.into(),
        }
    }
}

/// Write handle given to one source.
#[derive(Clone)]
pub struct StoreWriter {
    store: Store,
    source: Arc<str>,
}

impl StoreWriter {
    pub fn set(&self, key: impl Into<String>, value: Value) {
        let entry = Entry {
            value,
            updated_at: SystemTime::now(),
            source: self.source.clone(),
        };
        let mut guard = self.store.inner.write().unwrap_or_else(|e| e.into_inner());
        Arc::make_mut(&mut guard).insert(key.into(), entry);
    }

    /// Append `(now, value)` to the series at `key`, keeping at most the
    /// newest `max_len` points (at least 1). A missing or non-series entry
    /// is replaced by a new series.
    pub fn push_point(&self, key: impl Into<String>, value: f64, max_len: usize) {
        self.push_point_at(key, SystemTime::now(), value, max_len);
    }

    /// Like [`push_point`](Self::push_point) with an explicit point time.
    pub fn push_point_at(
        &self,
        key: impl Into<String>,
        at: SystemTime,
        value: f64,
        max_len: usize,
    ) {
        let key = key.into();
        let max_len = max_len.max(1);
        // Read-modify-write under one lock so concurrent pushes don't lose points.
        let mut guard = self.store.inner.write().unwrap_or_else(|e| e.into_inner());
        let old: &[(SystemTime, f64)] = match guard.get(&key).map(|e| &e.value) {
            Some(Value::Series(s)) => s,
            _ => &[],
        };
        let keep = old.len().min(max_len - 1);
        let mut points = Vec::with_capacity(keep + 1);
        points.extend_from_slice(&old[old.len() - keep..]);
        points.push((at, value));
        let entry = Entry {
            value: Value::Series(points.into()),
            updated_at: SystemTime::now(),
            source: self.source.clone(),
        };
        Arc::make_mut(&mut guard).insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_isolated_from_later_writes() {
        let store = Store::new();
        let w = store.writer("test");
        w.set("a", Value::Bool(true));
        let before = store.snapshot();
        w.set("a", Value::Bool(false));
        w.set("b", Value::Text("hi".into()));
        assert_eq!(before.len(), 1);
        assert_eq!(before["a"].value, Value::Bool(true));
        let after = store.snapshot();
        assert_eq!(after["a"].value, Value::Bool(false));
        assert_eq!(&*after["b"].source, "test");
    }

    #[test]
    fn push_point_appends_and_trims() {
        use std::time::Duration;
        let store = Store::new();
        let w = store.writer("test");
        let at = |i: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(i);
        for i in 0..5 {
            w.push_point_at("s", at(i), i as f64, 3);
        }
        let snap = store.snapshot();
        let Value::Series(s) = &snap["s"].value else {
            panic!("expected series");
        };
        assert_eq!(&**s, &[(at(2), 2.0), (at(3), 3.0), (at(4), 4.0)]);
        // A snapshot taken earlier is unaffected by later pushes.
        w.push_point_at("s", at(5), 5.0, 3);
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].1, 4.0);
    }

    #[test]
    fn push_point_replaces_non_series_and_clamps_max_len() {
        let store = Store::new();
        let w = store.writer("test");
        w.set("k", Value::Bool(true));
        w.push_point("k", 1.0, 0);
        w.push_point("k", 2.0, 0);
        let snap = store.snapshot();
        let Value::Series(s) = &snap["k"].value else {
            panic!("expected series");
        };
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1, 2.0);
    }
}
