use std::collections::HashMap;
use std::sync::Arc;

use duthub_agent::{TestRequest, TestResponse};
use duthub_core::model::{ModelProvider, ProviderModel};
use duthub_core::{Provider, model_provider_credential_id};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Div, Entity, Focusable, InteractiveElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::{BtnVariant, btn, field, icon_btn, panel_head, subtle};
use crate::i18n::label;
use crate::theme::Palette;
use crate::workspace::Workbench;

#[derive(Default)]
pub(crate) struct Catalog {
    draft: Option<Draft>,
    notice: Option<String>,
    tests: HashMap<(String, String, Probe), uuid::Uuid>,
}

impl Catalog {
    pub(crate) fn close_editor(&mut self) {
        self.draft = None;
        self.notice = None;
    }
}

struct Draft {
    id: String,
    name: Entity<InputState>,
    base: Entity<InputState>,
    key: Entity<InputState>,
    model: Entity<InputState>,
    protocol: Provider,
    preset: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Probe {
    Api,
    Model,
}

impl Probe {
    fn label(self) -> &'static str {
        label(match self {
            Self::Api => "testProviderApi",
            Self::Model => "testProviderModel",
        })
    }
}

fn credential(provider: &ModelProvider) -> String {
    model_provider_credential_id(&provider.id, provider.protocol, &provider.base_url)
}

struct ProviderPreset {
    label: &'static str,
    protocol: Provider,
    base: &'static str,
    model: &'static str,
}

const PROVIDER_PRESETS: [ProviderPreset; 2] = [
    ProviderPreset {
        label: "DeepSeek",
        protocol: Provider::Anthropic,
        base: "https://api.deepseek.com/anthropic",
        model: "deepseek-flash",
    },
    ProviderPreset {
        label: "",
        protocol: Provider::Compatible,
        base: "",
        model: "",
    },
];

fn open_form(
    w: &mut Workbench,
    saved: Option<ModelProvider>,
    preset: usize,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let input = |value: &str, window: &mut Window, cx: &mut Context<Workbench>| {
        cx.new(|cx| InputState::new(window, cx).default_value(value.to_owned()))
    };
    let defaults = &PROVIDER_PRESETS[preset];
    let draft = Draft {
        id: saved.as_ref().map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |provider| provider.id.clone(),
        ),
        name: input(
            saved.as_ref().map_or(defaults.label, |value| &value.name),
            window,
            cx,
        ),
        base: input(
            saved
                .as_ref()
                .map_or(defaults.base, |value| &value.base_url),
            window,
            cx,
        ),
        key: cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(label("agentModelTokenPlaceholder"))
        }),
        model: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(label("enterModelName"))
                .default_value(if saved.is_none() { defaults.model } else { "" }.to_owned())
        }),
        protocol: saved.map_or(defaults.protocol, |provider| provider.protocol),
        preset,
    };
    w.provider_models.draft = Some(draft);
    w.provider_models.notice = None;
}

fn save_provider(
    w: &mut Workbench,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Result<(), String> {
    let draft = w.provider_models.draft.as_ref().expect("provider editor");
    let id = draft.id.clone();
    let name = draft.name.read(cx).value().trim().to_owned();
    if name.is_empty() {
        return Err(label("enterProviderName").into());
    }
    let base_url = duthub_agent::validate_model_base(draft.base.read(cx).value().as_ref())?;
    let key = draft.key.read(cx).value().trim().to_owned();
    let protocol = draft.protocol;
    let initial_model = draft.model.read(cx).value().trim().to_owned();
    let mut providers = w.settings.agent.providers.clone();
    let position = providers.iter().position(|provider| provider.id == id);
    let key_ref = model_provider_credential_id(&id, protocol, &base_url);
    let old_destination = position
        .map(|index| credential(&providers[index]))
        .filter(|saved| saved != &key_ref);
    if key.is_empty()
        && let Some(old_destination) = &old_destination
        && duthub_core::get_key(old_destination)
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err(label("providerEndpointNeedsKey").into());
    }
    let mut previous_key = None;
    if !key.is_empty() {
        previous_key = duthub_core::get_key(&key_ref).map_err(|error| error.to_string())?;
        duthub_core::set_key(&key_ref, &key).map_err(|error| error.to_string())?;
    }
    match position {
        Some(index) => {
            providers[index].name = name;
            providers[index].base_url = base_url;
            providers[index].protocol = protocol;
        }
        None => providers.push(ModelProvider {
            id: id.clone(),
            name,
            base_url,
            protocol,
            enabled: true,
            models: (!initial_model.is_empty())
                .then_some(ProviderModel {
                    id: initial_model,
                    enabled: true,
                })
                .into_iter()
                .collect(),
        }),
    }
    if let Err(error) = w.save_model_providers(providers) {
        if !key.is_empty() {
            let rollback = previous_key.as_deref().map_or_else(
                || duthub_core::delete_key(&key_ref),
                |saved| duthub_core::set_key(&key_ref, saved),
            );
            if let Err(rollback) = rollback {
                return Err(format!("{error} · {rollback}"));
            }
        }
        return Err(error);
    }
    w.provider_models
        .tests
        .retain(|(provider, _, _), _| provider != &id);
    draft_key_clear(w, window, cx);
    if position.is_none()
        && let Some(draft) = &w.provider_models.draft
    {
        draft
            .model
            .update(cx, |input, cx| input.set_value("", window, cx));
    }
    Ok(())
}

fn draft_key_clear(w: &Workbench, window: &mut Window, cx: &mut Context<Workbench>) {
    if let Some(draft) = &w.provider_models.draft {
        draft
            .key
            .update(cx, |input, cx| input.set_value("", window, cx));
    }
}

fn change_provider(
    w: &mut Workbench,
    id: &str,
    edit: impl FnOnce(&mut ModelProvider) -> Result<(), String>,
) -> Result<(), String> {
    let mut providers = w.settings.agent.providers.clone();
    let configured = providers
        .iter_mut()
        .find(|provider| provider.id == id)
        .ok_or_else(|| label("saveProviderFirst").to_owned())?;
    edit(configured)?;
    w.save_model_providers(providers)
}

fn start_probe(
    w: &mut Workbench,
    provider_id: &str,
    model: &str,
    probe: Probe,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let Some(provider) = w
        .settings
        .agent
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
    else {
        return;
    };
    let slot = (provider.id.clone(), model.to_owned(), probe);
    if !provider.enabled
        || !provider
            .models
            .iter()
            .any(|entry| entry.id == model && entry.enabled)
        || w.provider_models.tests.contains_key(&slot)
    {
        return;
    }
    let api_key = match duthub_core::get_key(&credential(provider)) {
        Ok(key) => key.unwrap_or_default(),
        Err(error) => {
            show_probe_result(w, model, probe, Err(error.to_string()), window, cx);
            return;
        }
    };
    let task = TestRequest {
        provider: provider.protocol,
        base_url: provider.base_url.clone(),
        model: model.to_owned(),
        api_key,
        prompt: "Reply with OK.".into(),
    };
    let request = uuid::Uuid::new_v4();
    w.provider_models.tests.insert(slot.clone(), request);
    cx.notify();
    let worker = cx.background_executor().spawn(async move {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(async {
            match probe {
                Probe::Api => duthub_agent::test_model_api(&task).await,
                Probe::Model => duthub_agent::test_model_connection(&task).await,
            }
        })
    });
    cx.spawn_in(window, async move |view, cx| {
        let result = worker.await;
        let _ = cx.update(|window, cx| {
            let _ = view.update(cx, |w, cx| {
                if w.provider_models.tests.get(&slot) == Some(&request) {
                    w.provider_models.tests.remove(&slot);
                    show_probe_result(w, &slot.1, probe, result, window, cx);
                    cx.notify();
                }
            });
        });
    })
    .detach();
}

fn show_probe_result(
    w: &mut Workbench,
    model: &str,
    probe: Probe,
    result: Result<TestResponse, String>,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let title = format!("{} · {model}", probe.label());
    let (summary, detail, color) = match result {
        Ok(result) => (
            label("providerTestPassed"),
            if result.reply.is_empty() {
                format!("{} ms", result.latency_ms)
            } else {
                format!("{} ms · {}", result.latency_ms, result.reply)
            },
            w.p.accent,
        ),
        Err(reason) => (label("providerTestFailed"), reason, w.p.red),
    };
    if window.has_active_dialog(cx) {
        w.notify_toast(format!("{title} · {summary}\n{detail}"));
        return;
    }
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(div().text_color(color).child(summary))
                .child(div().child(detail.clone())),
        )
    });
}

pub(crate) fn render(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    if w.provider_models.draft.is_none() {
        let first = w.settings.agent.providers.first().cloned();
        open_form(w, first, 0, window, cx);
    }
    let mut nav = div().flex().flex_wrap().gap(px(8.));
    for provider in w.settings.agent.providers.iter().cloned() {
        let selected = w
            .provider_models
            .draft
            .as_ref()
            .is_some_and(|draft| draft.id == provider.id);
        nav = nav.child(
            btn(
                format!("model-provider-{}", provider.id),
                &provider.name,
                if selected {
                    &BtnVariant::TinyPrimary
                } else {
                    &BtnVariant::Tiny
                },
                p,
            )
            .on_click(cx.listener(move |w, _, window, cx| {
                open_form(w, Some(provider.clone()), 0, window, cx);
                cx.notify();
            })),
        );
    }
    nav = nav.child(
        btn(
            "add-model-provider",
            label("addModelProvider"),
            &BtnVariant::Primary,
            p,
        )
        .on_click(cx.listener(|w, _, window, cx| {
            open_form(w, None, 0, window, cx);
            cx.notify();
        })),
    );
    let form = render_provider_form(w, p, cx);
    let page = div()
        .max_w(px(820.))
        .w_full()
        .mx_auto()
        .p(px(24.))
        .flex()
        .flex_col()
        .gap(px(14.))
        .child(nav)
        .child(form);
    agent_config_sections(page, w, p, window, cx)
}

fn render_provider_form(
    w: &mut Workbench,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let draft = w.provider_models.draft.as_ref().expect("provider editor");
    let id = draft.id.clone();
    let protocol = draft.protocol;
    let preset = draft.preset;
    let name = draft.name.clone();
    let base = draft.base.clone();
    let key = draft.key.clone();
    let model = draft.model.clone();
    let saved = w
        .settings
        .agent
        .providers
        .iter()
        .find(|provider| provider.id == id)
        .cloned();
    let protocols = [
        (Provider::Compatible, "Chat Completions (/chat/completions)"),
        (Provider::Anthropic, "Messages (/v1/messages)"),
    ];
    let selection = protocols
        .iter()
        .position(|(value, _)| *value == protocol)
        .unwrap_or_default();
    let chooser = w.select_field(
        "provider-api-format",
        protocols[selection].1.into(),
        protocols
            .iter()
            .enumerate()
            .map(|(index, (_, name))| (name.to_string(), index))
            .collect(),
        Some(selection),
        Arc::new(move |w: &mut Workbench, index, _, cx| {
            if let Some(draft) = &mut w.provider_models.draft {
                draft.protocol = protocols[index].0;
            }
            cx.notify();
        }),
        p,
        cx,
    );
    let mut head = panel_head("spark", label("agentModelProvider"), p);
    if let Some(provider) = &saved {
        let id = provider.id.clone();
        head = head.child(
            Switch::new("model-provider-enabled")
                .checked(provider.enabled)
                .label(label("enableModelProvider"))
                .on_click(cx.listener(move |w, enabled, _, cx| {
                    let result = change_provider(w, &id, |provider| {
                        provider.enabled = *enabled;
                        Ok(())
                    });
                    w.provider_models.notice = result.err();
                    cx.notify();
                })),
        );
    }
    let mut body = div().p(px(16.));
    if saved.is_none() {
        let options: Vec<_> = PROVIDER_PRESETS
            .iter()
            .enumerate()
            .map(|(index, preset)| {
                let name = if preset.label.is_empty() {
                    label("providerPresetCustom")
                } else {
                    preset.label
                };
                (name.to_owned(), index)
            })
            .collect();
        let chooser = w.select_field(
            "goose-provider-preset",
            options[preset].0.clone(),
            options,
            Some(preset),
            Arc::new(|w, index, window, cx| {
                if index < PROVIDER_PRESETS.len()
                    && w.provider_models
                        .draft
                        .as_ref()
                        .is_some_and(|draft| draft.preset != index)
                {
                    open_form(w, None, index, window, cx);
                }
                cx.notify();
            }),
            p,
            cx,
        );
        body = body.child(field(label("providerPreset"), chooser, None, p));
    }
    body = body
        .child(field(
            label("providerName"),
            Input::new(&name)
                .id("model-provider-name")
                .w_full()
                .bg(p.bg)
                .text_size(px(13.)),
            None,
            p,
        ))
        .child(field(
            "Base URL",
            Input::new(&base)
                .id("model-provider-base")
                .w_full()
                .bg(p.bg)
                .text_size(px(13.)),
            None,
            p,
        ))
        .child(field(label("providerApiFormat"), chooser, None, p))
        .when(saved.is_none(), |body| {
            body.child(field(
                label("agentModelName"),
                Input::new(&model)
                    .id("provider-initial-model")
                    .w_full()
                    .bg(p.bg),
                None,
                p,
            ))
        })
        .child(field(
            label("agentModelToken"),
            Input::new(&key)
                .id("model-provider-key")
                .w_full()
                .bg(p.bg)
                .text_size(px(13.))
                .mask_toggle(),
            None,
            p,
        ))
        .child(
            div().flex().justify_end().child(
                btn(
                    "save-model-provider",
                    label("saveModelProvider"),
                    &BtnVariant::Primary,
                    p,
                )
                .on_click(cx.listener(|w, _, window, cx| {
                    let result = save_provider(w, window, cx);
                    w.provider_models.notice = Some(
                        result
                            .err()
                            .unwrap_or_else(|| label("agentModelSaved").into()),
                    );
                    cx.notify();
                })),
            ),
        );
    if let Some(provider) = saved {
        body = body.child(model_list(w, &provider, p, cx));
    }
    if let Some(notice) = &w.provider_models.notice {
        body = body.child(subtle(notice, p).mt(px(8.)));
    }
    div()
        .border_1()
        .border_color(p.border)
        .rounded(px(12.))
        .bg(p.surface)
        .child(head)
        .child(body)
}

fn model_list(
    w: &Workbench,
    provider: &ModelProvider,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let provider_id = provider.id.clone();
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .child(label("providerModelList"))
        .child(
            btn(
                "add-provider-model",
                label("addProviderModel"),
                &BtnVariant::Tiny,
                p,
            )
            .icon(crate::icons::kit_icon("plus").size(px(14.)))
            .on_click(cx.listener(move |_, _, window, cx| {
                edit_model_dialog(&provider_id, None, p, window, cx);
            })),
        );
    let mut rows = div()
        .border_1()
        .border_color(p.border)
        .rounded(px(8.))
        .overflow_hidden();
    for (index, model) in provider.models.iter().enumerate() {
        let active = w.settings.agent.active_provider_id.as_deref() == Some(provider.id.as_str())
            && w.settings.agent.active_model_id.as_deref() == Some(model.id.as_str());
        let mut actions = div().flex().flex_none().items_center().gap(px(2.));
        for (probe, icon) in [(Probe::Api, "plug"), (Probe::Model, "flask-conical")] {
            let id = provider.id.clone();
            let model_id = model.id.clone();
            let running =
                w.provider_models
                    .tests
                    .contains_key(&(id.clone(), model_id.clone(), probe));
            actions = actions.child(
                icon_btn(format!("provider-{icon}-test-{index}"), icon, p)
                    .tooltip(if running {
                        label("providerTestRunning")
                    } else {
                        probe.label()
                    })
                    .disabled(running || !provider.enabled || !model.enabled)
                    .on_click(cx.listener(move |w, _, window, cx| {
                        start_probe(w, &id, &model_id, probe, window, cx);
                    })),
            );
        }
        let id = provider.id.clone();
        let model_id = model.id.clone();
        actions = actions.child(
            icon_btn(format!("edit-provider-model-{index}"), "pencil", p)
                .tooltip(label("editProviderModel"))
                .on_click(cx.listener(move |_, _, window, cx| {
                    edit_model_dialog(&id, Some(&model_id), p, window, cx);
                })),
        );
        let id = provider.id.clone();
        let model_id = model.id.clone();
        actions = actions.child(
            icon_btn(
                format!("use-provider-model-{index}"),
                if active { "check" } else { "circle-check" },
                p,
            )
            .tooltip(label(if active {
                "providerModelActive"
            } else {
                "providerModelUse"
            }))
            .disabled(active || !provider.enabled || !model.enabled)
            .on_click(cx.listener(move |w, _, _, cx| {
                w.select_agent_model(&id, &model_id);
                cx.notify();
            })),
        );
        let id = provider.id.clone();
        let model_id = model.id.clone();
        actions = actions.child(
            Switch::new(("enable-provider-model", index))
                .checked(model.enabled)
                .on_click(cx.listener(move |w, enabled, _, cx| {
                    let result = change_provider(w, &id, |provider| {
                        if let Some(model) = provider
                            .models
                            .iter_mut()
                            .find(|model| model.id == model_id)
                        {
                            model.enabled = *enabled;
                        }
                        Ok(())
                    });
                    w.provider_models.notice = result.err();
                    cx.notify();
                })),
        );
        rows = rows.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(3.))
                .when(index > 0, |row| row.border_t_1().border_color(p.line))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .font_family(crate::theme::MONO_FONT)
                        .text_color(if model.enabled { p.text } else { p.muted })
                        .child(model.id.clone()),
                )
                .child(actions),
        );
    }
    div()
        .mt(px(16.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(header)
        .when(!provider.models.is_empty(), |list| list.child(rows))
}

fn edit_model_dialog(
    provider: &str,
    model: Option<&str>,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let title = label(if model.is_some() {
        "editProviderModel"
    } else {
        "addProviderModel"
    });
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(label("enterModelName"))
            .default_value(model.unwrap_or_default().to_owned())
    });
    let focus = input.clone();
    let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let provider = provider.to_owned();
    let previous = model.map(str::to_owned);
    let view = cx.entity();
    window.open_dialog(cx, move |dialog, _, _| {
        let view = view.clone();
        let provider = provider.clone();
        let previous = previous.clone();
        let draft = input.clone();
        let reason = error.borrow().clone();
        let error = error.clone();
        dialog
            .title(title)
            .child(Input::new(&input).id("provider-model-id").w_full())
            .child(
                div().children(
                    reason.map(|reason| div().pt(px(6.)).text_color(p.red).child(reason)),
                ),
            )
            .footer(
                // Hand-drawn button inside Dialog overlay (gpui-kit Button loses text in dialogs).
                div().flex().justify_end().child(
                    div()
                        .id("save-provider-model")
                        .flex()
                        .items_center()
                        .justify_center()
                        .min_h(px(32.0))
                        .px(px(14.0))
                        .rounded(px(8.0))
                        .bg(p.accent)
                        .text_size(px(13.0))
                        .font_weight(gpui_kit::FontWeight(700.0))
                        .text_color(p.accent_contrast)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .child(label("save"))
                        .on_click(move |_, window, cx| {
                            view.update(cx, |w, cx| {
                                let value = draft.read(cx).value();
                                match save_model_entry(
                                    w,
                                    &provider,
                                    previous.as_deref(),
                                    value.trim(),
                                ) {
                                    Ok(()) => window.close_dialog(cx),
                                    Err(reason) => {
                                        *error.borrow_mut() = Some(reason);
                                        window.refresh();
                                    }
                                }
                                cx.notify();
                            });
                        }),
                ),
            )
    });
    window.defer(cx, move |window, cx| {
        focus.read(cx).focus_handle(cx).focus(window, cx);
    });
}

fn save_model_entry(
    w: &mut Workbench,
    provider: &str,
    previous: Option<&str>,
    model: &str,
) -> Result<(), String> {
    if model.is_empty() {
        return Err(label("enterModelName").into());
    }
    let mut candidate = w.settings.clone();
    let agent = &mut candidate.agent;
    let models = &mut agent
        .providers
        .iter_mut()
        .find(|entry| entry.id == provider)
        .ok_or_else(|| label("saveProviderFirst").to_owned())?
        .models;
    if previous != Some(model) && models.iter().any(|entry| entry.id == model) {
        return Err(label("duplicateProviderModel").into());
    }
    if let Some(previous) = previous {
        let entry = models
            .iter_mut()
            .find(|entry| entry.id == previous)
            .ok_or_else(|| label("enterModelName").to_owned())?;
        model.clone_into(&mut entry.id);
        if agent.active_provider_id.as_deref() == Some(provider)
            && agent.active_model_id.as_deref() == Some(previous)
        {
            agent.active_model_id = Some(model.to_owned());
        }
    } else {
        models.push(ProviderModel {
            id: model.to_owned(),
            enabled: true,
        });
    }
    w.save_model_catalog(candidate)?;
    w.provider_models
        .tests
        .retain(|(id, old_model, _), _| id != provider || previous != Some(old_model.as_str()));
    Ok(())
}

/// Agent 页下半部分：goose 运行时（版本/模型/外部路径/三行徽标）→ A2A 协作 →
/// Agent 接入配置（自设置页迁入）。
fn agent_config_sections(
    page: Div,
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    page.child(crate::pages::agent_runtime_panel(w, p, window, cx))
        .child(crate::pages::a2a_panel(w, p, window, cx))
        .child(crate::pages::agent_access_panel(w, p, window, cx))
}
