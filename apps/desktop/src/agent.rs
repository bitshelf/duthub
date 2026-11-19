//! Agent 面板 —— kres 式工作区，1:1 移植原型 `#page-agent`（布局/文案/交互模板）。
//!
//! 原型即前端模拟（source=demo）：本模块同样只做 UI 状态机演示，
//! 真实 Agent 运行时在 M5 接入（进度/事件模板与原型一致）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui_kit::component::Disableable;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::{BtnVariant, btn, field, subtle, typed_char};
use crate::icons::icon;
use crate::theme::Palette;
use crate::workspace::Workbench;

/// 模型探针结果槽：(model_id, Ok((latency, reply)) | Err(message))
pub type ModelTestOutcome = (String, Result<(u64, String), String>);

pub static MODEL_TEST_RESULT: std::sync::Mutex<Option<ModelTestOutcome>> =
    std::sync::Mutex::new(None);

const STEP_INTERVAL: Duration = Duration::from_millis(900);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workflow {
    Review,
    Validate,
    Triage,
    Fix,
    Dut,
}

impl Workflow {
    pub fn zh(self) -> &'static str {
        match self {
            Workflow::Review => crate::i18n::label("codeReview"),
            Workflow::Validate => crate::i18n::label("findingValidation"),
            Workflow::Triage => crate::i18n::label("findingTriage"),
            Workflow::Fix => crate::i18n::label("findingFix"),
            Workflow::Dut => crate::i18n::label("dutDiagnosis"),
        }
    }

    pub fn select_label(self) -> &'static str {
        match self {
            Workflow::Review => crate::i18n::label("reviewWorkflow"),
            Workflow::Validate => crate::i18n::label("validateWorkflow"),
            Workflow::Triage => crate::i18n::label("triageWorkflow"),
            Workflow::Fix => crate::i18n::label("fixWorkflow"),
            Workflow::Dut => crate::i18n::label("dutWorkflow"),
        }
    }

    pub fn default_target(self) -> &'static str {
        match self {
            Workflow::Review => "drivers/mmc/core/",
            _ => "findings/mmc_timeout",
        }
    }

    pub fn prompt(self) -> &'static str {
        match self {
            Workflow::Review => {
                "检查 MMC 驱动中的超时处理、内存生命周期和并发问题；给出源码证据和待验证假设。"
            }
            Workflow::Validate => {
                "尝试证伪这条缺陷：核对每个前提是否成立，验证所有前提是否可同时满足。"
            }
            Workflow::Triage => "确认缺陷的状态与严重等级，附带证据、复现条件与影响范围。",
            Workflow::Fix => {
                "先验证缺陷，再规划并生成补丁。构建、提交以及任何可能影响硬件的操作必须获得批准。"
            }
            Workflow::Dut => {
                "收集当前 DUT 的串口内核日志，关联源码，分析异常根因并形成待验证的修复假设。不要自动重启或断电。"
            }
        }
    }

    pub fn steps(self) -> [&'static str; 5] {
        match self {
            Workflow::Review => [
                crate::i18n::label("createPlan"),
                crate::i18n::label("fastSearch"),
                crate::i18n::label("slowReview"),
                crate::i18n::label("rankTasks"),
                crate::i18n::label("consolidateFindings"),
            ],
            Workflow::Validate => [
                crate::i18n::label("readFinding"),
                crate::i18n::label("verifyClaims"),
                crate::i18n::label("testReachability"),
                crate::i18n::label("lookForCounterEvidence"),
                crate::i18n::label("generateVerdict"),
            ],
            Workflow::Triage => [
                crate::i18n::label("readIssue"),
                crate::i18n::label("collectEvidence"),
                crate::i18n::label("determineStatus"),
                crate::i18n::label("rankRisk"),
                crate::i18n::label("summarize"),
            ],
            Workflow::Fix => [
                crate::i18n::label("investigateIssue"),
                crate::i18n::label("planPatch"),
                crate::i18n::label("generateChanges"),
                crate::i18n::label("buildAndReview"),
                crate::i18n::label("pendingRelease"),
            ],
            Workflow::Dut => [
                crate::i18n::label("readDutMapping"),
                crate::i18n::label("collectSerialEvidence"),
                crate::i18n::label("fastCodeSearch"),
                crate::i18n::label("slowDiagnosis"),
                crate::i18n::label("createReport"),
            ],
        }
    }

    pub fn all() -> [Workflow; 5] {
        [
            Workflow::Review,
            Workflow::Validate,
            Workflow::Triage,
            Workflow::Fix,
            Workflow::Dut,
        ]
    }

    /// 事件描述（原型 kEventData hints，按步骤给出）。
    pub fn event_desc(self, step: usize, target: &str, dut_label: &str) -> String {
        match self {
            Workflow::Review => match step {
                1 => format!("目标 {target} · turns=10"),
                2 => format!("grep / read / git · {target}\nsemcode (MCP): symbols queued"),
                3 => "memory · concurrency · bounds · general\nslow=opus".into(),
                4 => "followups 4 · duplicate 1 · prioritized 3".into(),
                _ => "findings.json · report.md · session.json".into(),
            },
            Workflow::Validate | Workflow::Triage => match step {
                1 => format!("Finding: {target}"),
                2 => match self {
                    Workflow::Validate => "read / grep · checks 3".into(),
                    _ => "read / git history".into(),
                },
                3 => match self {
                    Workflow::Validate => "路径前提一致性核对：需更多证据".into(),
                    _ => "目标状态：needs verification".into(),
                },
                4 => match self {
                    Workflow::Validate => "main + secondary: refuter checks".into(),
                    _ => "严重性：待确认".into(),
                },
                _ => match self {
                    Workflow::Validate => "判定：未证实 · 应保留证据".into(),
                    _ => "summary.md · metadata.yaml".into(),
                },
            },
            Workflow::Fix => match step {
                1 => format!("Finding: {target}"),
                2 => "调查完成前不得生成补丁".into(),
                3 => "patch proposal staged — no disk changes".into(),
                4 => "make / review: confirmation required".into(),
                _ => "publish blocked pending operator approval".into(),
            },
            Workflow::Dut => match step {
                1 => format!("DUT {dut_label}"),
                2 => "serial.read via mcp\nread-only · captured sample log".into(),
                3 => "grep / read / git in kernel tree".into(),
                4 => "候选方向：超时 / 并发 / MMC 控制器状态".into(),
                _ => "diagnosis.md · evidence + next steps".into(),
            },
        }
    }

    /// 完成后 findings（原型在 dut/review 流程给出结论卡）。
    pub fn findings(self) -> Vec<(String, String)> {
        match self {
            Workflow::Review => vec![(
                "生命期风险".into(),
                "drivers/mmc/core/ 下两处引用计数释放顺序待验证（slow 审查阶段标记）。".into(),
            )],
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Idle,
    Running,
    Paused,
    Done,
    Stopped,
}

impl RunStatus {
    pub fn label(self) -> &'static str {
        match self {
            RunStatus::Idle => crate::i18n::label("agentIdle"),
            RunStatus::Running => crate::i18n::label("running"),
            RunStatus::Paused => crate::i18n::label("agentPaused"),
            RunStatus::Done => crate::i18n::label("agentDone"),
            RunStatus::Stopped => crate::i18n::label("agentStopped"),
        }
    }
}

pub struct AgentEvent {
    pub step_title: String,
    pub description: String,
}

pub(crate) fn serial_crash_evidence(log: &str) -> Option<&str> {
    log.lines().rev().map(str::trim).find(|line| {
        let message = line
            .strip_prefix('[')
            .and_then(|timestamp| timestamp.split_once(']'))
            .filter(|(timestamp, _)| timestamp.trim().parse::<f64>().is_ok())
            .map_or(*line, |(_, message)| message.trim_start());
        [
            "Kernel panic - not syncing:",
            "Oops:",
            "Internal error: Oops:",
            "BUG: unable to handle kernel",
            "watchdog: BUG: soft lockup",
            "watchdog: BUG: hard LOCKUP",
        ]
        .iter()
        .any(|signature| message.starts_with(signature))
    })
}

#[cfg(test)]
mod crash_tests {
    use super::serial_crash_evidence;

    #[test]
    fn uses_the_device_log_line_as_evidence() {
        let panic = "[ 23.50] Kernel panic - not syncing: Fatal exception";
        assert_eq!(
            serial_crash_evidence(&format!("booted\r\n{panic}\r\n")),
            Some(panic)
        );
        assert_eq!(
            serial_crash_evidence("# grep 'Kernel panic - not syncing:' /var/log/kern.log"),
            None
        );
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{AgentState, RunStatus, Workflow};

    #[test]
    fn binding_switches_drafts_and_reuses_one_agent_per_dut() {
        let mut agents = AgentState::new();
        assert!(agents.active().is_none());
        agents.bind(Some(("a", "Board")));
        let a = agents.active_mut().unwrap();
        a.prompt = "only A".into();
        a.workflow = Workflow::Review;
        agents.bind(Some(("b", "Board")));
        let b = agents.active_mut().unwrap();
        assert_eq!(b.workflow, Workflow::Dut);
        assert!(b.prompt.is_empty());
        b.prompt = "only B".into();
        agents.bind(Some(("a", "Renamed board")));
        let a = agents.active().unwrap();
        assert_eq!(a.prompt, "only A");
        assert_eq!(a.workflow, Workflow::Review);
        assert_eq!(a.dut_label, "Renamed board");
        assert_eq!(agents.sessions.len(), 2);
        agents.bind(None);
        assert!(agents.active().is_none());
    }

    #[test]
    fn background_completion_and_serial_close_use_the_original_dut() {
        let mut agents = AgentState::new();
        agents.bind(Some(("a", "A")));
        agents.start();
        agents.active_mut().unwrap().step = 4;
        agents.bind(Some(("b", "B")));
        let mut completed = Vec::new();
        assert!(agents.tick(|id, _| completed.push(id.to_owned())));
        assert_eq!(completed, ["a"]);
        assert_eq!(agents.active().unwrap().status, RunStatus::Idle);
        agents.start();
        agents.serial_closed("b");
        assert_eq!(agents.active().unwrap().status, RunStatus::Stopped);
        assert!(!agents.tick(|id, _| panic!("unexpected completion for {id}")));
        agents.remove("b");
        assert!(agents.active().is_none());
        agents.bind(Some(("a", "A")));
        assert_eq!(agents.active().unwrap().status, RunStatus::Done);
    }
}

pub struct AgentSession {
    pub workflow: Workflow,
    pub target: String,
    pub dut_label: String,
    pub prompt: String,
    last_tick: Option<Instant>,
    pub step: usize,
    pub status: RunStatus,
    pub events: Vec<AgentEvent>,
    pub findings: Vec<(String, String)>,
}

#[derive(Default)]
pub struct AgentState {
    sessions: HashMap<String, AgentSession>,
    active_dut: Option<String>,
    pub show_inspector: bool,
    pub prompt_focus: Option<gpui_kit::FocusHandle>,
    /// 模型配置（provider/base/model/keyId）；key 存 OS 密钥库
    pub model_provider: String,
    pub model_base: String,
    pub model_name: String,
    /// 真实模型探针进行中
    pub model_testing: bool,
    /// 最近一次探针结果（延迟/回复/错误）
    pub model_status: Option<String>,
}

impl AgentState {
    pub fn new() -> Self {
        Self {
            model_provider: "openai".into(),
            ..Self::default()
        }
    }

    pub fn bind(&mut self, serial: Option<(&str, &str)>) {
        let Some((id, label)) = serial else {
            self.active_dut = None;
            return;
        };
        let run = self
            .sessions
            .entry(id.to_owned())
            .or_insert_with(|| AgentSession {
                workflow: Workflow::Dut,
                target: String::new(),
                dut_label: String::new(),
                prompt: String::new(),
                step: 0,
                status: RunStatus::Idle,
                events: Vec::new(),
                findings: Vec::new(),
                last_tick: None,
            });
        if run.dut_label != label {
            label.clone_into(&mut run.dut_label);
        }
        if run.workflow == Workflow::Dut && run.target != label {
            label.clone_into(&mut run.target);
        }
        if self.active_dut.as_deref() != Some(id) {
            self.active_dut = Some(id.to_owned());
        }
    }

    pub fn serial_closed(&mut self, id: &str) {
        if let Some(run) = self.sessions.get_mut(id)
            && matches!(run.status, RunStatus::Running | RunStatus::Paused)
        {
            run.status = RunStatus::Stopped;
            run.last_tick = None;
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.sessions.remove(id);
        if self.active_dut.as_deref() == Some(id) {
            self.active_dut = None;
        }
    }

    pub fn active_mut(&mut self) -> Option<&mut AgentSession> {
        self.sessions.get_mut(self.active_dut.as_deref()?)
    }

    pub fn active(&self) -> Option<&AgentSession> {
        self.sessions.get(self.active_dut.as_deref()?)
    }

    pub fn active_dut_id(&self) -> Option<&str> {
        self.active_dut.as_deref()
    }

    pub fn tick(&mut self, mut on_done: impl FnMut(&str, &AgentSession)) -> bool {
        let now = Instant::now();
        let mut changed = false;
        for (id, session) in &mut self.sessions {
            if session.status != RunStatus::Running
                || session
                    .last_tick
                    .is_some_and(|tick| now.duration_since(tick) < STEP_INTERVAL)
            {
                continue;
            }
            session.last_tick = Some(now);
            session.step += 1;
            let workflow = session.workflow;
            session.events.push(AgentEvent {
                step_title: workflow.steps()[session.step.min(4) - 1].to_string(),
                description: workflow.event_desc(session.step, &session.target, &session.dut_label),
            });
            if session.step >= 5 {
                session.status = RunStatus::Done;
                session.findings = workflow.findings();
                on_done(id, session);
            }
            changed = true;
        }
        changed
    }

    pub fn start(&mut self) {
        let Some(session) = self.active_mut() else {
            return;
        };
        if matches!(
            session.status,
            RunStatus::Idle | RunStatus::Done | RunStatus::Stopped
        ) {
            session.step = 0;
            session.events.clear();
            session.findings.clear();
        }
        session.status = RunStatus::Running;
        session.last_tick = None;
    }

    /// 发起模型探针（后台线程）；结果由 Workbench 轮询 channel 写回。
    pub fn start_model_test(&mut self, api_key: String) -> bool {
        if self.model_testing {
            return false;
        }
        let provider = match duthub_core::Provider::from_setting(&self.model_provider) {
            Some(p) => p,
            None => {
                self.model_status = Some("未知 provider".into());
                return false;
            }
        };
        let model = self.model_name.clone();
        if model.trim().is_empty() {
            self.model_status = Some(crate::i18n::label("enterModelName").into());
            return false;
        }
        self.model_testing = true;
        self.model_status = Some("测试中…".into());
        let req = duthub_core::TestRequest {
            provider,
            base_url: self.model_base.clone(),
            model: model.clone(),
            api_key,
            prompt: String::new(),
        };
        let key_id = model.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let outcome = match rt {
                Ok(rt) => {
                    rt.block_on(async move { duthub_core::test_model_connection(&req).await })
                }
                Err(e) => Err(format!("runtime: {e}")),
            };
            // 结果经全局槽（同工具坞 job_queue 模式）回传由 Workbench drain
            MODEL_TEST_RESULT.lock().unwrap().replace((
                key_id,
                outcome
                    .map(|resp| (resp.latency_ms, resp.reply))
                    .map_err(|e| e.to_string()),
            ));
        });
        true
    }

    pub fn export_report(&self) -> Option<String> {
        let session = self.active()?;
        if session.events.is_empty() {
            return None;
        }
        let wf = session.workflow;
        let mut report = format!(
            "# duthub Agent · {}\n\n- Prototype: OFFLINE DEMO, no actual model calls or device operations\n- DUT: {}\n- Target: {}\n- Status: {:?}\n\n## Task\n{}\n\n## Activity\n",
            wf.zh(),
            session.dut_label,
            session.target,
            session.status,
            wf.prompt(),
        );
        for event in &session.events {
            report.push_str(&format!(
                "\n### {}\n{}\n",
                event.step_title, event.description
            ));
        }
        if !session.findings.is_empty() {
            report.push_str("\n## Findings\n");
            for (title, body) in &session.findings {
                report.push_str(&format!("\n- **{title}**：{body}\n"));
            }
        }
        Some(report)
    }
}

// ---------------------------------------------------------------------------
// 渲染（1:1 布局：k-top 44px / config strip / progress / content / composer）
// ---------------------------------------------------------------------------

/// 右侧 Agent 面板，跟随当前串口。
pub fn render_agent_pane(
    workbench: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let inspector_open = workbench.agent.show_inspector;
    let mut pane = div()
        .relative()
        .w_full()
        .h_full()
        .bg(p.surface)
        .border_l_1()
        .border_color(p.border)
        .flex()
        .flex_col()
        .child(render_top(workbench, p, cx))
        .child(render_config_strip(workbench, p, window, cx))
        .child(render_progress(workbench, p))
        .child(render_content(workbench, p, cx))
        .child(render_composer(workbench, p, window, cx));
    if inspector_open {
        pane = pane.child(render_inspector(workbench, p, cx));
    }
    pane
}

fn render_top(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let status = w
        .agent
        .active()
        .map(|s| s.status)
        .unwrap_or(RunStatus::Idle);
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(7.))
        .h(px(44.))
        .min_h(px(44.))
        .px(px(13.))
        .pl(px(10.))
        .border_b_1()
        .border_color(p.line)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .font_weight(gpui_kit::FontWeight(650.0))
                .text_color(p.text)
                .child(icon("spark").size(px(15.)).text_color(p.accent))
                .child("Agent"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    div()
                        .text_size(px(10.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.muted)
                        .max_w(px(68.))
                        .child(SharedString::from(status.label().to_string())),
                )
                .child(gpui_btn_icon(w, "agent-inspector", "eye", p, cx))
                .child(gpui_btn_icon(w, "agent-close", "panel-right", p, cx)),
        )
}

fn gpui_btn_icon(
    w: &Workbench,
    id: &'static str,
    name: &str,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let _ = w;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(27.))
        .rounded(px(6.))
        .text_color(p.muted)
        .hover(|s| s.bg(p.hover).text_color(p.text))
        .child(icon(name).size(px(14.)).text_color(p.muted))
        .on_click(cx.listener(move |this, _ev, _win, _cx| match id {
            "agent-inspector" => {
                this.agent.show_inspector = !this.agent.show_inspector;
            }
            "agent-close" => {
                this.side_panel.visible = None;
            }
            _ => {}
        }))
}

fn render_config_strip(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    // 原型 .k-config-strip（V16）：grid [1fr 1fr auto]，row2 = k-target 通栏；
    // label(10px muted) 在控件上方；控件 h30 字号 11px。
    let workflow = w
        .agent
        .active()
        .map(|s| s.workflow)
        .unwrap_or(Workflow::Dut);

    let cell_caption = |caption: &'static str| {
        div()
            .text_size(px(10.0))
            .text_color(p.muted)
            .child(SharedString::from(caption))
    };

    // 工作流 cell
    let workflow_options: Vec<(String, usize)> = Workflow::all()
        .iter()
        .map(|wf| (wf.select_label().to_string(), *wf as usize))
        .collect();
    let workflow_cell = div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .min_w_0()
        .child(cell_caption(crate::i18n::label("workflow")))
        .child(w.select_field(
            "agent-workflow",
            workflow.select_label().to_string(),
            workflow_options,
            Some(workflow as usize),
            std::sync::Arc::new(
                move |this: &mut Workbench,
                      value: usize,
                      _win: &mut Window,
                      cx: &mut Context<Workbench>| {
                    if let Some(session) = this.agent.active_mut() {
                        session.workflow = Workflow::all()[value];
                        session.target = session.workflow.default_target().to_string();
                        session.step = 0;
                        session.status = RunStatus::Idle;
                        session.events.clear();
                        session.findings.clear();
                    }
                    cx.notify();
                },
            ),
            p,
            cx,
        ));

    let dut_label = w
        .agent
        .active()
        .map(|session| session.dut_label.clone())
        .unwrap_or_default();
    let dut_cell = div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .min_w_0()
        .child(cell_caption(crate::i18n::label("targetDut")))
        .child(
            div()
                .text_size(px(12.))
                .text_color(p.text)
                .overflow_hidden()
                .child(dut_label),
        );

    // 参数按钮（V16：span 隐藏 → 仅图标，h30）
    let options_btn = div()
        .id("agent-config-toggle")
        .on_click(cx.listener(|this, _ev, _win, cx| {
            this.set_config_page(Some(crate::workspace::ConfigPage::AgentModel));
            cx.notify();
        }))
        .flex()
        .items_center()
        .justify_center()
        .h(px(30.0))
        .w(px(38.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(p.border)
        .bg(p.surface)
        .text_color(p.muted)
        .hover(|s| s.text_color(p.text).bg(p.hover))
        .child(icon("settings").size(px(14.0)).text_color(p.muted));

    // 目标路径 cell（row2 通栏）
    let target_cell = div()
        .flex()
        .flex_col()
        .gap(px(5.0))
        .min_w_0()
        .flex_1()
        .child(cell_caption(crate::i18n::label("targetScope")))
        .child(w.path_input(
            crate::workspace::In::Target,
            "drivers/mmc/core/ · commit · finding dir",
            p,
            window,
            cx,
        ));

    let mut strip = div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(11.0))
        .border_b_1()
        .border_color(p.line);
    strip = strip.child(
        div()
            .flex()
            .items_end()
            .gap(px(8.0))
            .child(workflow_cell.flex_1())
            .child(dut_cell.flex_1())
            .child(options_btn),
    );
    if workflow != Workflow::Dut {
        strip = strip.child(target_cell);
    }
    strip
}

/// Model credentials are edited in the settings overlay and stored in the OS keyring.
pub fn render_model_settings(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    use crate::i18n::label;

    let token = w
        .agent_token_input
        .get_or_insert_with(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(label("agentModelTokenPlaceholder"))
                    .masked(true)
            })
        })
        .clone();
    let provider = duthub_core::Provider::from_setting(&w.agent.model_provider)
        .map(|provider| provider.label())
        .unwrap_or(label("unknown"));
    let model_name = w.agent.model_name.clone();
    let model_input = w.text_field(
        crate::workspace::In::AgentModel,
        &model_name,
        "gpt-4o-mini",
        true,
        p,
        window,
        cx,
    );
    let token_input = Input::new(&token)
        .id("agent-model-token")
        .accessibility_id("agent-model-token")
        .w_full();
    let testing = w.agent.model_testing;
    let token_to_save = token.clone();
    div()
        .max_w(px(700.))
        .w_full()
        .mx_auto()
        .p(px(24.))
        .child(field(
            label("agentModelProvider"),
            subtle(provider, p),
            None,
            p,
        ))
        .child(field(label("agentModelName"), model_input, None, p))
        .child(field(
            label("agentModelToken"),
            token_input,
            Some(label("agentModelTokenStorage")),
            p,
        ))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(
                    btn(
                        "agent-model-save",
                        label("agentModelSave"),
                        &BtnVariant::TinyPrimary,
                        p,
                    )
                    .disabled(testing)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let model = this.agent.model_name.trim().to_owned();
                        if model.is_empty() {
                            this.agent.model_status = Some(label("enterModelName").into());
                            cx.notify();
                            return;
                        }
                        let value = token_to_save.read(cx).value();
                        if !value.trim().is_empty()
                            && let Err(error) = duthub_core::set_key(&model, value.trim())
                        {
                            this.agent.model_status = Some(error.to_string());
                            cx.notify();
                            return;
                        }
                        this.agent.model_name.clone_from(&model);
                        this.settings.agent.slow = model;
                        this.persist_settings();
                        this.agent_token_input = None;
                        this.agent.model_status = Some(label("agentModelSaved").into());
                        cx.notify();
                    })),
                )
                .child(
                    btn(
                        "model-test-btn",
                        label("testConnection"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .disabled(testing)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let draft = token.read(cx).value();
                        let key = if draft.trim().is_empty()
                            && !this.agent.model_name.trim().is_empty()
                        {
                            duthub_core::get_key(this.agent.model_name.trim())
                        } else {
                            Ok(Some(draft.trim().to_owned()))
                        };
                        match key {
                            Ok(key) => {
                                this.agent.start_model_test(key.unwrap_or_default());
                            }
                            Err(error) => this.agent.model_status = Some(error.to_string()),
                        }
                        cx.notify();
                    })),
                ),
        )
        .when_some(w.agent.model_status.clone(), |el, status| {
            el.child(subtle(&status, p).mt(px(10.)))
        })
}

fn render_progress(w: &Workbench, p: &'static Palette) -> impl IntoElement {
    let (step, status) = w
        .agent
        .active()
        .map(|s| (s.step, s.status))
        .unwrap_or((0, RunStatus::Idle));
    let mut track = div().flex().min_w_0().gap(px(5.)).flex_1();
    for i in 1..=5 {
        track = track.child(div().flex_1().h(px(4.)).rounded(px(3.)).bg(if i <= step {
            if status == RunStatus::Stopped {
                p.red
            } else {
                p.accent
            }
        } else if i == step + 1 && status == RunStatus::Running {
            p.blue
        } else {
            p.border
        }));
    }
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(12.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(p.line)
        .text_color(p.faint)
        .child(track)
        .child(
            div()
                .text_size(px(10.))
                .font_family(crate::theme::MONO_FONT)
                .child(SharedString::from(format!("{step} / 5"))),
        )
}

fn render_content(
    w: &Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let Some(session) = w.agent.active() else {
        return div().id("agent-content-empty").flex_1();
    };
    let mut content = div()
        .id("agent-content")
        .flex_1()
        .flex_col()
        .overflow_y_scroll()
        .px(px(14.0))
        .pt(px(14.0))
        .pb(px(12.0))
        .text_size(px(12.0));
    if session.events.is_empty() {
        // 空态 intro（原型 k-intro）
        content = content
            .child(
                div()
                    .mx_auto()
                    .mt(px(45.0))
                    .max_w(px(540.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_color(p.muted)
                    .child(
                        div()
                            .size(px(35.0))
                            .rounded(px(11.0))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.accent_weak)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(p.accent)
                            .child(icon("spark").size(px(17.0)).text_color(p.accent)),
                    )
                    .child(
                        div()
                            .mt(px(10.0))
                            .text_size(px(13.0))
                            .font_weight(gpui_kit::FontWeight(650.0))
                            .text_color(p.text)
                            .child(crate::i18n::label("agentHeading")),
                    )
                    .child(
                        div()
                            .mt(px(9.0))
                            .text_size(px(11.0))
                            .line_height(px(18.7))
                            .child(SharedString::from(crate::i18n::label("agentIntro"))),
                    ),
            )
            .child({
                let mut suggestions = div()
                    .flex()
                    .justify_center()
                    .flex_wrap()
                    .gap(px(5.0))
                    .mt(px(15.0));
                for (label, target) in [
                    (crate::i18n::label("reviewMmc"), "drivers/mmc/core/"),
                    (crate::i18n::label("analyzePanic"), ""),
                    (
                        crate::i18n::label("validateFinding"),
                        "findings/mmc_timeout",
                    ),
                ] {
                    suggestions = suggestions.child(
                        div()
                            .id(SharedString::from(format!("agent-suggest-{label}")))
                            .on_click(cx.listener(move |this, _ev, _win, cx| {
                                if let Some(session) = this.agent.active_mut() {
                                    if !target.is_empty() {
                                        session.target = target.to_string();
                                    }
                                    this.start_agent();
                                }
                                cx.notify();
                            }))
                            .px(px(11.))
                            .py(px(8.))
                            .rounded(px(6.))
                            .bg(p.surface)
                            .border_1()
                            .border_color(p.border)
                            .text_color(p.muted)
                            .text_size(px(11.))
                            .hover(|s| s.text_color(p.text).border_color(p.accent))
                            .child(SharedString::from(label.to_string())),
                    );
                }
                suggestions
            });
    } else {
        // 时间线 + findings
        for (i, event) in session.events.iter().enumerate() {
            content = content.child(
                div()
                    .flex_row()
                    .gap(px(10.))
                    .mb(px(17.))
                    .max_w(px(830.))
                    .child(
                        div()
                            .size(px(22.))
                            .rounded(px(7.))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.surface)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .font_family(crate::theme::MONO_FONT)
                            .text_color(p.accent)
                            .child(SharedString::from(format!("{}", i + 1))),
                    )
                    .child(
                        div()
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .mt(px(2.))
                                    .font_weight(gpui_kit::FontWeight(620.0))
                                    .text_color(p.text)
                                    .child(SharedString::from(event.step_title.clone()))
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .font_family(crate::theme::MONO_FONT)
                                            .text_color(p.faint)
                                            .child(SharedString::from(format!("step {}", i + 1))),
                                    ),
                            )
                            .child(
                                div()
                                    .mt(px(5.))
                                    .text_color(p.muted)
                                    .line_height(px(20.))
                                    .child(SharedString::from(event.description.clone())),
                            ),
                    ),
            );
        }
        if !session.findings.is_empty() {
            content = content
                .child(
                    div()
                        .border_t_1()
                        .border_color(p.line)
                        .mt(px(18.))
                        .pt(px(12.)),
                )
                .children(session.findings.iter().map(|(title, body)| {
                    div()
                        .mt(px(12.))
                        .max_w(px(830.))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(9.))
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .font_weight(gpui_kit::FontWeight(600.0))
                                        .text_color(p.text)
                                        .child(SharedString::from(title.clone())),
                                )
                                .child(
                                    div()
                                        .px(px(6.))
                                        .py(px(3.))
                                        .rounded(px(5.))
                                        .border_1()
                                        .border_color(p.border)
                                        .text_size(px(10.))
                                        .font_family(crate::theme::MONO_FONT)
                                        .text_color(p.muted)
                                        .child("finding"),
                                ),
                        )
                        .child(
                            div()
                                .mt(px(7.))
                                .text_color(p.muted)
                                .line_height(px(20.))
                                .child(SharedString::from(body.clone())),
                        )
                }));
        }
    }
    content
}

fn render_composer(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let status = w
        .agent
        .active()
        .map(|s| s.status)
        .unwrap_or(RunStatus::Idle);
    let running = status == RunStatus::Running;
    let prompt = w
        .agent
        .active()
        .map(|session| session.prompt.clone())
        .unwrap_or_default();
    let focused = w
        .agent
        .prompt_focus
        .as_ref()
        .map(|f| f.is_focused(window))
        .unwrap_or(false);
    let mut prompt_box = div()
        .id("agent-prompt")
        .when_some(w.agent.prompt_focus.clone(), |el, focus| {
            el.track_focus(&focus)
        })
        .min_h(px(66.))
        .max_h(px(130.))
        .w_full()
        .px(px(12.))
        .py(px(10.))
        .rounded(px(7.))
        .bg(p.bg)
        .border_1()
        .border_color(if focused { p.accent } else { p.border })
        .text_size(px(12.))
        .line_height(px(19.))
        .text_color(p.text)
        .on_click(cx.listener(|this, _ev, window, cx| {
            if let Some(focus) = this.agent.prompt_focus.clone() {
                window.focus(&focus, cx);
            }
        }))
        .on_key_down(
            cx.listener(|this, event: &gpui_kit::KeyDownEvent, _win, cx| {
                let k = &event.keystroke;
                if k.key == "enter" && !k.modifiers.shift {
                    // Enter 发送（Shift+Enter 换行）
                    this.agent_start_from_prompt(cx);
                    return;
                }
                let Some(session) = this.agent.active_mut() else {
                    return;
                };
                if k.key == "enter" {
                    session.prompt.push('\n');
                    cx.notify();
                    return;
                }
                if k.key == "backspace" {
                    session.prompt.pop();
                    cx.notify();
                    return;
                }
                if let Some(ch) = typed_char(event).filter(|_| session.prompt.len() < 8192) {
                    session.prompt.push_str(&ch);
                    cx.notify();
                }
            }),
        );
    if prompt.is_empty() {
        prompt_box = prompt_box.child(
            div()
                .text_color(p.faint)
                .child(SharedString::from(crate::i18n::label("agentPlaceholder"))),
        );
    } else {
        for line in prompt.split('\n') {
            prompt_box = prompt_box.child(SharedString::from(line.to_string()));
        }
    }

    // 动作按钮
    let has_events = w
        .agent
        .active()
        .map(|s| !s.events.is_empty())
        .unwrap_or(false);
    let _ = has_events;
    let mut composer = div()
        .flex()
        .flex_col()
        .gap(px(9.))
        .px(px(18.))
        .py(px(11.))
        .border_t_1()
        .border_color(p.border)
        .bg(p.surface)
        .child(prompt_box);
    // codex-compose-toolbar（原型：左 = ＋上下文/模型/串口标签；右 = 输入提示）
    composer = composer.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .id("codex-add-context")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(24.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.raised)
                            .text_color(p.muted)
                            .hover(|s| s.text_color(p.text).bg(p.hover))
                            .child(icon("plus").size(px(12.0)).text_color(p.muted)),
                    )
                    .child(
                        div()
                            .px(px(7.0))
                            .h(px(24.0))
                            .rounded(px(5.0))
                            .border_1()
                            .border_color(p.border)
                            .bg(p.bg)
                            .text_size(px(11.0))
                            .text_color(p.muted)
                            .child(crate::i18n::label("noModel")),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(p.faint)
                            .overflow_hidden()
                            .child(SharedString::from(
                                w.agent
                                    .active()
                                    .map(|s| format!("串口 · {}", s.dut_label))
                                    .unwrap_or_else(|| {
                                        crate::i18n::label("noSerialConnection").into()
                                    }),
                            )),
                    ),
            )
            .child(
                div()
                    .text_size(px(9.0))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.faint)
                    .whitespace_nowrap()
                    .child(crate::i18n::label("agentSendHint")),
            ),
    );
    // k-compose-foot：左 hint，右动作
    composer = composer.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .gap(px(12.0))
                    .text_size(px(10.0))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.faint)
                    .overflow_hidden()
                    .child(crate::i18n::label("agentPlanMode")),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .child(
                        action_button(
                            "agent-export",
                            crate::i18n::label("export"),
                            false,
                            has_events,
                            p,
                        )
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            if let Some(report) = this.agent.export_report() {
                                this.export_agent_report(&report);
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        action_button("agent-stop", crate::i18n::label("stop"), false, running, p)
                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                if let Some(session) = this.agent.active_mut()
                                    && session.status == RunStatus::Running
                                {
                                    session.status = RunStatus::Stopped;
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        action_button(
                            "agent-pause",
                            crate::i18n::label("pause"),
                            false,
                            running,
                            p,
                        )
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            if let Some(session) = this.agent.active_mut() {
                                if session.status == RunStatus::Running {
                                    session.status = RunStatus::Paused;
                                } else if session.status == RunStatus::Paused {
                                    session.status = RunStatus::Running;
                                }
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        action_button("agent-send", crate::i18n::label("send"), true, !running, p)
                            .on_click(cx.listener(|this, _ev, _win, cx| {
                                this.agent_start_from_prompt(cx);
                            })),
                    )
                    .child(
                        action_button(
                            "agent-next",
                            crate::i18n::label("nextStage"),
                            false,
                            !running,
                            p,
                        )
                        .on_click(cx.listener(|this, _ev, _win, cx| {
                            if let Some(session) = this.agent.active_mut() {
                                session.status = RunStatus::Running;
                                session.last_tick = None;
                            }
                            cx.notify();
                        })),
                    ),
            ),
    );
    composer
}

fn action_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    enabled: bool,
    p: &'static Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let mut btn = if primary {
        div().id(id).bg(p.accent).text_color(p.accent_contrast)
    } else {
        div()
            .id(id)
            .bg(p.raised)
            .border_1()
            .border_color(p.border)
            .text_color(p.muted)
    };
    if !enabled {
        btn = btn.opacity(0.45);
    }
    btn.flex()
        .items_center()
        .justify_center()
        .h(px(28.0))
        .px(px(6.0))
        .rounded(px(6.))
        .text_size(px(11.))
        .hover(|s| s.text_color(if primary { p.accent_contrast } else { p.text }))
        .child(SharedString::from(label.to_string()))
}

fn render_inspector(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> impl IntoElement {
    let dut = w.duts.file.duts.get(w.duts.selected).cloned();
    let mut drawer = div()
        .absolute()
        .right_0()
        .top(px(44.))
        .w(px(270.))
        .h_full()
        .bg(p.surface)
        .border_l_1()
        .border_color(p.border)
        .flex()
        .flex_col()
        .occlude();
    drawer = drawer.child(
        div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(46.))
            .px(px(15.))
            .border_b_1()
            .border_color(p.line)
            .text_color(p.muted)
            .child(
                div()
                    .text_size(px(10.))
                    .font_weight(gpui_kit::FontWeight(750.0))
                    .child(crate::i18n::label("contextEvidence")),
            )
            .child(
                div()
                    .id("agent-inspect-close")
                    .on_click(cx.listener(|this, _ev, _win, cx| {
                        this.agent.show_inspector = false;
                        cx.notify();
                    }))
                    .size(px(25.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(16.))
                    .hover(|s| s.bg(p.hover).text_color(p.text))
                    .child("×"),
            ),
    );
    // 设备映射
    let mut rows: Vec<(String, String)> = Vec::new();
    if let Some(dut) = &dut {
        rows.push(("DUT".into(), dut.name.clone()));
        rows.push((
            "Serial".into(),
            format!("{} @ {}", dut.serial.port, dut.serial.baud),
        ));
        rows.push((
            "USB ADB".into(),
            if dut.adb.serial.is_empty() {
                "—".into()
            } else {
                dut.adb.serial.clone()
            },
        ));
        rows.push((
            "Control".into(),
            match &dut.relay {
                duthub_core::model::RelayConfig::Disabled => {
                    crate::i18n::label("notEnabled").into()
                }
                duthub_core::model::RelayConfig::UsbSerial { node, .. } => format!("USB {node}"),
                duthub_core::model::RelayConfig::UsbBoard { board_id, .. } => w
                    .duts
                    .file
                    .relay_boards
                    .iter()
                    .find(|board| board.id == *board_id)
                    .map_or_else(
                        || crate::i18n::label("relayUnavailable").into(),
                        |board| format!("USB {}", board.node),
                    ),
                duthub_core::model::RelayConfig::SnmpPdu { snmp, outlets } => {
                    format!("SNMP {} / outlet {}", snmp.address, outlets.power)
                }
            },
        ));
    }
    let section = |title: &'static str, body: Div| -> Div {
        div()
            .px(px(15.))
            .py(px(18.))
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .text_size(px(10.))
                    .font_weight(gpui_kit::FontWeight(750.0))
                    .text_color(p.muted)
                    .child(title),
            )
            .child(body)
    };
    drawer = drawer.child(section(
        crate::i18n::label("deviceMapping"),
        div().children(rows.into_iter().map(|(k, v)| {
            div()
                .flex()
                .justify_between()
                .gap(px(10.))
                .mt(px(11.))
                .line_height(px(19.))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(p.muted)
                        .child(SharedString::from(k)),
                )
                .child(
                    div()
                        .max_w(px(155.))
                        .text_size(px(10.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.text)
                        .child(SharedString::from(v)),
                )
        })),
    ));
    // roles
    drawer = drawer.child(section(
        "AGENT ROLES",
        div().children(
            [
                ("fast", "Gather · 检索"),
                ("slow", "Analyze · 分析"),
                ("todo", "Plan · 调度"),
                ("validate", "Verify · 校验"),
            ]
            .map(|(role, desc)| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mt(px(13.))
                    .child(
                        div()
                            .w(px(53.))
                            .text_size(px(11.))
                            .font_family(crate::theme::MONO_FONT)
                            .text_color(p.text)
                            .child(role),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(10.))
                            .text_color(p.muted)
                            .child(desc),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .font_family(crate::theme::MONO_FONT)
                            .text_color(p.accent)
                            .child("—"),
                    )
            }),
        ),
    ));
    // todo / findings
    let findings = w
        .agent
        .active()
        .map(|s| s.findings.clone())
        .unwrap_or_default();
    drawer = drawer.child(section(
        crate::i18n::label("tasksFindings"),
        div().children(findings.iter().map(|(title, body)| {
            div()
                .py(px(8.))
                .border_b_1()
                .border_color(p.line)
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(11.))
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(p.accent)
                        .child("☐"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(p.muted)
                        .child(SharedString::from(format!("{title} — {body}"))),
                )
        })),
    ));
    // policy
    drawer = drawer.child(section(
        crate::i18n::label("toolPolicy"),
        div()
            .child(
                div()
                    .text_size(px(10.))
                    .font_family(crate::theme::MONO_FONT)
                    .line_height(px(18.))
                    .text_color(p.text)
                    .child("grep · read · git · find · make"),
            )
            .child(
                div()
                    .mt(px(7.))
                    .text_size(px(10.))
                    .line_height(px(18.))
                    .text_color(p.muted)
                    .child(crate::i18n::label("policyHint")),
            ),
    ));
    drawer.child(
        div()
            .mt_auto()
            .px(px(15.))
            .py(px(12.))
            .flex()
            .justify_between()
            .text_size(px(10.))
            .text_color(p.faint)
            .child("kres · source ↗")
            .child(crate::i18n::label("interactivePrototype")),
    )
}
