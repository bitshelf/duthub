//! 心跳检测共享类型（需求⑦）：配置 / 状态 / 系统信息启发式提取。
//!
//! - 配置挂在 [`crate::model::DutConfig`]（duts.json `heartbeat` 段，缺段 = 关闭）。
//! - 状态由运行时（desktop 心跳引擎）维护：`last_ok=None` 表示尚未探测，
//!   不与「探测失败」混淆（避免首次渲染就红徽标）。
//! - [`extract_sysinfo`] 是纯函数：从响应原文提取 uptime/mem/cpu/hostname，
//!   提不出就留空（不伪造）。

use serde::{Deserialize, Serialize};

pub const DEFAULT_INTERVAL_SECS: u32 = 30;
pub const DEFAULT_LOSS_THRESHOLD: u32 = 3;
pub const DEFAULT_COMMAND: &str = "uname -a";

/// 心跳配置（duts.json `heartbeat` 段；Agent 可经 `heartbeat.set` 改写命令与间隔）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatConfig {
    pub enabled: bool,
    pub interval_secs: u32,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_until_unix_ms: Option<u64>,
    /// 连续多少次无响应判定丢联（发 `AlertEvent`）。
    pub loss_threshold: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: DEFAULT_INTERVAL_SECS,
            command: DEFAULT_COMMAND.into(),
            expected_response: None,
            paused_until_unix_ms: None,
            loss_threshold: DEFAULT_LOSS_THRESHOLD,
        }
    }
}

impl HeartbeatConfig {
    /// 与默认完全一致时序列化省略整段（保持旧 duts.json 形状）。
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 间隔下限保护（1 秒）：0 会让 tick 每帧狂发命令。
    #[must_use]
    pub fn normalized_interval(&self) -> u32 {
        self.interval_secs.clamp(1, 86_400)
    }

    #[must_use]
    pub fn is_paused_at(&self, now_ms: u64) -> bool {
        self.paused_until_unix_ms
            .is_some_and(|until| now_ms < until)
    }
}

/// 系统信息（启发式提取结果；提不出的字段为 None，不伪造）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SysInfo {
    pub uptime: Option<String>,
    pub mem: Option<String>,
    pub cpu: Option<String>,
    pub hostname: Option<String>,
}

/// 心跳状态（需求⑦字段命名）：`last_ok=None` 尚未探测。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeartbeatState {
    pub last_ok: Option<bool>,
    pub last_at_unix_ms: u64,
    pub loss_count: u32,
    pub last_response: String,
    pub sysinfo: SysInfo,
    /// 探测因 Busy/Lease 被暂缓（P2-036：Agent 持租约期活性盲区标记；
    /// 成功探测或收到其他错误时清除）。false 为默认，序列化时省略。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub paused_by_lease: bool,
}

/// 丢联告警事件（供弹窗阶段消费；当前先入队列由 UI drain 成 toast）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatAlert {
    pub dut_id: String,
    pub loss_count: u32,
    pub message: String,
    pub at_unix_ms: u64,
}

/// 判定一行是否为「设备响应」（排除本机回显/驱动信令/分段标记）。
#[must_use]
pub fn is_response_line(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with("[TX] ")
        && !text.starts_with("[serial] ")
        && !text.starts_with("[mark] ")
}

/// 常见架构 token（uname -m / uname -a 输出）。
const ARCH_TOKENS: [&str; 9] = [
    "aarch64",
    "armv7l",
    "armv8l",
    "x86_64",
    "i686",
    "riscv64",
    "loongarch64",
    "ppc64le",
    "s390x",
];

/// 从响应原文提取 uptime/mem/cpu/hostname（启发式，提不出留 None）。
pub fn extract_sysinfo(text: &str) -> SysInfo {
    let mut info = SysInfo::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // uptime：` 14:32:01 up 4 days,  2:03,  2 users, load average: …`
        if info.uptime.is_none() && line.contains(" up ") {
            let rest = line.rsplit(" up ").next().unwrap_or("").trim();
            // 截掉 load average 段，再去掉结尾的 users 计数段（`, 2 users` / `, 1 user`）
            let mut seg = rest
                .split_once("load average")
                .map_or(rest, |(head, _)| head)
                .trim_end()
                .to_string();
            if let Some(pos) = seg.rfind(" user") {
                let tail = &seg[pos + " user".len()..];
                if tail == "s" || tail == "s," || tail == "," || tail.is_empty() {
                    // 同时剥掉计数数字：`, 2 users` → `,`
                    let head = seg[..pos]
                        .trim_end()
                        .trim_end_matches(|c: char| c.is_ascii_digit())
                        .trim_end_matches(',')
                        .trim_end();
                    seg = head.to_string();
                }
            }
            let seg = collapse_spaces(seg.trim_end_matches([',', ' ']));
            if !seg.is_empty() {
                info.uptime = Some(seg);
            }
        }
        // uptime：/proc/uptime 形态（1–2 个浮点数单独成行）
        if info.uptime.is_none() && looks_like_proc_uptime(line) {
            info.uptime = Some(format!(
                "{} s",
                line.split_whitespace().next().unwrap_or(line)
            ));
        }
        // mem：free 输出 `Mem:` 行 或 /proc/meminfo `MemTotal:` 行
        if info.mem.is_none() {
            if let Some(rest) = line.strip_prefix("Mem:") {
                info.mem = Some(format!("Mem: {}", rest.trim()));
            } else if let Some(rest) = line.strip_prefix("MemTotal:") {
                info.mem = Some(format!("MemTotal: {}", rest.trim()));
            }
        }
        // cpu：uname 架构 token 或 `model name : …`
        if info.cpu.is_none() {
            if let Some(rest) = line.split_once("model name") {
                info.cpu = rest
                    .1
                    .split_once(':')
                    .map(|(_, v)| v.trim().to_string())
                    .filter(|v| !v.is_empty());
            }
            if info.cpu.is_none() {
                let token = line
                    .split([' ', ','])
                    .find(|t| ARCH_TOKENS.contains(&t.trim()));
                if let Some(t) = token {
                    info.cpu = Some(t.trim().to_string());
                }
            }
        }
        // hostname：提示符 `root@rk3576:~#` 或 uname `Linux rk3576 …`
        if info.hostname.is_none() {
            if let Some((user, host_and_path)) = line.split_once('@')
                && let Some((host, _)) = host_and_path.split_once(':')
                && !user.is_empty()
                && user
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
                && !host.is_empty()
                && host
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'_' | b'-' | b'.'))
            {
                info.hostname = Some(host.to_owned());
            }
            if info.hostname.is_none()
                && let Some(rest) = line.strip_prefix("Linux ")
                && !rest.starts_with("version ")
            {
                info.hostname = rest.split_whitespace().next().map(str::to_string);
            }
        }
    }
    info
}

/// 连续空白折叠为单空格（uptime 输出 `4 days,  2:03` → `4 days, 2:03`）。
fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        let space = ch == ' ' || ch == '\t';
        if space && prev_space {
            continue;
        }
        out.push(if space { ' ' } else { ch });
        prev_space = space;
    }
    out.trim().to_string()
}

/// `/proc/uptime` 形态：1–2 个浮点数构成的独立行。
fn looks_like_proc_uptime(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let first_ok = tokens
        .next()
        .is_some_and(|t| t.parse::<f64>().is_ok_and(|v| v >= 0.0));
    let second_ok = match tokens.next() {
        Some(t) => t.parse::<f64>().is_ok(),
        None => true,
    };
    first_ok && second_ok && tokens.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_line_filter_excludes_local_marks() {
        assert!(is_response_line("Linux rk3576 6.1.118 #1 SMP"));
        assert!(is_response_line("root@rk3576:~# "));
        assert!(!is_response_line(""));
        assert!(!is_response_line("[TX] uname -a"));
        assert!(!is_response_line("[serial] COM25 @ 9600 已打开"));
        assert!(!is_response_line("[mark] 继电器 power ON"));
    }

    #[test]
    fn extract_uname_output() {
        let info = extract_sysinfo("Linux rk3576 6.1.118 #1 SMP PREEMPT aarch64 GNU/Linux");
        assert_eq!(info.hostname.as_deref(), Some("rk3576"));
        assert_eq!(info.cpu.as_deref(), Some("aarch64"));
        assert_eq!(info.uptime, None);
        assert_eq!(info.mem, None);
    }

    #[test]
    fn kernel_build_host_is_not_the_dut_hostname() {
        let banner = "Linux version 6.1.118 (builder@lab) #1 SMP";
        assert_eq!(extract_sysinfo(banner).hostname, None);
        let text = format!("[  0.000000] {banner}\nLinux rk3576 6.1.118 aarch64 GNU/Linux");
        assert_eq!(extract_sysinfo(&text).hostname.as_deref(), Some("rk3576"));
    }

    #[test]
    fn extract_uptime_and_prompt_hostname() {
        let text = " 14:32:01 up 4 days,  2:03,  2 users,  load average: 0.05, 0.07, 0.06\nroot@rk3576:~# ";
        let info = extract_sysinfo(text);
        assert_eq!(info.uptime.as_deref(), Some("4 days, 2:03"));
        assert_eq!(info.hostname.as_deref(), Some("rk3576"));
    }

    #[test]
    fn extract_free_mem_and_meminfo() {
        let info = extract_sysinfo(
            "Mem:          1234       567        800         12        100        600",
        );
        assert!(info.mem.as_deref().unwrap_or_default().starts_with("Mem:"));
        let info2 = extract_sysinfo("MemTotal:        16384256 kB");
        assert_eq!(info2.mem.as_deref(), Some("MemTotal: 16384256 kB"));
    }

    #[test]
    fn extract_proc_uptime_line() {
        let info = extract_sysinfo("381948.23 1514820.91");
        assert_eq!(info.uptime.as_deref(), Some("381948.23 s"));
        // 负数/非数字/多列不算
        assert_eq!(extract_sysinfo("-1 2").uptime, None);
        assert_eq!(extract_sysinfo("abc 2").uptime, None);
        assert_eq!(extract_sysinfo("1 2 3").uptime, None);
    }

    #[test]
    fn extract_model_name_cpu() {
        let info = extract_sysinfo("model name\t: Intel(R) Core(TM) i7-10700 CPU @ 2.90GHz");
        assert!(info.cpu.unwrap_or_default().contains("Intel"));
    }

    #[test]
    fn heartbeat_config_default_and_roundtrip() {
        let cfg = HeartbeatConfig::default();
        assert_eq!(cfg.interval_secs, 30);
        assert_eq!(cfg.loss_threshold, 3);
        assert_eq!(cfg.command, "uname -a");
        assert!(cfg.is_default());
        let json = serde_json::json!({"enabled": true, "intervalSecs": 5, "command": "cat /proc/uptime", "lossThreshold": 2});
        let back: HeartbeatConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back.interval_secs, 5);
        assert_eq!(back.loss_threshold, 2);
        assert!(!back.is_default());
        // 旧文件缺字段 → 默认补齐
        let legacy: HeartbeatConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(legacy, HeartbeatConfig::default());
        assert_eq!(legacy.normalized_interval(), 30);
    }
}
