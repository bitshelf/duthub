use std::collections::HashMap;
use std::sync::Arc;

use duthub_core::model::{ModelProvider, ProviderModel};
use duthub_core::{Provider, TestRequest, TestResponse, model_provider_credential_id};
use gpui_kit::component::Disableable;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::{AppContext, Context, Div, Entity, ParentElement, Styled, Window, div, px};

use crate::components::{BtnVariant, btn, field, panel_head, subtle};
use crate::i18n::label;
use crate::theme::Palette;
use crate::workspace::Workbench;

#[derive(Default)]
pub(crate) struct Catalog {
    draft: Option<Draft>,
    legacy_selected: bool,
    notice: Option<String>,
    tests: HashMap<(String, String, Probe), ProbeState>,
}

impl Catalog {
    pub(crate) fn close_editor(&mut self) {
        self.draft = None;
        self.legacy_selected = false;
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
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Probe {
    Api,
    Model,
}

enum ProbeState {
    Running(uuid::Uuid),
    Finished(Result<TestResponse, String>),
}

fn credential(provider: &ModelProvider) -> String {
    model_provider_credential_id(&provider.id, provider.protocol, &provider.base_url)
}

fn open_form(
    w: &mut Workbench,
    saved: Option<ModelProvider>,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) {
    let input = |value: &str, window: &mut Window, cx: &mut Context<Workbench>| {
        cx.new(|cx| InputState::new(window, cx).default_value(value.to_owned()))
    };
    let draft = Draft {
        id: saved.as_ref().map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |provider| provider.id.clone(),
        ),
        name: input(saved.as_ref().map_or("", |value| &value.name), window, cx),
        base: input(
            saved.as_ref().map_or("", |value| &value.base_url),
            window,
            cx,
        ),
        key: cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(label("agentModelTokenPlaceholder"))
        }),
        model: cx.new(|cx| InputState::new(window, cx).placeholder(label("enterModelName"))),
        protocol: saved.map_or(Provider::Compatible, |provider| provider.protocol),
    };
    w.provider_models.draft = Some(draft);
    w.provider_models.legacy_selected = false;
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
    let base_url = duthub_core::validate_model_base(draft.base.read(cx).value().as_ref())?;
    let key = draft.key.read(cx).value().trim().to_owned();
    let protocol = draft.protocol;
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
            models: Vec::new(),
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
        || matches!(
            w.provider_models.tests.get(&slot),
            Some(ProbeState::Running(_))
        )
    {
        return;
    }
    let api_key = match duthub_core::get_key(&credential(provider)) {
        Ok(key) => key.unwrap_or_default(),
        Err(error) => {
            w.provider_models
                .tests
                .insert(slot, ProbeState::Finished(Err(error.to_string())));
            cx.notify();
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
    w.provider_models
        .tests
        .insert(slot.clone(), ProbeState::Running(request));
    cx.notify();
    let worker = cx.background_executor().spawn(async move {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        runtime.block_on(async {
            match probe {
                Probe::Api => duthub_core::test_model_api(&task).await,
                Probe::Model => duthub_core::test_model_connection(&task).await,
            }
        })
    });
    cx.spawn(async move |view, cx| {
        let result = worker.await;
        let _ = view.update(cx, |w, cx| {
            if matches!(w.provider_models.tests.get(&slot), Some(ProbeState::Running(active)) if *active == request) {
                w.provider_models.tests.insert(slot, ProbeState::Finished(result));
                cx.notify();
            }
        });
    }).detach();
}

pub(crate) fn render(
    w: &mut Workbench,
    p: &'static Palette,
    window: &mut Window,
    cx: &mut Context<Workbench>,
) -> Div {
    if !w.provider_models.legacy_selected
        && w.provider_models.draft.is_none()
        && let Some(first) = w.settings.agent.providers.first().cloned()
    {
        open_form(w, Some(first), window, cx);
    }
    let mut nav = div().flex().flex_wrap().gap(px(8.));
    if !w.settings.agent.providers.is_empty() && !w.settings.agent.slow.trim().is_empty() {
        nav = nav.child(
            btn(
                "model-provider-legacy",
                label("legacyAgentModel"),
                if w.provider_models.legacy_selected {
                    &BtnVariant::TinyPrimary
                } else {
                    &BtnVariant::Tiny
                },
                p,
            )
            .on_click(cx.listener(|w, _, _, cx| {
                w.provider_models.close_editor();
                w.provider_models.legacy_selected = true;
                cx.notify();
            })),
        );
    }
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
                open_form(w, Some(provider.clone()), window, cx);
                cx.notify();
            })),
        );
    }
    nav = nav.child(
        btn(
            "add-model-provider",
            label("addModelProvider"),
            &BtnVariant::Tiny,
            p,
        )
        .on_click(cx.listener(|w, _, window, cx| {
            open_form(w, None, window, cx);
            cx.notify();
        })),
    );
    let mut page = div()
        .max_w(px(820.))
        .w_full()
        .mx_auto()
        .p(px(24.))
        .child(nav);
    if w.provider_models.draft.is_none() {
        return page.child(crate::agent::render_model_settings(w, p, window, cx));
    }
    let draft = w.provider_models.draft.as_ref().expect("provider editor");
    let id = draft.id.clone();
    let protocol = draft.protocol;
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
        (Provider::Anthropic, "Anthropic Messages (/v1/messages)"),
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
    let mut body = div()
        .p(px(16.))
        .child(field(
            label("providerName"),
            Input::new(&name).id("model-provider-name"),
            None,
            p,
        ))
        .child(field(
            "Base URL",
            Input::new(&base).id("model-provider-base"),
            None,
            p,
        ))
        .child(field(label("providerApiFormat"), chooser, None, p))
        .child(field(
            "API Key",
            Input::new(&key).id("model-provider-key"),
            Some(label("agentModelTokenStorage")),
            p,
        ))
        .child(
            btn(
                "save-model-provider",
                label("saveModelProvider"),
                &BtnVariant::TinyPrimary,
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
        );
    if let Some(provider) = saved {
        let add_id = provider.id.clone();
        let add_input = model.clone();
        let mut list = div()
            .mt(px(16.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(subtle(label("providerModelList"), p))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&model).id("provider-model-id")),
                    )
                    .child(
                        btn(
                            "add-provider-model",
                            label("addProviderModel"),
                            &BtnVariant::Tiny,
                            p,
                        )
                        .on_click(cx.listener(move |w, _, window, cx| {
                            let value = add_input.read(cx).value().trim().to_owned();
                            let result = change_provider(w, &add_id, |provider| {
                                if value.is_empty() {
                                    return Err(label("enterModelName").into());
                                }
                                if provider.models.iter().any(|model| model.id == value) {
                                    return Err(label("duplicateProviderModel").into());
                                }
                                provider.models.push(ProviderModel {
                                    id: value,
                                    enabled: true,
                                });
                                Ok(())
                            });
                            if result.is_ok() {
                                add_input.update(cx, |input, cx| input.set_value("", window, cx));
                            }
                            w.provider_models.notice = result.err();
                            cx.notify();
                        })),
                    ),
            );
        for (index, model) in provider.models.iter().enumerate() {
            let id = provider.id.clone();
            let model_id = model.id.clone();
            let mut actions = div().flex().flex_wrap().items_center().gap(px(6.));
            let mut statuses = div().flex().flex_col().gap(px(4.));
            for (probe, action, name) in [
                (Probe::Api, "api", label("testProviderApi")),
                (Probe::Model, "model", label("testProviderModel")),
            ] {
                let state = w
                    .provider_models
                    .tests
                    .get(&(id.clone(), model_id.clone(), probe));
                let running = matches!(state, Some(ProbeState::Running(_)));
                let id = id.clone();
                let model_id = model_id.clone();
                actions = actions.child(
                    btn(
                        format!("provider-{action}-test-{index}"),
                        name,
                        &BtnVariant::Tiny,
                        p,
                    )
                    .disabled(running || !provider.enabled || !model.enabled)
                    .on_click(cx.listener(move |w, _, _, cx| {
                        start_probe(w, &id, &model_id, probe, cx);
                        cx.notify();
                    })),
                );
                if let Some(state) = state {
                    let detail = match state {
                        ProbeState::Running(_) => label("providerTestRunning").to_owned(),
                        ProbeState::Finished(Ok(response)) if response.reply.is_empty() => format!(
                            "{} · {} ms",
                            label("providerTestPassed"),
                            response.latency_ms
                        ),
                        ProbeState::Finished(Ok(response)) => {
                            format!("{} ms · {}", response.latency_ms, response.reply)
                        }
                        ProbeState::Finished(Err(error)) => error.clone(),
                    };
                    statuses = statuses.child(subtle(&format!("{name} · {detail}"), p));
                }
            }
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
            list = list.child(
                div()
                    .p(px(9.))
                    .bg(p.raised)
                    .rounded(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .child(model.id.clone())
                            .child(actions),
                    )
                    .child(statuses),
            );
        }
        body = body.child(list);
    }
    if let Some(notice) = &w.provider_models.notice {
        body = body.child(subtle(notice, p).mt(px(8.)));
    }
    page = page.child(
        div()
            .mt(px(12.))
            .border_1()
            .border_color(p.border)
            .rounded(px(8.))
            .bg(p.surface)
            .child(head)
            .child(body),
    );
    page
}
