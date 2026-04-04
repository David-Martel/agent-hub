//! Re-export shim — real implementation lives in `agent-bus-core`.
pub(crate) use agent_bus_core::ops::*;

// Submodule shims for code that imports directly from ops submodules.
pub(crate) mod admin {
    pub(crate) use agent_bus_core::ops::admin::*;
}
pub(crate) mod channel {
    pub(crate) use agent_bus_core::ops::channel::*;
}
pub(crate) mod claim {
    pub(crate) use agent_bus_core::ops::claim::*;
}
pub(crate) mod inbox {
    pub(crate) use agent_bus_core::ops::inbox::*;
}
pub(crate) mod inventory {
    pub(crate) use agent_bus_core::ops::inventory::*;
}
pub(crate) mod subscription {
    pub(crate) use agent_bus_core::ops::subscription::*;
}
pub(crate) mod task {
    pub(crate) use agent_bus_core::ops::task::*;
}
