//! One-time provisioning of the dedicated sandbox identity (ADR-12).
//!
//! Run it from an **elevated** shell:
//!
//! ```text
//! cargo run -p duthub-sandbox --example provision-sandbox
//! ```
//!
//! With `--check` it only prints what the current machine can enforce and exits
//! (no elevation needed). `--root <dir>` stores the credential inside that
//! directory (default `%LOCALAPPDATA%\duthub\sandbox`; the host can also call
//! `duthub_sandbox::set_identity_root`). Without `--check` it creates the local group
//! `DuthubSandboxUsers`, the account `DuthubSandbox` with a random password,
//! stores that password DPAPI-sealed under `%LOCALAPPDATA%\duthub\sandbox`, and
//! verifies the account logs on with a non-administrator token.
//!
//! The example never escalates itself: elevation is the operator's explicit
//! action, so nothing pops a UAC prompt in the background.

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let check_only = arguments.iter().any(|arg| arg == "--check");
    let root = arguments
        .iter()
        .position(|arg| arg == "--root")
        .and_then(|index| arguments.get(index + 1))
        .map(std::path::PathBuf::from);
    if let Some(root) = &root
        && let Err(error) = duthub_sandbox::set_identity_root(Some(root.clone()))
    {
        eprintln!("--root 无效: {error}");
        std::process::exit(1);
    }
    match duthub_sandbox::identity_root() {
        Ok(dir) => println!("凭据目录: {}", dir.display()),
        Err(error) => println!("凭据目录不可用: {error}"),
    }
    let status = duthub_sandbox::status();
    println!("平台: {}", status.platform);
    println!("能力: {}", status.detail);

    if check_only {
        println!(
            "专用沙箱身份: {}（原因见上一行的能力描述）",
            status.identity.as_str()
        );
        return;
    }

    if !duthub_sandbox::is_elevated() {
        eprintln!(
            "需要管理员权限：请在“以管理员身份运行”的终端里重跑本命令。\n\
             当前进程不是提升后的上下文，未做任何改动。\n\
             提示：只读检查用 `cargo run -p duthub-sandbox --example provision-sandbox -- --check`。"
        );
        std::process::exit(1);
    }

    let provisioned = match &root {
        Some(root) => duthub_sandbox::provision_identity_in(root),
        None => duthub_sandbox::provision_identity(),
    };
    match provisioned {
        Ok(report) => {
            println!("账户: {}", report.username);
            println!("组: {}", report.group);
            println!("账户 SID: {}", report.account_sid);
            println!("新建账户: {}", report.created_user);
            println!("新建组: {}", report.created_group);
            println!("轮换口令: {}", report.rotated_password);
            println!("凭据文件: {}", report.credential_file.display());
            println!(
                "初始化完成：脚本与 Agent 现在以专用账户运行，受保护路径对该 SID 写 deny-read ACE。"
            );
        }
        Err(error) => {
            eprintln!("初始化失败: {error}");
            std::process::exit(2);
        }
    }
}
