#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
//! Client side of the agent control channel (ADR-0434): wire pieces shared with the app, a socket
//! client, and the MCP (stdio JSON-RPC) server built on it. No GUI, no async runtime.

pub mod client;
pub mod mcp;
pub mod wire;
