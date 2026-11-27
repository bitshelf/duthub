//! Error type for the tool download/install pipeline.

use std::path::PathBuf;

use thiserror::Error;

/// Failure modes of [`crate::install_tool`] and its helpers.
#[derive(Debug, Error)]
pub enum ToolInstallError {
    /// A `ProxyConfig::Custom` URL or a proxy URL read from the environment
    /// (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`) failed to parse as
    /// http/https/socks5.
    #[error("invalid proxy URL {url:?} (expected http://, https:// or socks5://)")]
    InvalidProxyUrl {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The override is not an allowed source for this tool.
    #[error("the download URL is not an allowed HTTPS source for this tool")]
    InvalidDownloadUrl,
    /// The HTTP download client could not be built.
    #[error("failed to build the HTTP download client")]
    ClientBuild {
        #[source]
        source: reqwest::Error,
    },
    /// Sending the download request failed (DNS, TLS, connection refused, ...).
    #[error("download request failed for {url}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The server answered with a non-success status (after redirects).
    #[error("download of {url} failed with HTTP status {status}")]
    HttpStatus {
        url: String,
        status: reqwest::StatusCode,
    },
    /// Filesystem operation failed at `path`.
    #[error("filesystem operation failed at {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Refusing to install through a symlink/junction (install root or tool dir) —
    /// it could redirect the install somewhere unintended.
    #[error(
        "{} is a symlink/junction or not a normal directory; refusing to install through it",
        path.display()
    )]
    UnsafePath { path: PathBuf },
    /// Archive member name is absolute, carries a drive letter/UNC prefix or contains
    /// a `..` component (zip-slip guard; applies to every member, not only extracted ones).
    #[error(
        "refusing archive member with unsafe path {member:?} \
         (absolute path, drive letter or `..` component)"
    )]
    UnsafeZipEntry { member: String },
    /// The zip container (central directory, member metadata) could not be read.
    #[error("failed to read zip archive")]
    Zip {
        #[source]
        source: zip::result::ZipError,
    },
    /// Reading an accepted archive member failed or ended short of its declared size.
    #[error("failed to read archive member {member:?}")]
    ArchiveRead {
        member: String,
        #[source]
        source: std::io::Error,
    },
    /// An archive member declares an uncompressed size above the per-member cap
    /// (archive bomb guard).
    #[error(
        "archive member {member:?} declares {declared} uncompressed bytes, \
         above the per-member cap"
    )]
    OversizedZipEntry { member: String, declared: u64 },
    /// A file declared in the tool spec was not found in the archive.
    #[error("archive for tool {tool:?} does not contain expected file {file_name:?}")]
    MissingArchiveEntry { tool: String, file_name: String },
    /// The archive digest does not match the pinned sha256.
    #[error("downloaded archive sha256 mismatch: pinned {expected}, measured {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    /// `manifest.json` exists but cannot be parsed.
    #[error("install manifest {} is not valid JSON", path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}
