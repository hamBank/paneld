//! `paneld`: composes data into scenes and drives HUB75 LED panels.
//!
//! See `DESIGN.md` at the repository root for the architecture. As of
//! milestone M0 this crate contains the config, canvas, data store, scene
//! renderer with the `clock` and `text` widgets, the render loop and the
//! HTTP preview sink.

pub mod canvas;
pub mod color;
pub mod colorlight_sink;
pub mod condition;
pub mod config;
pub mod engine;
pub mod fonts;
pub mod png_io;
pub mod postprocess;
pub mod preview;
pub mod render;
pub mod scene;
pub mod scheduler;
pub mod store;
pub mod test_pattern;
pub mod widgets;
