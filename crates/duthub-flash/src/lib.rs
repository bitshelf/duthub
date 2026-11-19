//! duthub-flash：烧录工具的只读测试执行（`--help` / `--version` 白名单）。
//!
//! 规则沿用原 `local_runner.py` 的 SAFE_ARGS 白名单；真实刷写不在此 crate 暴露
//! （M3.4 后续实现带审批 token 的 Job 状态机，H4 永不自动）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use duthub_core::process::{ProcessRunner, RealProcessRunner, resolve_executable};
use duthub_core::{DeviceError, DeviceResult, FlashExecutor, ProcessResult};

const TEST_TIMEOUT: Duration = Duration::from_secs(12);

/// 只读检查参数白名单（docs/SECURITY.md #4：`--help` 也不可尽信，参数必须显式白名单）。
pub const SAFE_ARGS: [&str; 5] = ["--help", "-h", "--version", "-V", "-?"];

pub fn is_safe_args(args: &[String]) -> bool {
    !args.is_empty() && args.iter().all(|a| SAFE_ARGS.contains(&a.as_str()))
}

pub struct FlashTester {
    runner: Arc<dyn ProcessRunner>,
}

impl FlashTester {
    pub fn new(runner: Arc<dyn ProcessRunner>) -> Self {
        Self { runner }
    }

    pub fn real() -> Self {
        Self::new(Arc::new(RealProcessRunner))
    }

    /// Empty preset arguments request a version probe. Never launch a tool implicitly without flags.
    pub fn run_preset_test(
        &self,
        executable: &str,
        test_args: &str,
    ) -> DeviceResult<ProcessResult> {
        let args = if test_args.trim().is_empty() {
            vec!["--version".into()]
        } else {
            test_args.split_whitespace().map(str::to_owned).collect()
        };
        self.run_test(executable, &args)
    }

    /// 运行只读测试（仅白名单参数）。返回真实 ProcessResult 供 UI 展示。
    pub fn run_test(&self, executable_spec: &str, args: &[String]) -> DeviceResult<ProcessResult> {
        if !is_safe_args(args) {
            return Err(DeviceError::Invalid(format!(
                "测试参数仅允许 {:?}（烧录类参数需要独立审批流程）",
                SAFE_ARGS
            )));
        }
        let exe = resolve_executable(executable_spec)?;
        self.runner.run(&exe, args, TEST_TIMEOUT)
    }
}

#[async_trait]
impl FlashExecutor for FlashTester {
    async fn test(
        &self,
        executable: &std::path::Path,
        args: &[String],
        timeout: Duration,
    ) -> DeviceResult<ProcessResult> {
        if !is_safe_args(args) {
            return Err(DeviceError::Invalid("非白名单参数".into()));
        }
        self.runner.run(executable, args, timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl ProcessRunner for MockRunner {
        fn run(
            &self,
            exe: &Path,
            args: &[String],
            _timeout: Duration,
        ) -> DeviceResult<ProcessResult> {
            self.calls
                .lock()
                .unwrap()
                .push((exe.to_string_lossy().to_string(), args.to_vec()));
            Ok(ProcessResult {
                exit_code: Some(0),
                stdout: "upgrade_tool 2.4\n".into(),
                stderr: String::new(),
                elapsed_ms: 30,
            })
        }
    }

    #[test]
    fn whitelist_accepts_only_readonly_flags() {
        let ok: Vec<Vec<String>> = vec![
            vec!["--help".into()],
            vec!["-h".into()],
            vec!["--version".into()],
            vec!["-V".into()],
            vec!["-?".into()],
        ];
        for args in &ok {
            assert!(is_safe_args(args), "{args:?} 应放行");
        }
        let bad: Vec<Vec<String>> = vec![
            vec![],
            vec!["--list".into()],
            vec!["--help".into(), "RK3576".into()],
            vec!["-w".into(), "image.img".into()],
        ];
        for args in &bad {
            assert!(!is_safe_args(args), "{args:?} 必须拒绝");
        }
    }

    #[test]
    fn tester_rejects_nonwhitelist_before_spawn() {
        let mock = Arc::new(MockRunner {
            calls: Mutex::new(Vec::new()),
        });
        let tester = FlashTester::new(mock.clone());
        let err = tester
            .run_test("upgrade_tool", &["--list".to_string()])
            .unwrap_err();
        assert!(matches!(err, DeviceError::Invalid(_)));
        assert!(mock.calls.lock().unwrap().is_empty(), "拒绝时不得启动进程");
    }

    #[test]
    fn tester_runs_whitelisted() {
        let mock = Arc::new(MockRunner {
            calls: Mutex::new(Vec::new()),
        });
        let tester = FlashTester::new(mock.clone());
        let result = tester
            .run_test("upgrade_tool", &["--help".to_string()])
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("upgrade_tool"));
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls[0].1, vec!["--help".to_string()]);
    }

    #[test]
    fn tester_reports_missing_executable() {
        let mock = Arc::new(MockRunner {
            calls: Mutex::new(Vec::new()),
        });
        let tester = FlashTester::new(mock);
        let err = tester
            .run_test(
                "definitely-not-a-real-flash-tool-xyz",
                &["--help".to_string()],
            )
            .unwrap_err();
        assert!(matches!(err, DeviceError::NotFound));
    }

    #[test]
    fn preset_test_uses_a_readonly_default_and_preserves_configured_flags() {
        let runner = Arc::new(MockRunner::default());
        let tester = FlashTester::new(runner.clone());
        let exe = std::env::current_exe().unwrap();
        let exe = exe.to_str().unwrap();

        tester.run_preset_test(exe, "  ").unwrap();
        tester.run_preset_test(exe, "-h").unwrap();
        assert!(tester.run_preset_test(exe, "-h -w image.img").is_err());

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, ["--version"]);
        assert_eq!(calls[1].1, ["-h"]);
    }
}
