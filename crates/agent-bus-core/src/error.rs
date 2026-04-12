use thiserror::Error;

/// Core error type for agent-bus operations, conceptually aligned with MCP/JSON-RPC error classes 
/// where applicable.
#[derive(Error, Debug)]
pub enum AgentBusError {
    /// Maps to MCP InvalidParams (-32602): Schema mismatches, empty fields, bad priorities
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    /// Maps to MCP MethodNotFound (-32601) or ResourceNotFound: e.g. thread or channel does not exist
    #[error("Not found: {0}")]
    NotFound(String),

    // Backend errors bubble up as Internal errors (-32603) via MCP/HTTP boundaries
    #[error("Database error: {0}")]
    Postgres(#[from] postgres::Error),
    
    #[error("Bus/Redis error: {0}")]
    Redis(#[from] redis::RedisError),
    
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Catch-all for other underlying crashes, also maps to Internal error
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, AgentBusError>;
