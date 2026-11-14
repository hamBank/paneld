//! One TLS stack for the whole daemon: rustls with the `ring` provider.
//!
//! `ring` (not `aws-lc-rs`) keeps cross-compiling for the Pi simple: it only
//! needs a C cross-compiler (`gcc-aarch64-linux-gnu`), no CMake. reqwest and
//! tokio-tungstenite are built without their own provider choice, so the
//! process-wide default must be installed before the first TLS connection.

/// Install the `ring` provider as the process default. Idempotent; call it
/// before building any TLS client ([`crate::sources::http_client`] does).
pub fn init() {
    // Fails only if a provider is already installed, which is what we want.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
