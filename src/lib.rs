// `async_trait` expansion emits `#[must_use]` on generated fns returning
// `Pin<Box<dyn Future>>` — already must_use, so clippy 1.99's
// `double_must_use` fires on macro output we don't control.
#![allow(unknown_lints)]
#![allow(clippy::double_must_use)]

pub mod channel;
pub mod code_mode;
pub mod config;
pub mod domain;
pub mod harness;
pub mod http;
pub(crate) mod json_utils;
pub mod mcp;
pub mod mcp_commands;
pub mod memory;
pub mod provider;
pub mod scheduler;
pub mod service;
pub mod skills;
pub mod skills_commands;
