//! The `ffmpeg` subprocess that fills a [`FrameSlot`](super::FrameSlot).
//!
//! M7 groundwork: the API only. The implementation (spawning `ffmpeg`
//! when the zone is wanted, reading `width*height*3`-byte frames from its
//! stdout, stopping it when idle, restarting with backoff, never logging the
//! URL) is `TODO(M7 decoder)`.

use std::sync::Arc;

use super::{DecoderSpec, Shared};

/// Keeps the decoder thread alive; dropping it stops the thread and any
/// running `ffmpeg`.
pub struct DecoderGuard {
    _private: (),
}

impl DecoderGuard {
    /// A guard that owns nothing (for detached handles).
    pub fn none() -> Self {
        DecoderGuard { _private: () }
    }
}

/// Start the decoder thread for `spec`, writing into `shared`.
pub fn spawn(_spec: DecoderSpec, _shared: Arc<Shared>) -> DecoderGuard {
    // TODO(M7 decoder): implement.
    tracing::warn!("video decoder not implemented yet; the zone stays black");
    DecoderGuard::none()
}
