//! Re-export shim — real implementation lives in `agent-bus-core`.
//!
//! All `crate::models::*` imports in this crate continue to work unchanged.
pub(crate) use agent_bus_core::models::*;
