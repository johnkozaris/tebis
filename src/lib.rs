//! Internal library surface. Shared between `main.rs` and `examples/`.
//! Not a published API — `pub` is implementation detail.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_long_first_doc_paragraph
)]

pub mod agent_hooks;
pub mod audio;
pub mod bridge;
pub mod config;
pub mod env_file;
pub mod fsutil;
pub mod hooks_cli;
pub mod inspect;
pub mod lockfile;
pub mod metrics;
pub mod notify;
pub mod sanitize;
pub mod security;
pub mod service;
pub mod setup;
pub mod telegram;
pub mod tmux;
