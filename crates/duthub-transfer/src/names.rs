//! Publish-name and board-name validation.
//!
//! Single-component filename rules mirror `duthub-core` `public_filename`
//! with two additions: Windows reserved device names are rejected in every
//! casing and stem form (`CON`, `con.img`, `NUL.iso`, ...), and a publish name's
//! extension must appear in the configured whitelist. The publish name is what
//! the tus disk store finalizes a completed upload to, so it must never carry
//! path separators or anything a filesystem would reinterpret.

/// Publish-name / board-name validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// Name violates single-component filename rules.
    #[error("invalid name: {0:?}")]
    InvalidName(String),
    /// Windows reserved device name in any casing or stem form.
    #[error("reserved device name: {0:?}")]
    ReservedDeviceName(String),
    /// Extension outside the configured whitelist.
    #[error("extension not allowed: {0:?}")]
    ExtensionNotAllowed(String),
}

/// Base single-component rules shared by publish names and board names.
fn base_filename_rules(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with([' ', '.'])
        && name.encode_utf16().count() <= 255
        && !name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
        && !name.chars().any(char::is_control)
}

/// `true` when the part before the first dot is a Windows reserved device
/// name. Windows treats `NUL.txt`, `NUL.tar.gz`, `COM0`, `COM¹`, and `CON .img`
/// as reserved devices, so all forms are rejected via the shared core guard.
fn is_reserved_device_name(name: &str) -> bool {
    duthub_core::path_guard::is_windows_reserved_name(name)
}

/// Validate a publish (artifact) filename: base rules, reserved device names
/// and the extension whitelist (case-insensitive).
pub fn validate_publish_name(
    name: &str,
    allowed_extensions: &[String],
) -> Result<(), PublishError> {
    if !base_filename_rules(name) {
        return Err(PublishError::InvalidName(name.to_string()));
    }
    if is_reserved_device_name(name) {
        return Err(PublishError::ReservedDeviceName(name.to_string()));
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return Err(PublishError::ExtensionNotAllowed(name.to_string()));
    };
    if allowed_extensions
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(ext))
    {
        Ok(())
    } else {
        Err(PublishError::ExtensionNotAllowed(name.to_string()))
    }
}

/// Validate a board name: base rules plus reserved device names. The board is
/// an authorization scope, not a filesystem component.
pub fn validate_board_name(name: &str) -> Result<(), PublishError> {
    if !base_filename_rules(name) {
        return Err(PublishError::InvalidName(name.to_string()));
    }
    if is_reserved_device_name(name) {
        return Err(PublishError::ReservedDeviceName(name.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extensions() -> Vec<String> {
        vec!["img".to_string(), "iso".to_string()]
    }

    #[test]
    fn valid_publish_names_pass() {
        let exts = extensions();
        assert!(validate_publish_name("flash.img", &exts).is_ok());
        assert!(validate_publish_name("flash.ISO", &exts).is_ok());
        assert!(validate_publish_name("rk3576-boot.img", &exts).is_ok());
    }

    #[test]
    fn path_and_control_attacks_fail() {
        let exts = extensions();
        for bad in [
            "../../etc/passwd",
            "a/b.img",
            "a\\b.img",
            "..",
            ".hidden.img",
            "trailing.img.",
            "trailing space.img ",
            "with:colon.img",
            "star*.img",
        ] {
            assert!(validate_publish_name(bad, &exts).is_err(), "{bad:?}");
        }
        // Inner spaces and semicolons are legal Windows filename characters —
        // they must stay accepted (public_filename base rules).
        assert!(validate_publish_name("trailing .img", &exts).is_ok());
        assert!(validate_publish_name("semi;colon.img", &exts).is_ok());
    }

    #[test]
    fn reserved_device_names_fail_in_stem_form() {
        let exts = extensions();
        for bad in [
            "CON.img",
            "con.iso",
            "NUL.img",
            "com1.img",
            "LPT1",
            "Aux",
            // Windows resolves the device from the part before the first dot.
            "CON.tar.img",
            "com1.a.img",
            "NUL.x.iso",
        ] {
            assert!(validate_publish_name(bad, &exts).is_err(), "{bad:?}");
        }
        // Reserved stems with an odd shape still fail even before extensions.
        assert!(validate_board_name("CON").is_err());
        assert!(validate_board_name("CON.tar").is_err());
        assert!(validate_board_name("board1").is_ok());
    }

    #[test]
    fn extension_whitelist_is_enforced() {
        let exts = extensions();
        assert_eq!(
            validate_publish_name("evil.exe", &exts),
            Err(PublishError::ExtensionNotAllowed("evil.exe".to_string()))
        );
        assert_eq!(
            validate_publish_name("noext", &exts),
            Err(PublishError::ExtensionNotAllowed("noext".to_string()))
        );
    }
}
