use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

fn append(path: &Path, text: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text)
}

fn acp(counter: &Path, fail_early: bool) -> io::Result<ExitCode> {
    append(counter, b"spawn\n")?;
    if fail_early {
        return Ok(ExitCode::from(3));
    }

    let mut input = io::stdin().lock();
    let responses = [
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-acp","version":"0.0.1"},"agentCapabilities":{"loadSession":false,"mcpCapabilities":{"http":true}}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fake-session-1","_meta":{"extensionResults":[{"name":"duthub","success":true}]}}}"#,
    ];
    for response in responses {
        let mut request = String::new();
        if input.read_line(&mut request)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ACP request missing",
            ));
        }
        let mut output = io::stdout().lock();
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    thread::sleep(Duration::from_secs(1));
    Ok(ExitCode::SUCCESS)
}

fn run_tree(marker: &Path) -> io::Result<()> {
    let status = Command::new(std::env::current_exe()?)
        .arg("heartbeat")
        .arg(marker)
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "heartbeat child exited with {status}"
        )));
    }
    Ok(())
}

fn heartbeat(marker: &Path) -> io::Result<()> {
    for _ in 0..600 {
        append(marker, b"beat\n")?;
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn marker_path(value: Option<OsString>) -> io::Result<PathBuf> {
    value
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "fixture marker path missing"))
}

fn main() -> io::Result<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("fail-early") => return acp(&marker_path(args.next())?, true),
        Some("crash-after-ready") => return acp(&marker_path(args.next())?, false),
        Some("tree") => run_tree(&marker_path(args.next())?)?,
        Some("heartbeat") => heartbeat(&marker_path(args.next())?)?,
        Some("-p") => {
            let mut output = io::stdout().lock();
            writeln!(
                output,
                "{}",
                r#"{"type":"system","subtype":"init","session_id":"sess-cli-slow"}"#
            )?;
            output.flush()?;
            drop(output);
            run_tree(Path::new("heartbeat.log"))?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown fixture mode",
            ));
        }
    }
    Ok(ExitCode::SUCCESS)
}
