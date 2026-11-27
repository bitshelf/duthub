//! Long-lived containerized processes (ACP / streaming CLI lifecycle) backed by
//! `mxc-sdk`.
//!
//! Verifies bidirectional stdio streaming, write isolation, read isolation, and
//! deterministic whole-tree termination on `terminate()` and `Drop`.

#![cfg(windows)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use duthub_sandbox::{CellSpec, SandboxPolicy, SpawnRequest, prepare_cell, spawn_streaming};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "duthub-sandbox-stream-{tag}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cmd_exe() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("cmd.exe")
}

fn agent_environment(cell: &CellSpec, extra: &[(&str, OsString)]) -> Vec<(OsString, OsString)> {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let layout = cell.layout();
    let mut env = vec![
        (OsString::from("SystemRoot"), system_root.clone()),
        (OsString::from("windir"), system_root.clone()),
        (
            OsString::from("PATH"),
            OsString::from(format!(
                "{}\\System32;{}",
                system_root.to_string_lossy(),
                system_root.to_string_lossy()
            )),
        ),
        (OsString::from("COMSPEC"), cmd_exe().into_os_string()),
        (
            OsString::from("PATHEXT"),
            OsString::from(".COM;.EXE;.BAT;.CMD"),
        ),
        (OsString::from("TEMP"), layout.temp().into_os_string()),
        (OsString::from("TMP"), layout.temp().into_os_string()),
        (
            OsString::from("USERPROFILE"),
            layout.home().into_os_string(),
        ),
        (
            OsString::from("LOCALAPPDATA"),
            layout.home().join("AppData").join("Local").into_os_string(),
        ),
        (
            OsString::from("APPDATA"),
            layout
                .home()
                .join("AppData")
                .join("Roaming")
                .into_os_string(),
        ),
    ];
    for (k, v) in extra {
        env.push((OsString::from(k), v.clone()));
    }
    env
}

fn line_channel(stdout: Box<dyn Read + Send>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    receiver
}

#[test]
fn streaming_child_round_trips_and_enforces_write_and_read_boundaries() {
    let work = temp_dir("round-trip");
    let cell = CellSpec::for_dut(&work, "dut-stream").expect("cell");
    let sibling = CellSpec::for_dut(&work, "dut-sibling").expect("sibling");
    prepare_cell(&cell).expect("prepare cell");
    prepare_cell(&sibling).expect("prepare sibling");
    let sibling_secret = sibling.root().join("secret.txt");
    std::fs::write(&sibling_secret, "STREAM-SIBLING-SECRET").unwrap();

    let policy = SandboxPolicy::agent_cell(&cell, None);
    let user_profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
    let marker = PathBuf::from(&user_profile)
        .join(format!("duthub-stream-escape-{}.txt", uuid::Uuid::new_v4()));

    let script = cell.root().join("stream_check.cmd");
    std::fs::write(
        &script,
        "@echo off\r\n\
         (echo escaped>\"%ESCAPE_TARGET%\") 2>nul && (echo WROTE) || (echo WRITE_DENIED)\r\n\
         (type \"%SIBLING_SECRET%\" >nul 2>nul) && (echo READ_LEAKED) || (echo READ_DENIED)\r\n\
         :loop\r\n\
         set \"line=\"\r\n\
         set /p \"line=\" || goto :eof\r\n\
         echo echo:%line%\r\n\
         goto :loop\r\n",
    )
    .expect("write stream check script");

    let request = SpawnRequest {
        program: cmd_exe(),
        args: vec![
            OsString::from("/D"),
            OsString::from("/Q"),
            OsString::from("/C"),
            OsString::from("stream_check.cmd"),
        ],
        cwd: cell.root().to_path_buf(),
        environment: agent_environment(
            &cell,
            &[
                ("ESCAPE_TARGET", marker.as_os_str().to_os_string()),
                ("SIBLING_SECRET", sibling_secret.as_os_str().to_os_string()),
            ],
        ),
    };
    let mut process = spawn_streaming(&cell, &policy, &request).expect("spawn_streaming");
    let mut stdin = process.take_stdin().expect("stdin");
    let lines = line_channel(process.take_stdout().expect("stdout"));

    let first = lines
        .recv_timeout(Duration::from_secs(15))
        .expect("child did not emit write verdict");
    assert_eq!(first.trim(), "WRITE_DENIED", "got {first:?}");
    assert!(
        !marker.exists(),
        "escaped write created {}",
        marker.display()
    );

    let second = lines
        .recv_timeout(Duration::from_secs(15))
        .expect("child did not emit read verdict");
    assert_eq!(second.trim(), "READ_DENIED", "got {second:?}");

    stdin.write_all(b"ping\r\n").expect("write stdin");
    stdin.flush().expect("flush stdin");
    let echo = lines
        .recv_timeout(Duration::from_secs(15))
        .expect("streaming child did not echo stdin");
    assert_eq!(echo.trim(), "echo:ping");

    // Closing stdin exits the loop cleanly.
    drop(stdin);
    let code = process
        .wait_timeout(Duration::from_secs(10))
        .expect("wait_timeout")
        .expect("child did not exit after stdin EOF");
    assert_eq!(code, 0);

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn drop_terminates_the_whole_tree() {
    let work = temp_dir("tree");
    let cell = CellSpec::for_dut(&work, "dut-tree").expect("cell");
    prepare_cell(&cell).expect("prepare cell");
    let policy = SandboxPolicy::agent_cell(&cell, None);
    let marker = cell.root().join("grandchild-marker.txt");

    let grandchild_script = cell.root().join("grandchild_loop.cmd");
    std::fs::write(
        &grandchild_script,
        "@echo off\r\n\
         :loop\r\n\
         echo tick>>grandchild-marker.txt\r\n\
         for /L %%i in (1,1,4000) do @rem\r\n\
         goto loop\r\n",
    )
    .expect("write grandchild script");

    let request = SpawnRequest {
        program: cmd_exe(),
        args: vec![
            OsString::from("/D"),
            OsString::from("/Q"),
            OsString::from("/C"),
            OsString::from("cmd.exe /D /Q /C grandchild_loop.cmd"),
        ],
        cwd: cell.root().to_path_buf(),
        environment: agent_environment(&cell, &[]),
    };
    let process = spawn_streaming(&cell, &policy, &request).expect("spawn_streaming");

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0) > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let before_drop = std::fs::metadata(&marker)
        .expect("grandchild never wrote marker file")
        .len();
    assert!(before_drop > 0, "grandchild never wrote marker file");

    // Dropping the handle terminates the whole MXC Job tree.
    drop(process);
    std::thread::sleep(Duration::from_millis(300));
    let after_drop = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    std::thread::sleep(Duration::from_millis(700));
    let later = std::fs::metadata(&marker).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        after_drop, later,
        "grandchild kept writing after SandboxedProcess::Drop ({after_drop} -> {later})"
    );

    let _ = std::fs::remove_dir_all(&work);
}
