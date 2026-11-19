//! Error type for the tool download/install pipeline.

use std::fmt;
use std::path::PathBuf;

/// Failure modes of [`crate::install_tool`] and its helpers.
#[derive(Debug)]
pub enum ToolInstallError {
    /// A `ProxyConfig::Custom` URL or a proxy URL read from the environment
    /// (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`) failed to parse as
    /// http/https/socks5.
    InvalidProxyUrl { url: String, source: reqwest::Error },
    /// The override is not an allowed source for this tool.
    InvalidDownloadUrl,
    /// The HTTP download client could not be built.
    ClientBuild { source: reqwest::Error },
    /// Sending the download request failed (DNS, TLS, connection refused, ...).
    Http { url: String, source: reqwest::Error },
    /// The server answered with a non-success status (after redirects).
    HttpStatus {
        url: String,
        status: reqwest::StatusCode,
    },
    /// Filesystem operation failed at `path`.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Refusing to install through a symlink/junction (install root or tool dir) —
    /// it could redirect the install somewhere unintended.
    UnsafePath { path: PathBuf },
    /// Archive member name is absolute, carries a drive letter/UNC prefix or contains
    /// a `..` component (zip-slip guard; applies to every member, not only extracted ones).
    UnsafeZipEntry { member: String },
    /// The zip container (central directory, member metadata) could not be read.
    Zip { source: zip::result::ZipError },
    /// Reading an accepted archive member failed or ended short of its declared size.
    ArchiveRead {
        member: String,
        source: std::io::Error,
    },
    /// An archive member declares an uncompressed size above the per-member cap
    /// (archive bomb guard).
    OversizedZipEntry { member: String, declared: u64 },
    /// A file declared in the tool spec was not found in the archive.
    MissingArchiveEntry { tool: String, file_name: String },
    /// The archive digest does not match the pinned sha256.
    ChecksumMismatch { expected: String, actual: String },
    /// `manifest.json` exists but cannot be parsed.
    Manifest {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl fmt::Display for ToolInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProxyUrl { url, .. } => write!(
                f,
                "invalid proxy URL {url:?} (expected http://, https:// or socks5://)"
            ),
            Self::InvalidDownloadUrl => {
                f.write_str("the download URL is not an allowed HTTPS source for this tool")
            }
            Self::ClientBuild { .. } => write!(f, "failed to build the HTTP download client"),
            Self::Http { url, .. } => write!(f, "download request failed for {url}"),
            Self::HttpStatus { url, status } => {
                write!(f, "download of {url} failed with HTTP status {status}")
            }
            Self::Io { path, .. } => {
                write!(f, "filesystem operation failed at {}", path.display())
            }
            Self::UnsafePath { path } => write!(
                f,
                "{} is a symlink/junction or not a normal directory; refusing to install through it",
                path.display()
            ),
            Self::UnsafeZipEntry { member } => write!(
                f,
                "refusing archive member with unsafe path {member:?} \
                 (absolute path, drive letter or `..` component)"
            ),
            Self::Zip { .. } => write!(f, "failed to read zip archive"),
            Self::ArchiveRead { member, .. } => {
                write!(f, "failed to read archive member {member:?}")
            }
            Self::OversizedZipEntry { member, declared } => write!(
                f,
                "archive member {member:?} declares {declared} uncompressed bytes, \
                 above the per-member cap"
            ),
            Self::MissingArchiveEntry { tool, file_name } => write!(
                f,
                "archive for tool {tool:?} does not contain expected file {file_name:?}"
            ),
            Self::ChecksumMismatch { expected, actual } => write!(
                f,
                "downloaded archive sha256 mismatch: pinned {expected}, measured {actual}"
            ),
            Self::Manifest { path, .. } => {
                write!(f, "install manifest {} is not valid JSON", path.display())
            }
        }
    }
}

impl std::error::Error for ToolInstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidProxyUrl { source, .. }
            | Self::ClientBuild { source }
            | Self::Http { source, .. } => Some(source),
            Self::Io { source, .. } | Self::ArchiveRead { source, .. } => Some(source),
            Self::Zip { source } => Some(source),
            Self::Manifest { source, .. } => Some(source),
            _ => None,
        }
    }
}
