//! Internal library surface. Exists so `src/main.rs` and anything under
//! `examples/` can share the bridge's modules in one compilation unit.
//!
//! Not published. The public items are the same items `main.rs` uses —
//! nothing marked `pub` here is a promise outside this repo.

// Pedantic lib-style doc rules are noise for an internal crate. We
// document errors in error-type `Display` impls and the module prose;
// `# Errors` sections per-fn would duplicate that. `must_use_candidate`
// fires on every non-`()` pub helper even when the caller clearly uses
// the result. `too_long_first_doc_paragraph` flags prose style that
// `rustdoc` renders fine. Silencing crate-wide keeps the non-pedantic
// `clippy::missing_errors_doc` / `clippy::must_use` / etc. still in
// force where they matter.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_long_first_doc_paragraph
)]

pub mod bridge;
pub mod config;
pub mod handler;
pub mod inspect;
pub mod metrics;
pub mod notify;
pub mod sanitize;
pub mod security;
pub mod session;
pub mod setup;
pub mod telegram;
pub mod tmux;
pub mod types;
