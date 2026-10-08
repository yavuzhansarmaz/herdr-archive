//! herdr-archive: archive idle agent tabs, restore them with sessions resumed.
//!
//! Rust port of the herdr-shelf Python plugin (behavior parity per
//! DESIGN.md). The binary (`src/main.rs`) dispatches subcommands; this
//! library exposes the modules so tests can drive logic directly.

pub mod activity;
pub mod agents;
pub mod api;
pub mod archive;
pub mod config;
pub mod confirm;
pub mod history;
pub mod log;
pub mod manual;
pub mod migrate;
pub mod picker;
pub mod picker_tty;
pub mod restore;
pub mod scan;
pub mod session;
pub mod sweep;
#[doc(hidden)]
pub mod testutil;
pub mod util;

/// Abstraction over the herdr socket client so logic can be tested with a
/// scripted fake as well as the real [`api::Client`].
pub trait Herdr {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, api::HerdrError>;
}

impl Herdr for api::Client {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, api::HerdrError> {
        api::Client::call(self, method, params)
    }
}
