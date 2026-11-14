//! `type = "static"`: fixed values written once at start. Handy for demos,
//! for laying out scenes before a real source exists, and in tests.
//!
//! ```toml
//! [sources.demo]
//! type = "static"
//! values = { "demo:temp" = 21.5, "demo:status" = "ok", "demo:on" = true }
//! ```

use super::{Source, SourceCtx, SourceFuture};
use crate::config::{StaticSourceConfig, StaticValue};
use crate::store::{Change, Value};

pub struct StaticSource {
    values: Vec<(String, Value)>,
}

impl StaticSource {
    pub fn new(_name: &str, cfg: &StaticSourceConfig) -> Self {
        StaticSource {
            values: cfg
                .values
                .iter()
                .map(|(k, v)| {
                    let value = match v {
                        StaticValue::Bool(b) => Value::Bool(*b),
                        StaticValue::Number(n) => Value::Number {
                            value: *n,
                            unit: None,
                        },
                        StaticValue::Text(t) => Value::Text(t.clone()),
                    };
                    (k.clone(), value)
                })
                .collect(),
        }
    }
}

impl Source for StaticSource {
    fn run(self: Box<Self>, mut ctx: SourceCtx) -> SourceFuture {
        Box::pin(async move {
            ctx.store.apply(
                self.values
                    .into_iter()
                    .map(|(k, v)| Change::Set(k, v))
                    .collect(),
            );
            ctx.ok();
            ctx.shutdown.cancelled().await;
            Ok(())
        })
    }
}
