//! Buddy core: filesystem store, runner state machine, actor launchers,
//! parsers, git integration — the Rust port of `src/main/buddy/`.

pub mod coalesce;
pub mod commit_message;
pub mod defaults;
pub mod events;
pub mod git;
pub mod launchers;
pub mod locks;
pub mod model_detect;
pub mod notifications;
pub mod parsers;
pub mod paths;
pub mod prompts;
pub mod queue_coordinator;
pub mod redact;
pub mod runner;
pub mod schemas;
pub mod service;
pub mod session_insight;
pub mod shell_path;
pub mod store;
pub mod task_id;
pub mod types;
