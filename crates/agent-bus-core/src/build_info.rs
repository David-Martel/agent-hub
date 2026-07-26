//! Runtime build provenance shared by every transport binary.

/// Crate version plus the best-effort Git revision and commit date.
///
/// The Git component is `unknown` when built outside a Git checkout. This is
/// operational identity only; protocol compatibility remains governed by
/// [`crate::models::PROTOCOL_VERSION`].
pub const BUILD_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("AGENT_BUS_CORE_GIT_VERSION"),
    ")"
);
