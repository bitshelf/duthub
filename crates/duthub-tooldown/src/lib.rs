//! duthub-tooldown — download kernel for external tools (adb, ffmpeg, …).
//!
//! Table-driven [`ToolSpec`]s describe what to fetch and which files to extract. The
//! pipeline streams the archive into a staging directory inside the install root, verifies
//! its sha256 against the spec pin (unpinned downloads are recorded as unverified),
//! extracts with zip-slip rejection, then atomically moves the result to
//! `<install_root>/<tool id>/` together with a `manifest.json`. Installs are idempotent
//! (a valid install skips the download unless forced) and cooperative-cancellable via an
//! [`AtomicBool`].
//!
//! All paths are caller-supplied; the crate deliberately has no workspace-internal
//! dependencies and never touches settings files or the GUI.
//!
//! ```no_run
//! use std::path::Path;
//! use duthub_tooldown::{install_tool, InstallOptions, InstallOutcome, ProxyConfig, TOOL_ADB};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let opts = InstallOptions {
//!     install_root: Path::new("D:/portable/.duthub/tools"),
//!     force: false,
//!     cancel: None,
//!     proxy: &ProxyConfig::Env,
//!     progress: None,
//! };
//! match install_tool(&TOOL_ADB, &opts).await? {
//!     InstallOutcome::Installed(m) => println!("installed {:?}", m.executables),
//!     InstallOutcome::AlreadyInstalled(m) => println!("up to date: {:?}", m.executables),
//!     InstallOutcome::Cancelled => println!("cancelled"),
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod chunks;
mod error;
mod extract;
mod install;
mod manifest;
mod proxy;
mod spec;
mod timefmt;

pub use error::ToolInstallError;
pub use install::{
    DownloadProgress, InstallOptions, InstallOutcome, ProgressCallback, install_tool,
    install_tool_from_url, validate_download_url,
};
pub use manifest::{
    InstallManifest, installed_executable, installed_tool_dir, is_installed, read_manifest,
};
pub use proxy::ProxyConfig;
pub use spec::{
    EntryKind, ExtractEntry, TOOL_ADB, TOOL_FFMPEG, TOOL_GOOSE, ToolSpec, builtin_specs, spec_by_id,
};
