use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn executable() -> &'static Path {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let source = PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"),
        )
        .join("tests/support/agent_process.rs");
        let output_path = std::env::current_exe()
            .expect("the test executable path must be available")
            .with_extension(format!("{}.fixture.exe", std::process::id()));
        // Cargo places the test executable under its target directory.
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .args(["--edition=2024", "-D", "warnings"])
            .arg(source)
            .arg("-o")
            .arg(&output_path)
            .output()
            .expect("failed to invoke rustc for the Agent test fixture");
        assert!(
            output.status.success(),
            "Agent fixture compilation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output_path
    })
}
