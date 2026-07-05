// Regression gate: production code must not panic via unwrap/expect. Test
// builds (unit-test modules live inside src files) are exempt via cfg(test).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod api;
pub mod app;
pub mod attach_target;
pub mod cli;
pub mod clipboard;
pub mod command_history;
pub mod config;
pub mod filtering;
pub mod input;
pub mod io;
pub mod ipc;
#[cfg(unix)]
pub mod runtime;
pub mod session;
pub mod storage;
pub mod ui;
#[cfg(unix)]
pub mod upgrade;
pub mod xdg;
