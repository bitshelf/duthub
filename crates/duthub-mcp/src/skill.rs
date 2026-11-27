//! Generate and export the `duthub` Agent Skill from the single-source `skills/duthub/` package.
//!
//! `skills/duthub/SKILL.md` and `skills/duthub/references/*.md` are embedded directly
//! into the binary so `npx skills add bitshelf/duthub`, `duthub.exe`'s MCP `skill_export`
//! tool, and desktop Skill exports stay 100% in sync without requiring any server-side binary.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

/// Stable directory and frontmatter name for Agent Skills discovery (`npx skills add`).
pub const SKILL_NAME: &str = "duthub";

/// Default loopback HTTP endpoint placeholder inside `skills/duthub/SKILL.md`.
pub const HTTP_ENDPOINT: &str = "http://127.0.0.1:8797/mcp";

/// Executable name for optional local stdio snapshots.
pub const STDIO_BIN: &str = "duthub-mcp";

/// Canonical `SKILL.md` template embedded from the repository's `skills/duthub/` folder.
const SKILL_MD_TEMPLATE: &str = include_str!("../../../skills/duthub/SKILL.md");

/// Progressive-disclosure reference docs exported alongside `SKILL.md`.
const SKILL_REFERENCES: &[(&str, &str)] = &[
    (
        "tools-and-errors.md",
        include_str!("../../../skills/duthub/references/tools-and-errors.md"),
    ),
    (
        "recipes.md",
        include_str!("../../../skills/duthub/references/recipes.md"),
    ),
];

/// Generate the full skill text without writing files.
#[must_use]
pub fn generate_skill_md() -> String {
    generate_for_endpoint(HTTP_ENDPOINT)
}

#[must_use]
pub fn skill_for_listener(port: Option<u16>) -> String {
    let endpoint = port.map(|port| SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    skill_for_endpoint(endpoint)
}

/// Generate a skill using the listener's actual advertised endpoint (default `zh-CN` fallback).
#[must_use]
pub fn skill_for_endpoint(endpoint: Option<SocketAddr>) -> String {
    skill_for_endpoint_localized(endpoint, crate::i18n::McpLocale::ZhCn)
}

/// Generate a skill using the listener's actual advertised endpoint and active duthub locale.
#[must_use]
pub fn skill_for_endpoint_localized(
    endpoint: Option<SocketAddr>,
    locale: crate::i18n::McpLocale,
) -> String {
    match endpoint {
        Some(endpoint) => generate_for_endpoint(&format!("http://{endpoint}/mcp")),
        None => generate_for_endpoint(crate::i18n::skill_unbound_endpoint(locale)),
    }
}

fn generate_for_endpoint(http_endpoint: &str) -> String {
    SKILL_MD_TEMPLATE
        .replace("\r\n", "\n")
        .replace(HTTP_ENDPOINT, http_endpoint)
}

fn write_skill_bundle(dir: &Path, skill_md: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, skill_md)?;
    let refs_dir = dir.join("references");
    std::fs::create_dir_all(&refs_dir)?;
    for (filename, content) in SKILL_REFERENCES {
        std::fs::write(refs_dir.join(filename), content.replace("\r\n", "\n"))?;
    }
    Ok(path)
}

/// Write the complete `duthub` skill bundle into the work directory.
/// Return the resulting `SKILL.md` path.
pub fn export_skill_in(work_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = work_dir.join("skill").join(SKILL_NAME);
    write_skill_bundle(&dir, &generate_skill_md())
}

/// Write separate copies for different HTTP listeners so one cannot overwrite another's endpoint.
pub fn export_skill_for_listener(work_dir: &Path, port: Option<u16>) -> std::io::Result<PathBuf> {
    export_skill_for_listener_localized(work_dir, port, crate::i18n::McpLocale::ZhCn)
}

/// Write separate copies for different HTTP listeners using the active duthub locale.
pub fn export_skill_for_listener_localized(
    work_dir: &Path,
    port: Option<u16>,
    locale: crate::i18n::McpLocale,
) -> std::io::Result<PathBuf> {
    let endpoint = port.map(|port| SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    export_skill_for_endpoint_localized(work_dir, endpoint, locale)
}

/// Export for a specific advertised address and identify the export by its listener port.
pub fn export_skill_for_endpoint(
    work_dir: &Path,
    endpoint: Option<SocketAddr>,
) -> std::io::Result<PathBuf> {
    export_skill_for_endpoint_localized(work_dir, endpoint, crate::i18n::McpLocale::ZhCn)
}

/// Export for a specific advertised address using the active duthub locale.
pub fn export_skill_for_endpoint_localized(
    work_dir: &Path,
    endpoint: Option<SocketAddr>,
    locale: crate::i18n::McpLocale,
) -> std::io::Result<PathBuf> {
    let mut dir = work_dir.join("skill");
    if let Some(endpoint) = endpoint {
        dir.push(format!("http-{}", endpoint.port()));
    }
    let dir = dir.join(SKILL_NAME);
    write_skill_bundle(&dir, &skill_for_endpoint_localized(endpoint, locale))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify Agent Skills frontmatter, size budget, contract tool names, and deprecated-tool exclusion.
    #[test]
    fn generated_skill_md_covers_contract() {
        let md = generate_skill_md();
        assert!(md.starts_with("---\n"), "must start with YAML frontmatter");
        assert!(
            md.contains("name: duthub"),
            "frontmatter must declare name: duthub"
        );
        let desc_line = md
            .lines()
            .find(|line| line.starts_with("description:"))
            .expect("frontmatter must include description");
        let desc_val = desc_line.trim_start_matches("description:").trim();
        assert!(!desc_val.is_empty(), "description cannot be empty");
        assert!(
            desc_val.chars().count() <= 1024,
            "description must stay within 1024 chars per Agent Skills spec"
        );
        // Guard against unquoted `: ` in YAML frontmatter scalars, which YAML parsers reject
        // as invalid nested mappings (breaks `npx skills add`).
        let frontmatter = md
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .expect("frontmatter must be closed with ---")
            .0;
        for line in frontmatter.lines() {
            if let Some((_, raw_val)) = line.split_once(':') {
                let val = raw_val.trim();
                let quoted = (val.starts_with('"') && val.ends_with('"'))
                    || (val.starts_with('\'') && val.ends_with('\''));
                assert!(
                    quoted || !val.contains(": "),
                    "unquoted YAML frontmatter scalar contains ': ' (breaks YAML parsers): {line}"
                );
            }
        }
        assert!(
            md.lines().count() < 500,
            "core SKILL.md should stay concise (<500 lines) with details in references/"
        );
        assert!(
            md.contains(HTTP_ENDPOINT),
            "default HTTP endpoint must appear"
        );
        assert!(md.contains("127.0.0.1:8797/mcp"));
        assert!(md.contains("`dutState`"));
        assert!(md.contains("`serial_read`"));
        assert!(md.contains("`serial_exec`"));
        assert!(md.contains("`log_segment_read`"));
        assert!(md.contains("`agent_feedback`"));
        assert!(md.contains("`PLAN_MODE_ONLY`"));
        assert!(md.contains("duthub://dut/{dutId}/serial/screen"));
        for tool in [
            "serial_write",
            "control_power",
            "approval_request",
            "adb_shell",
        ] {
            assert!(
                !md.contains(tool),
                "deprecated tool name must not be recommended: {tool}"
            );
        }
    }

    #[test]
    fn exports_keep_listener_addresses_separate() {
        let work =
            std::env::temp_dir().join(format!("duthub-listener-skill-{}", std::process::id()));
        let first = export_skill_for_listener(&work, Some(18797)).unwrap();
        let second = export_skill_for_listener(&work, Some(18798)).unwrap();
        let unbound = export_skill_for_listener(&work, None).unwrap();
        assert!(
            first
                .parent()
                .unwrap()
                .join("references/tools-and-errors.md")
                .is_file()
        );
        assert!(
            first
                .parent()
                .unwrap()
                .join("references/recipes.md")
                .is_file()
        );
        let files = [first, second, unbound].map(|path| std::fs::read_to_string(path).unwrap());
        std::fs::remove_dir_all(&work).unwrap();
        assert!(files[0].contains("127.0.0.1:18797/mcp"));
        assert!(!files[0].contains("127.0.0.1:18798/mcp"));
        assert!(files[1].contains("127.0.0.1:18798/mcp"));
        assert!(!files[2].contains("127.0.0.1:"));
    }

    #[test]
    fn skill_for_endpoint_uses_the_advertised_lan_host() {
        let endpoint: SocketAddr = "192.168.1.175:8797".parse().unwrap();
        let md = skill_for_endpoint(Some(endpoint));
        assert!(
            md.contains("http://192.168.1.175:8797/mcp"),
            "LAN endpoint must be written into SKILL.md: {md}"
        );
        assert!(!md.contains("http://127.0.0.1:8797/mcp"));
    }

    #[test]
    fn export_skill_writes_to_work_dir() {
        let work = std::env::temp_dir().join(format!(
            "duthub-skill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_default(|d| d.as_nanos())
        ));
        let path = export_skill_in(&work).expect("export succeeds");
        assert_eq!(path, work.join("skill").join(SKILL_NAME).join("SKILL.md"));
        let raw = std::fs::read_to_string(&path).expect("SKILL.md exists");
        assert!(raw.contains("name: duthub"));
        assert_eq!(
            raw,
            generate_skill_md(),
            "written content matches generator"
        );
        assert!(
            work.join("skill")
                .join(SKILL_NAME)
                .join("references/tools-and-errors.md")
                .is_file()
        );
        assert!(
            work.join("skill")
                .join(SKILL_NAME)
                .join("references/recipes.md")
                .is_file()
        );
        // Re-export replaces the previous skill cleanly.
        assert_eq!(export_skill_in(&work).expect("overwrite export"), path);
        let _ = std::fs::remove_dir_all(&work);
    }
}
