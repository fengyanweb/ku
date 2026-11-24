pub mod ast;
#[cfg(not(target_arch = "wasm32"))]
pub mod backend;
pub mod checker;
#[cfg(not(target_arch = "wasm32"))]
pub mod cli;
#[cfg(not(target_arch = "wasm32"))]
pub mod cli_contract;
pub mod env;
pub mod error;
mod interop;
#[cfg(not(target_arch = "wasm32"))]
pub mod ir;
pub mod lexer;
#[cfg(not(target_arch = "wasm32"))]
mod native_tls_archive;
#[cfg(not(target_arch = "wasm32"))]
pub mod package;
pub mod parser;
#[cfg(not(target_arch = "wasm32"))]
pub mod process_execution;
#[cfg(not(target_arch = "wasm32"))]
pub mod registry_admin;
#[cfg(not(target_arch = "wasm32"))]
pub mod registry_server;
#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
pub mod runner_bundle;
pub mod runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod service_supervision;
pub mod span;
pub mod stdlib;
pub mod token;

pub use runtime::{interpreter, value};

#[cfg(feature = "browser")]
pub mod browser;

pub const LANGUAGE_VERSION: &str = env!("CARGO_PKG_VERSION");
