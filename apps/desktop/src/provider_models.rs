use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;

use duthub_agent::{TestRequest, TestResponse};
use duthub_core::model::{ModelCapabilities, ModelInputModalities, ModelProvider, ProviderModel};
use duthub_core::{Provider, model_provider_credential_id};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Div, Entity, Focusable, InteractiveElement, ParentElement, SharedString,
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
    fetching_models: HashMap<String, uuid::Uuid>,
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
    let base_url = duthub_agent::validate_model_base(draft.base.read(cx).value().as_ref())
        .map_err(|error| error.to_string())?;
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
                .then_some(ProviderModel::new(initial_model, true))
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
                Probe::Api => duthub_agent::test_model_api(&task)
                    .await
                    .map_err(|error| error.to_string()),
                Probe::Model => duthub_agent::test_model_connection(&task)
                    .await
                    .map_err(|error| error.to_string()),
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
        (Provider::Compatible, "OpenAI Chat Completions"),
        (Provider::OpenaiResponses, "OpenAI Responses"),
        (Provider::Anthropic, "Anthropic Messages"),
        (Provider::Gemini, "Gemini API"),
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

fn format_context_badge(tokens: u64) -> String {
    match tokens {
        n if n >= 1_000_000 && n.is_multiple_of(1_000_000) => format!("{}M", n / 1_000_000),
        n if n >= 1_000_000 => {
            let tenths = (n + 50_000) / 100_000;
            format!("{}.{}M", tenths / 10, tenths % 10)
        }
        n if n >= 1_000 => format!("{}K", n / 1_000),
        n => n.to_string(),
    }
}

fn start_fetch_models(
    w: &mut Workbench,
    provider_id: &str,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let Some(provider) = w
        .settings
        .agent
        .providers
        .iter()
        .find(|entry| entry.id == provider_id)
        .cloned()
    else {
        return;
    };
    if w.provider_models.fetching_models.contains_key(&provider.id) {
        return;
    }
    let api_key = match duthub_core::get_key(&credential(&provider)) {
        Ok(Some(key)) if !key.trim().is_empty() => key,
        Ok(_) => {
            w.provider_models.notice = Some(label("providerEndpointNeedsKey").to_owned());
            cx.notify();
            return;
        }
        Err(error) => {
            w.provider_models.notice = Some(error.to_string());
            cx.notify();
            return;
        }
    };
    let req = TestRequest {
        provider: provider.protocol,
        base_url: provider.base_url.clone(),
        model: String::new(),
        api_key,
        prompt: String::new(),
    };
    let token = uuid::Uuid::new_v4();
    let pid = provider.id.clone();
    w.provider_models.fetching_models.insert(pid.clone(), token);
    w.provider_models.notice = None;
    cx.notify();
    let worker = cx.background_executor().spawn(async move {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(async {
            duthub_agent::fetch_available_models(&req)
                .await
                .map_err(|error| error.to_string())
        })
    });
    cx.spawn_in(window, async move |view, cx| {
        let result = worker.await;
        let _ = cx.update(|window, cx| {
            let _ = view.update(cx, |w, cx| {
                if w.provider_models.fetching_models.get(&pid) != Some(&token) {
                    return;
                }
                w.provider_models.fetching_models.remove(&pid);
                match result {
                    Ok(models) if models.is_empty() => {
                        w.provider_models.notice = Some(label("fetchModelsEmpty").to_owned());
                    }
                    Ok(models) => {
                        show_fetched_models_dialog(w, &pid, models, w.p, window, cx);
                    }
                    Err(error) => {
                        w.provider_models.notice = Some(error);
                    }
                }
                cx.notify();
            });
        });
    })
    .detach();
}

fn show_fetched_models_dialog(
    w: &Workbench,
    provider_id: &str,
    discovered: Vec<String>,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let existing: BTreeSet<String> = w
        .settings
        .agent
        .providers
        .iter()
        .find(|entry| entry.id == provider_id)
        .map_or_else(BTreeSet::new, |entry| {
            entry.models.iter().map(|m| m.id.clone()).collect()
        });
    let selected = Rc::new(RefCell::new(BTreeSet::<String>::new()));
    let provider = provider_id.to_owned();
    let view = cx.entity();
    window.open_dialog(cx, move |dialog, _, _| {
        let mut list = div()
            .id("fetched-models-list")
            .flex()
            .flex_col()
            .gap(px(4.))
            .max_h(px(360.))
            .overflow_y_scroll();
        for (idx, model_id) in discovered.iter().enumerate() {
            let already = existing.contains(model_id);
            let checked = already || selected.borrow().contains(model_id);
            let sel = selected.clone();
            let mid = model_id.clone();
            list = list.child(
                div()
                    .id(("fetched-model-row", idx))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(7.))
                    .bg(if checked { p.hover } else { p.bg })
                    .border_1()
                    .border_color(if checked { p.accent } else { p.line })
                    .when(!already, |row| {
                        row.cursor_pointer().on_click(move |_, window, _| {
                            let mut set = sel.borrow_mut();
                            if !set.remove(&mid) {
                                set.insert(mid.clone());
                            }
                            window.refresh();
                        })
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .min_w_0()
                            .flex_1()
                            .child(
                                div()
                                    .w(px(14.))
                                    .h(px(14.))
                                    .rounded(px(3.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .border_1()
                                    .border_color(if checked { p.accent } else { p.border })
                                    .bg(if checked { p.accent } else { p.bg })
                                    .when(checked, |box_el| {
                                        box_el.child(
                                            crate::icons::kit_icon("check")
                                                .size(px(10.))
                                                .text_color(p.accent_contrast),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .font_family(crate::theme::MONO_FONT)
                                    .text_size(px(12.))
                                    .text_color(if already { p.muted } else { p.text })
                                    .child(SharedString::from(model_id.clone())),
                            ),
                    )
                    .when(already, |row| {
                        row.child(subtle(label("fetchModelsAlreadyAdded"), p))
                    }),
            );
        }
        let chosen_count = selected.borrow().len();
        let view_save = view.clone();
        let provider_save = provider.clone();
        let sel_save = selected.clone();
        dialog.title(label("fetchModelsTitle")).child(list).footer(
            div()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.))
                .child(
                    div()
                        .id("cancel-fetched-models")
                        .flex()
                        .items_center()
                        .justify_center()
                        .min_h(px(32.))
                        .px(px(14.))
                        .rounded(px(8.))
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.border)
                        .text_size(px(13.))
                        .text_color(p.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .child(label("cancel"))
                        .on_click(|_, window, cx| {
                            window.close_dialog(cx);
                        }),
                )
                .child(
                    div()
                        .id("confirm-fetched-models")
                        .flex()
                        .items_center()
                        .justify_center()
                        .min_h(px(32.))
                        .px(px(14.))
                        .rounded(px(8.))
                        .bg(p.accent)
                        .text_size(px(13.))
                        .font_weight(gpui_kit::FontWeight(700.0))
                        .text_color(p.accent_contrast)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .child(SharedString::from(if chosen_count > 0 {
                            format!("{} ({chosen_count})", label("fetchModelsAddSelected"))
                        } else {
                            label("fetchModelsAddSelected").to_owned()
                        }))
                        .on_click(move |_, window, cx| {
                            let picks: Vec<String> = sel_save.borrow().iter().cloned().collect();
                            if picks.is_empty() {
                                window.close_dialog(cx);
                                return;
                            }
                            view_save.update(cx, |w, cx| {
                                let added = picks.len();
                                let res = change_provider(w, &provider_save, |entry| {
                                    for mid in &picks {
                                        if !entry.models.iter().any(|m| &m.id == mid) {
                                            entry.models.push(ProviderModel::new(mid, true));
                                        }
                                    }
                                    Ok(())
                                });
                                match res {
                                    Ok(()) => {
                                        w.provider_models.notice = Some(crate::i18n::message(
                                            "fetchModelsAdded",
                                            &[("count", &added.to_string())],
                                        ));
                                        window.close_dialog(cx);
                                    }
                                    Err(err) => {
                                        w.provider_models.notice = Some(err);
                                    }
                                }
                                cx.notify();
                            });
                        }),
                ),
        )
    });
}

fn model_list(
    w: &Workbench,
    provider: &ModelProvider,
    p: &'static Palette,
    cx: &mut Context<Workbench>,
) -> Div {
    let provider_id = provider.id.clone();
    let fetch_pid = provider.id.clone();
    let fetching = w.provider_models.fetching_models.contains_key(&provider.id);
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(label("providerModelList"))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    btn(
                        "fetch-provider-models",
                        if fetching {
                            label("fetchingAvailableModels")
                        } else {
                            label("fetchAvailableModels")
                        },
                        &BtnVariant::Tiny,
                        p,
                    )
                    .disabled(fetching)
                    .on_click(cx.listener(move |w, _, window, cx| {
                        start_fetch_models(w, &fetch_pid, window, cx);
                    })),
                )
                .child(
                    btn(
                        "add-provider-model",
                        label("addProviderModel"),
                        &BtnVariant::Tiny,
                        p,
                    )
                    .icon(crate::icons::kit_icon("plus").size(px(14.)))
                    .on_click(cx.listener(move |w, _, window, cx| {
                        edit_model_dialog(w, &provider_id, None, p, window, cx);
                    })),
                ),
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
                .on_click(cx.listener(move |w, _, window, cx| {
                    edit_model_dialog(w, &id, Some(&model_id), p, window, cx);
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
        let mut left = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(if model.enabled { p.text } else { p.muted })
                    .child(model.id.clone()),
            );
        if let Some(ctx_tokens) = model.context_window.filter(|v| *v > 0) {
            left = left.child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(5.))
                    .bg(p.raised)
                    .border_1()
                    .border_color(p.border)
                    .text_size(px(10.5))
                    .font_family(crate::theme::MONO_FONT)
                    .text_color(p.muted)
                    .child(SharedString::from(format_context_badge(ctx_tokens))),
            );
        }
        if model.input_modalities.supports_vision() {
            left = left.child(
                div()
                    .flex_none()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(999.))
                    .bg(p.raised)
                    .border_1()
                    .border_color(p.border)
                    .text_size(px(10.5))
                    .text_color(p.muted)
                    .child(label("modelVisionBadge")),
            );
        }
        rows = rows.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(5.))
                .when(index > 0, |row| row.border_t_1().border_color(p.line))
                .child(left)
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

#[derive(Clone)]
struct ModelDialogState {
    advanced_open: bool,
    modalities: ModelInputModalities,
    capabilities: ModelCapabilities,
    reasoning_levels: Vec<String>,
    error: Option<String>,
}

type ToggleCallback = Box<dyn Fn(&mut Window)>;

fn check_pill(
    id: &'static str,
    title: &'static str,
    checked: bool,
    locked: bool,
    p: &'static Palette,
    on_toggle: Option<ToggleCallback>,
) -> gpui_kit::Stateful<Div> {
    let mut el = div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .py(px(5.))
        .rounded(px(7.))
        .bg(if checked { p.raised } else { p.bg })
        .border_1()
        .border_color(if checked { p.accent } else { p.border })
        .text_size(px(12.))
        .text_color(if locked { p.muted } else { p.text })
        .child(
            div()
                .w(px(14.))
                .h(px(14.))
                .rounded(px(3.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(if checked { p.accent } else { p.border })
                .bg(if checked { p.accent } else { p.bg })
                .when(checked, |b| {
                    b.child(
                        crate::icons::kit_icon("check")
                            .size(px(10.))
                            .text_color(p.accent_contrast),
                    )
                }),
        )
        .child(title);
    if locked {
        el = el.child(
            crate::icons::kit_icon("lock")
                .size(px(11.))
                .text_color(p.muted),
        );
    }
    if let Some(handler) = on_toggle {
        el = el.cursor_pointer().on_click(move |_, window, _| {
            handler(window);
            window.refresh();
        });
    }
    el
}

fn edit_model_dialog(
    w: &Workbench,
    provider: &str,
    model: Option<&str>,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let existing = model.and_then(|mid| {
        w.settings
            .agent
            .providers
            .iter()
            .find(|entry| entry.id == provider)
            .and_then(|entry| entry.models.iter().find(|m| m.id == mid))
            .cloned()
    });
    let title = label(if existing.is_some() {
        "editProviderModel"
    } else {
        "addProviderModel"
    });
    let id_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(label("enterModelName"))
            .default_value(existing.as_ref().map_or_else(String::new, |m| m.id.clone()))
    });
    let ctx_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("1000000")
            .default_value(
                existing
                    .as_ref()
                    .and_then(|m| m.context_window)
                    .map_or_else(String::new, |v| v.to_string()),
            )
    });
    let max_out_input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("128000")
            .default_value(
                existing
                    .as_ref()
                    .and_then(|m| m.max_output_tokens)
                    .map_or_else(String::new, |v| v.to_string()),
            )
    });
    let level_input = cx
        .new(|cx| InputState::new(window, cx).placeholder(label("modelReasoningLevelPlaceholder")));
    let params_default = existing
        .as_ref()
        .and_then(|m| m.reasoning_params.as_ref())
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_default();
    let params_input = cx.new(|cx| {
        TextareaState::new(window, cx)
            .placeholder(label("modelReasoningParamsPlaceholder"))
            .default_value(params_default)
    });
    let state = Rc::new(RefCell::new(ModelDialogState {
        advanced_open: existing.as_ref().is_some_and(|m| {
            !m.input_modalities.is_default()
                || !m.capabilities.is_default()
                || !m.reasoning_levels.is_empty()
                || m.reasoning_params.is_some()
        }),
        modalities: existing
            .as_ref()
            .map_or_else(ModelInputModalities::default, |m| {
                m.input_modalities.clone()
            }),
        capabilities: existing
            .as_ref()
            .map_or_else(ModelCapabilities::default, |m| m.capabilities.clone()),
        reasoning_levels: existing
            .as_ref()
            .map_or_else(Vec::new, |m| m.reasoning_levels.clone()),
        error: None,
    }));
    let focus = id_input.clone();
    let provider = provider.to_owned();
    let previous = model.map(str::to_owned);
    let view = cx.entity();
    window.open_dialog(cx, move |dialog, _, _| {
        let snapshot = state.borrow().clone();
        let mut content = div()
            .id("edit-model-dialog-scroll")
            .flex()
            .flex_col()
            .gap(px(10.))
            .max_h(px(460.))
            .overflow_y_scroll()
            .child(field(
                label("agentModelName"),
                Input::new(&id_input)
                    .id("provider-model-id")
                    .w_full()
                    .bg(p.bg),
                None,
                p,
            ))
            .child(field(
                label("modelContextWindow"),
                Input::new(&ctx_input)
                    .id("provider-model-context")
                    .w_full()
                    .bg(p.bg),
                None,
                p,
            ))
            .child(field(
                label("modelMaxOutputTokens"),
                Input::new(&max_out_input)
                    .id("provider-model-max-output")
                    .w_full()
                    .bg(p.bg),
                None,
                p,
            ));

        let adv_state = state.clone();
        content = content.child(
            div()
                .id("toggle-model-advanced")
                .flex()
                .items_center()
                .gap(px(6.))
                .py(px(4.))
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(p.text)
                .child(
                    crate::icons::kit_icon(if snapshot.advanced_open {
                        "chevron-down"
                    } else {
                        "chevron-right"
                    })
                    .size(px(14.))
                    .text_color(p.muted),
                )
                .child(label("modelAdvancedConfig"))
                .on_click(move |_, window, _| {
                    let mut st = adv_state.borrow_mut();
                    st.advanced_open = !st.advanced_open;
                    window.refresh();
                }),
        );

        if snapshot.advanced_open {
            let st_img = state.clone();
            let st_vid = state.clone();
            let st_pdf = state.clone();
            let modalities_row = div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .child(check_pill(
                    "mod-text",
                    label("modelModalityText"),
                    true,
                    true,
                    p,
                    None,
                ))
                .child(check_pill(
                    "mod-image",
                    label("modelModalityImage"),
                    snapshot.modalities.image,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = st_img.borrow_mut();
                        st.modalities.image = !st.modalities.image;
                    })),
                ))
                .child(check_pill(
                    "mod-video",
                    label("modelModalityVideo"),
                    snapshot.modalities.video,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = st_vid.borrow_mut();
                        st.modalities.video = !st.modalities.video;
                    })),
                ))
                .child(check_pill(
                    "mod-pdf",
                    label("modelModalityPdf"),
                    snapshot.modalities.pdf,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = st_pdf.borrow_mut();
                        st.modalities.pdf = !st.modalities.pdf;
                    })),
                ));

            let state_structured = state.clone();
            let state_web_search = state.clone();
            let state_mid_system = state.clone();
            let caps_row = div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .child(check_pill(
                    "cap-structured",
                    label("modelCapStructuredOutput"),
                    snapshot.capabilities.structured_output,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = state_structured.borrow_mut();
                        st.capabilities.structured_output = !st.capabilities.structured_output;
                    })),
                ))
                .child(check_pill(
                    "cap-websearch",
                    label("modelCapWebSearch"),
                    snapshot.capabilities.web_search,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = state_web_search.borrow_mut();
                        st.capabilities.web_search = !st.capabilities.web_search;
                    })),
                ))
                .child(check_pill(
                    "cap-midsystem",
                    label("modelCapMidSystem"),
                    snapshot.capabilities.mid_conversation_system,
                    false,
                    p,
                    Some(Box::new(move |_| {
                        let mut st = state_mid_system.borrow_mut();
                        st.capabilities.mid_conversation_system =
                            !st.capabilities.mid_conversation_system;
                    })),
                ));

            let mut levels_row = div().flex().flex_wrap().items_center().gap(px(6.));
            for (idx, level) in snapshot.reasoning_levels.iter().enumerate() {
                let st_rm = state.clone();
                let lvl = level.clone();
                levels_row = levels_row.child(
                    div()
                        .id(("reasoning-level-chip", idx))
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .px(px(9.))
                        .py(px(4.))
                        .rounded(px(7.))
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.border)
                        .font_family(crate::theme::MONO_FONT)
                        .text_size(px(12.))
                        .text_color(p.text)
                        .cursor_pointer()
                        .hover(|s| s.border_color(p.red))
                        .child(SharedString::from(level.clone()))
                        .child(
                            crate::icons::kit_icon("x")
                                .size(px(11.))
                                .text_color(p.muted),
                        )
                        .on_click(move |_, window, _| {
                            st_rm.borrow_mut().reasoning_levels.retain(|l| l != &lvl);
                            window.refresh();
                        }),
                );
            }
            let st_add = state.clone();
            let lvl_input_add = level_input.clone();
            levels_row = levels_row
                .child(
                    div().w(px(130.)).child(
                        Input::new(&level_input)
                            .id("model-reasoning-level-input")
                            .bg(p.bg),
                    ),
                )
                .child(
                    div()
                        .id("add-reasoning-level")
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(28.))
                        .h(px(28.))
                        .rounded(px(7.))
                        .bg(p.raised)
                        .border_1()
                        .border_color(p.border)
                        .cursor_pointer()
                        .hover(|s| s.bg(p.hover))
                        .child(
                            crate::icons::kit_icon("plus")
                                .size(px(13.))
                                .text_color(p.text),
                        )
                        .on_click(move |_, window, cx| {
                            let next = lvl_input_add.read(cx).value().trim().to_owned();
                            if !next.is_empty() {
                                let mut st = st_add.borrow_mut();
                                if !st.reasoning_levels.contains(&next) {
                                    st.reasoning_levels.push(next);
                                }
                                lvl_input_add.update(cx, |inp, cx| inp.set_value("", window, cx));
                                window.refresh();
                            }
                        }),
                );

            content = content
                .child(field(
                    label("modelInputModalities"),
                    modalities_row,
                    None,
                    p,
                ))
                .child(field(label("modelCapabilities"), caps_row, None, p))
                .child(field(label("modelReasoningLevels"), levels_row, None, p))
                .child(field(
                    label("modelReasoningParams"),
                    Textarea::new(&params_input)
                        .w_full()
                        .h(px(96.))
                        .bg(p.bg)
                        .font_family(crate::theme::MONO_FONT)
                        .text_size(px(12.)),
                    None,
                    p,
                ));
        }

        if let Some(reason) = snapshot.error {
            content = content.child(
                div()
                    .pt(px(4.))
                    .text_size(px(12.))
                    .text_color(p.red)
                    .child(reason),
            );
        }

        let st_reset = state.clone();
        let ctx_reset = ctx_input.clone();
        let max_reset = max_out_input.clone();
        let lvl_reset = level_input.clone();
        let params_reset = params_input.clone();

        let view_save = view.clone();
        let provider_save = provider.clone();
        let previous_save = previous.clone();
        let id_save = id_input.clone();
        let ctx_save = ctx_input.clone();
        let max_save = max_out_input.clone();
        let params_save = params_input.clone();
        let st_save = state.clone();

        dialog.title(title).child(content).footer(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .child(
                    div()
                        .id("reset-provider-model-form")
                        .text_size(px(12.5))
                        .text_color(p.muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(p.text))
                        .child(label("modelResetForm"))
                        .on_click(move |_, window, cx| {
                            ctx_reset.update(cx, |inp, cx| inp.set_value("", window, cx));
                            max_reset.update(cx, |inp, cx| inp.set_value("", window, cx));
                            lvl_reset.update(cx, |inp, cx| inp.set_value("", window, cx));
                            params_reset.update(cx, |inp, cx| inp.set_value("", window, cx));
                            let mut st = st_reset.borrow_mut();
                            st.modalities = ModelInputModalities::default();
                            st.capabilities = ModelCapabilities::default();
                            st.reasoning_levels.clear();
                            st.error = None;
                            window.refresh();
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .id("cancel-provider-model")
                                .flex()
                                .items_center()
                                .justify_center()
                                .min_h(px(32.0))
                                .px(px(14.0))
                                .rounded(px(8.0))
                                .bg(p.raised)
                                .border_1()
                                .border_color(p.border)
                                .text_size(px(13.0))
                                .text_color(p.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(p.hover))
                                .child(label("cancel"))
                                .on_click(|_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
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
                                    let id_val = id_save.read(cx).value().trim().to_owned();
                                    let ctx_raw = ctx_save.read(cx).value().trim().to_owned();
                                    let max_raw = max_save.read(cx).value().trim().to_owned();
                                    let params_raw = params_save.read(cx).value().trim().to_owned();
                                    let parse_opt_u64 = |raw: &str| -> Result<Option<u64>, String> {
                                        if raw.is_empty() {
                                            return Ok(None);
                                        }
                                        raw.parse::<u64>()
                                            .ok()
                                            .filter(|v| *v > 0)
                                            .map(Some)
                                            .ok_or_else(|| label("modelInvalidNumber").to_owned())
                                    };
                                    let parsed = (|| -> Result<ProviderModel, String> {
                                        let context_window = parse_opt_u64(&ctx_raw)?;
                                        let max_output_tokens = parse_opt_u64(&max_raw)?;
                                        let reasoning_params = if params_raw.is_empty() {
                                            None
                                        } else {
                                            let val: serde_json::Value = serde_json::from_str(
                                                &params_raw,
                                            )
                                            .map_err(|_| label("modelInvalidJson").to_owned())?;
                                            if !val.is_object() {
                                                return Err(label("modelInvalidJson").to_owned());
                                            }
                                            Some(val)
                                        };
                                        let st = st_save.borrow();
                                        Ok(ProviderModel {
                                            id: id_val,
                                            enabled: true,
                                            context_window,
                                            max_output_tokens,
                                            input_modalities: st.modalities.clone(),
                                            capabilities: st.capabilities.clone(),
                                            reasoning_levels: st.reasoning_levels.clone(),
                                            reasoning_params,
                                        })
                                    })();
                                    match parsed {
                                        Ok(entry) => {
                                            view_save.update(cx, |w, cx| {
                                                match save_model_entry(
                                                    w,
                                                    &provider_save,
                                                    previous_save.as_deref(),
                                                    entry,
                                                ) {
                                                    Ok(()) => window.close_dialog(cx),
                                                    Err(reason) => {
                                                        st_save.borrow_mut().error = Some(reason);
                                                        window.refresh();
                                                    }
                                                }
                                                cx.notify();
                                            });
                                        }
                                        Err(reason) => {
                                            st_save.borrow_mut().error = Some(reason);
                                            window.refresh();
                                        }
                                    }
                                }),
                        ),
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
    mut model: ProviderModel,
) -> Result<(), String> {
    if model.id.is_empty() {
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
    if previous != Some(model.id.as_str()) && models.iter().any(|entry| entry.id == model.id) {
        return Err(label("duplicateProviderModel").into());
    }
    if let Some(previous) = previous {
        let entry = models
            .iter_mut()
            .find(|entry| entry.id == previous)
            .ok_or_else(|| label("enterModelName").to_owned())?;
        model.enabled = entry.enabled;
        *entry = model.clone();
        if agent.active_provider_id.as_deref() == Some(provider)
            && agent.active_model_id.as_deref() == Some(previous)
        {
            agent.active_model_id = Some(model.id);
        }
    } else {
        models.push(model);
    }
    w.save_model_catalog(candidate)?;
    w.provider_models
        .tests
        .retain(|(id, old_model, _), _| id != provider || previous != Some(old_model.as_str()));
    Ok(())
}

/// In the lower half of the agent page: Goose runtime settings (version/model/external path/three
/// logos) → A2A collaboration → Agent connection configuration (settings imported from the
/// self-setup page).
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
