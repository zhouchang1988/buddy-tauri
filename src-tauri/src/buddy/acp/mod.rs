//! ACP (Agent Client Protocol) execution channel, port of
//! `src/main/buddy/acp/` from the Electron edition: JSON-RPC 2.0 wire types,
//! a newline-delimited stdio transport over `tokio::process`, and the client
//! driving initialize/session/prompt/cancel against an external agent.

pub mod agent_catalog;
pub mod client;
pub mod codex_home;
pub mod models;
pub mod transport;
pub mod types;

pub use agent_catalog::{
    check_global_acp_adapters, default_acp_args, ensure_wecode_codex_wrapper,
    find_global_executable, find_wecode_path, prepare_acp_environment, resolve_acp_binary,
    ResolvedAcpBinary,
};
pub use client::{
    AcpClient, AcpClientOptions, AcpPromptHandlers, ExecuteToolFn, ExecuteToolFuture,
};
pub use codex_home::{codex_isolation_wrapper, prepare_codex_home};
pub use models::{
    apply_acp_session_model, extract_acp_session_models, probe_acp_models, AcpModelClient,
    AcpModelSource, ExtractedAcpModels, ProbeAcpModelsOptions,
};
pub use transport::{AcpStdioTransport, AcpTransport, AcpTransportEvent, StdioTransportOptions};
pub use types::*;
