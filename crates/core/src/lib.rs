//! ambidiff-core: every derivation shared by the three frontends.
//!
//! Frontends (TUI, nvim via the stdio engine, browser via wasm) only paint
//! rows and forward intents; everything they show is computed here so the
//! same code runs natively and in wasm. Native-only infrastructure (git,
//! filesystem review store, watching) sits behind the `native` feature.

pub mod anchor;
pub mod commands;
pub mod highlight;
pub mod lifecycle;
pub mod model;
pub mod parser;
pub mod projection;
pub mod protocol;
pub mod review;
pub mod rows;
pub mod sanitize;
pub mod search;
pub mod tree;
pub mod util;
pub mod view;
pub mod view_state;
pub mod worddiff;

pub mod source;

#[cfg(feature = "native")]
pub mod git_source;
// Root-relative reads use openat/readlinkat; other native targets get a
// stub that reports every read as unsupported rather than a wrong answer.
#[cfg(all(feature = "native", unix))]
mod rootio;
#[cfg(all(feature = "native", not(unix)))]
#[path = "rootio_stub.rs"]
mod rootio;
#[cfg(feature = "native")]
pub mod store;
#[cfg(feature = "native")]
pub mod sys;
#[cfg(feature = "native")]
pub mod watch;

#[cfg(feature = "wasm")]
pub mod wasm;
