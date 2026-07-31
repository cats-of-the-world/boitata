//! Shared substrate for boitata: LLM providers, tools, context management,
//! configuration, audit logging, and the MCP client. Both the agent and the
//! orchestrator build on these primitives.

pub mod audit;
pub mod config;
pub mod context;
pub mod mcp;
pub mod provider;
pub mod tools;
