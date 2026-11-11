//! The data store: latest value per key, written by sources, read by the
//! render loop as cheap immutable snapshots (DESIGN.md §5.3).
//!
//! Writes copy the map and swap the `Arc` (copy-on-write). Reads clone an
//! `Arc` under a briefly held lock, so the render loop never waits on a
//! source doing I/O.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// A stored value. More variants (`Image`, `Series`, `Json`) arrive with the
/// widgets and sources that need them.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number { value: f64, unit: Option<String> },
    Text(String),
    Bool(bool),
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
}
