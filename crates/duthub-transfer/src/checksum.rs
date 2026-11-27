//! Client-requested whole-file verification.
//!
//! A client opts in by declaring the digest of the *whole* file on the creation
//! request — `Upload-Checksum: <algorithm> <lowercase hex>` — which is exactly
//! what the `ztus` CLI sends. Without that header the receiver never hashes
//! anything: a plain LAN transfer pays no verification cost at all. When the
//! header is present the file is hashed once, after its last byte is committed.
//!
//! This is deliberately not the tus `checksum` extension: that one checksums
//! every PATCH chunk and is not advertised here.

use std::io::{self, Read};
use std::path::Path;

use blake3::Hasher;
use sha2::{Digest, Sha256};

/// Metadata key the receiver persists the declared digest under.
pub const METADATA_KEY: &str = "checksum";

/// Hash algorithms a client may declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// BLAKE3: memory-mapped and parallel, the default choice for `curl` flows.
    Blake3,
    /// SHA-256: the strongest algorithm the `ztus` CLI can compute.
    Sha256,
}

impl Algorithm {
    /// Resolve an `Upload-Checksum` algorithm token (case-insensitive).
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "blake3" => Some(Self::Blake3),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }

    /// Token as it appears on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blake3 => "blake3",
            Self::Sha256 => "sha256",
        }
    }

    /// Length of the lowercase hex digest.
    const fn digest_len(self) -> usize {
        // Both BLAKE3 and SHA-256 emit 32 bytes.
        match self {
            Self::Blake3 | Self::Sha256 => 64,
        }
    }
}

/// Expected digest of the whole file, declared at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algorithm: Algorithm,
    /// Lowercase hex.
    pub digest: String,
}

/// Why an `Upload-Checksum` value cannot be honored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChecksumError {
    /// Not shaped like `<algorithm> <hex digest>`.
    #[error("malformed Upload-Checksum header")]
    Malformed,
    /// The algorithm is not one this receiver can verify.
    #[error("unsupported checksum algorithm: {0}")]
    Unsupported(String),
}

impl Checksum {
    /// Parse an `Upload-Checksum` header value. Uppercase hex is accepted and
    /// normalized; anything else is rejected rather than silently unverified.
    pub fn parse_header(value: &str) -> Result<Self, ChecksumError> {
        let (algorithm, digest) = value
            .trim()
            .split_once(char::is_whitespace)
            .ok_or(ChecksumError::Malformed)?;
        let algorithm = Algorithm::parse(algorithm)
            .ok_or_else(|| ChecksumError::Unsupported(algorithm.trim().to_owned()))?;
        let digest = digest.trim().to_ascii_lowercase();
        if digest.len() != algorithm.digest_len() || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(ChecksumError::Malformed);
        }
        Ok(Self { algorithm, digest })
    }

    /// Wire form persisted in the upload metadata: `<algorithm>:<digest>`.
    #[must_use]
    pub fn to_metadata(&self) -> String {
        format!("{}:{}", self.algorithm.as_str(), self.digest)
    }

    /// Inverse of [`Self::to_metadata`]; `None` for anything else.
    #[must_use]
    pub fn from_metadata(value: &str) -> Option<Self> {
        let (algorithm, digest) = value.split_once(':')?;
        let checksum = Self::parse_header(&format!("{algorithm} {digest}")).ok()?;
        (checksum.to_metadata() == value).then_some(checksum)
    }

    /// Digest the whole file at `path` (lowercase hex) in a single pass. This
    /// blocks for the duration of the read: call it from a blocking task.
    pub fn digest_file(&self, path: &Path) -> io::Result<String> {
        match self.algorithm {
            Algorithm::Blake3 => Ok(Hasher::new()
                .update_mmap_rayon(path)?
                .finalize()
                .to_hex()
                .to_string()),
            Algorithm::Sha256 => {
                let mut file =
                    io::BufReader::with_capacity(1024 * 1024, std::fs::File::open(path)?);
                let mut hasher = Sha256::new();
                let mut buffer = vec![0u8; 1024 * 1024];
                loop {
                    let read = file.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
                Ok(hex(&hasher.finalize()))
            }
        }
    }
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLAKE3_ABC: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn header_parses_both_algorithms_and_normalizes_hex() {
        let blake3 = Checksum::parse_header(&format!("blake3 {BLAKE3_ABC}")).unwrap();
        assert_eq!(blake3.algorithm, Algorithm::Blake3);
        assert_eq!(blake3.digest, BLAKE3_ABC);
        let upper =
            Checksum::parse_header(&format!("SHA256 {}", SHA256_ABC.to_uppercase())).unwrap();
        assert_eq!(upper.algorithm, Algorithm::Sha256);
        assert_eq!(upper.digest, SHA256_ABC);
        // Extra whitespace around the tokens is tolerated.
        assert_eq!(
            Checksum::parse_header(
                "  blake3   <digest>  "
                    .replace("<digest>", BLAKE3_ABC)
                    .as_str()
            )
            .unwrap(),
            blake3
        );
    }

    #[test]
    fn header_rejects_malformed_values() {
        assert_eq!(
            Checksum::parse_header("blake3"),
            Err(ChecksumError::Malformed)
        );
        assert_eq!(Checksum::parse_header(""), Err(ChecksumError::Malformed));
        // Wrong length and non-hex digests never degrade to "unverified".
        assert_eq!(
            Checksum::parse_header(&format!("blake3 {}", &BLAKE3_ABC[..63])),
            Err(ChecksumError::Malformed)
        );
        assert_eq!(
            Checksum::parse_header(&format!("blake3 {}", "z".repeat(64))),
            Err(ChecksumError::Malformed)
        );
    }

    #[test]
    fn unsupported_algorithm_is_named_for_the_client() {
        // ztus falls back from sha1 to sha256 when the server names the problem.
        assert_eq!(
            Checksum::parse_header(&format!("sha1 {}", "a".repeat(40))),
            Err(ChecksumError::Unsupported("sha1".to_string()))
        );
    }

    #[test]
    fn metadata_roundtrip_is_canonical() {
        let checksum = Checksum::parse_header(&format!("blake3 {BLAKE3_ABC}")).unwrap();
        assert_eq!(checksum.to_metadata(), format!("blake3:{BLAKE3_ABC}"));
        assert_eq!(
            Checksum::from_metadata(&checksum.to_metadata()),
            Some(checksum)
        );
        for junk in ["", "blake3", "blake3:", "md5:abcd", "blake3:zz"] {
            assert_eq!(Checksum::from_metadata(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn digest_file_matches_known_vectors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        let blake3 = Checksum {
            algorithm: Algorithm::Blake3,
            digest: BLAKE3_ABC.to_string(),
        };
        let sha256 = Checksum {
            algorithm: Algorithm::Sha256,
            digest: SHA256_ABC.to_string(),
        };
        assert_eq!(blake3.digest_file(&path).unwrap(), BLAKE3_ABC);
        assert_eq!(sha256.digest_file(&path).unwrap(), SHA256_ABC);
        // An empty file is a legal (zero-length) upload.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            blake3.digest_file(&path).unwrap(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            sha256.digest_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
