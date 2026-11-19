//! HDMI monitor float window and the shared frame pump
//! (`docs/design/CAPTURE_STREAM_BACKENDS.md` §6.3/§6.4).
//!
//! `FramePump` is the **only** consumer of `InlinePreview::poll`. It lives on
//! `Workbench` and is driven by a 100ms `cx.spawn` timer, so the preview keeps
//! producing frames while the main window is minimized or another side panel
//! is visible. The side panel and the float window both render
//! `FramePump::frame()` clones.
//!
//! Every frame swap and every clear drops the replaced image with
//! `App::drop_image` (all windows), never with `Window::drop_image` alone:
//! once a float window exists it has its own sprite atlas, so a window-local
//! drop would leak the texture in the other window.
//!
//! The float window is a normal window (`WindowKind::Normal`, `focus: false`)
//! because `PopUp` is always-on-top and borderless and `Floating` has no
//! Windows-specific behavior (§6.4). The system titlebar is hidden
//! (`appears_transparent`), so the window carries its own narrow icon strip:
//! the whole strip is a `WindowControlArea::Drag` region (drag + double-click
//! maximize) and each icon button is `.occlude()`d (pit #1: the drag area
//! would otherwise swallow the click). Buttons are hand-drawn divs (pit #2:
//! kit buttons inside floating layers render as blank blocks).

use std::sync::Arc;

use gpui_kit::{
    AnyWindowHandle, Context, Div, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    Render, RenderImage, Stateful, StatefulInteractiveElement, Styled, StyledImage, WeakEntity,
    Window, WindowControlArea, div, img, px,
};

use crate::media::{InlinePreview, Source};
use crate::theme::Palette;
use crate::workspace::Workbench;
use duthub_desktop::capture_ui::MonitorPlacement;

/// The single owner of the monitor preview and its decoded frame (§6.3).
pub(crate) struct FramePump {
    preview: Option<InlinePreview>,
    frame: Option<Arc<RenderImage>>,
    last_error: Option<String>,
    key: Option<(String, Source)>,
    /// Mirrors [`MonitorPlacement`] so `release_monitor` can refuse while the
    /// preview is owned by the float window.
    placement: MonitorPlacement,
    /// Float view that mirrors frames; weak so a user close cannot keep the
    /// entity alive after its window is gone.
    float: Option<WeakEntity<FloatMonitor>>,
    /// Set by `tick` when the preview reported a failure; the Workbench
    /// resolves the detail (`media.capture_error`) and drives `PreviewFailed`.
    failed: bool,
}

impl FramePump {
    pub(crate) fn new() -> Self {
        Self {
            preview: None,
            frame: None,
            last_error: None,
            key: None,
            placement: MonitorPlacement::Docked,
            float: None,
            failed: false,
        }
    }

    /// Take ownership of a freshly opened preview; the previous frame is
    /// dropped from every window atlas (`window` = the current one when the
    /// caller is inside a window update).
    pub(crate) fn attach(
        &mut self,
        cx: &mut Context<Workbench>,
        window: Option<&mut Window>,
        key: (String, Source),
        preview: InlinePreview,
    ) {
        self.drop_frame(cx, window);
        self.preview = Some(preview);
        self.key = Some(key);
        self.last_error = None;
        self.failed = false;
    }

    /// Poll once and mirror the new frame into the float window.
    ///
    /// Called only by the 100ms pump timer — never from a render path — so the
    /// preview cannot be consumed twice and keeps running while the main
    /// window is minimized.
    pub(crate) fn tick(&mut self, cx: &mut Context<Workbench>) -> bool {
        let Some(preview) = self.preview.as_mut() else {
            return false;
        };
        // Nothing new since the last tick: skip the lock/take round trip.
        if !preview.has_update() {
            return false;
        }
        let (next, error) = preview.poll();
        let mut changed = false;
        if let Some(frame) = next {
            if let Some(old) = self.frame.replace(frame.clone()) {
                cx.drop_image(old, None);
            }
            self.push_to_float(cx, frame);
            changed = true;
        }
        if let Some(error) = error {
            self.last_error = Some(error);
            self.failed = true;
            changed = true;
        }
        changed
    }

    fn push_to_float(&self, cx: &mut Context<Workbench>, frame: Arc<RenderImage>) {
        let Some(float) = &self.float else {
            return;
        };
        let _ = float.update(cx, |view, cx| {
            view.set_frame(frame);
            cx.notify();
        });
    }

    pub(crate) fn frame(&self) -> Option<Arc<RenderImage>> {
        self.frame.clone()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub(crate) fn set_error(&mut self, error: Option<String>) {
        self.last_error = error;
    }

    /// Raw preview error once per failure; the caller translates it.
    pub(crate) fn take_failure(&mut self) -> Option<String> {
        if !self.failed {
            return None;
        }
        self.failed = false;
        Some(self.last_error.clone().unwrap_or_default())
    }

    pub(crate) fn key(&self) -> Option<&(String, Source)> {
        self.key.as_ref()
    }

    pub(crate) fn placement(&self) -> MonitorPlacement {
        self.placement
    }

    pub(crate) fn set_placement(&mut self, placement: MonitorPlacement) {
        self.placement = placement;
    }

    pub(crate) fn set_float(&mut self, float: Option<WeakEntity<FloatMonitor>>) {
        self.float = float;
    }

    /// Release the preview and its frame. The caller decides whether the
    /// `Monitor` lease is released (`placement == Floating` forbids it) and
    /// passes the current window when it is inside a window update.
    pub(crate) fn clear(&mut self, cx: &mut Context<Workbench>, window: Option<&mut Window>) {
        self.preview = None;
        self.key = None;
        self.last_error = None;
        self.failed = false;
        self.drop_frame(cx, window);
    }

    /// App-level drop (all windows + the current one when given): a window-local
    /// `Window::drop_image` would leak the texture in the float window's atlas.
    fn drop_frame(&mut self, cx: &mut Context<Workbench>, window: Option<&mut Window>) {
        if let Some(frame) = self.frame.take() {
            cx.drop_image(frame, window);
        }
    }
}

/// Handle plus view of the one float window that may exist (§6.4).
pub(crate) struct FloatWindow {
    pub handle: AnyWindowHandle,
}

/// Content of the HDMI float window: icon toolbar + frame area.
pub(crate) struct FloatMonitor {
    frame: Option<Arc<RenderImage>>,
    p: &'static Palette,
    /// Redock / fullscreen / close are routed back to the `Workbench`; weak so
    /// the window can outlive a panic-free teardown.
    workbench: WeakEntity<Workbench>,
}

impl FloatMonitor {
    pub(crate) fn new(
        frame: Option<Arc<RenderImage>>,
        p: &'static Palette,
        workbench: WeakEntity<Workbench>,
    ) -> Self {
        Self {
            frame,
            p,
            workbench,
        }
    }

    fn set_frame(&mut self, frame: Arc<RenderImage>) {
        self.frame = Some(frame);
    }

    /// "还原": hand the preview back to the right side panel, focus the main
    /// window and close this one. The Workbench clears its float bookkeeping
    /// first so the close observer treats it as a programmatic close.
    fn redock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(workbench) = self.workbench.upgrade() {
            workbench.update(cx, |workbench, cx| workbench.prepare_redock(cx));
        }
        window.remove_window();
    }

    /// The toolbar X behaves exactly like the platform close button: the
    /// window goes away and `on_window_closed` drives `UserClosedWindow`.
    fn close(&mut self, window: &mut Window) {
        window.remove_window();
    }
}

/// Hand-drawn 26×26 icon button for the float toolbar (pit #2: kit buttons
/// inside floating layers render as blank blocks). The interactive cluster is
/// occluded so the surrounding drag area does not swallow the click (pit #1).
fn float_icon_button(
    id: &'static str,
    icon_name: &'static str,
    p: &'static Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .w(px(26.))
        .h(px(26.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|style| style.bg(p.hover))
        .child(
            crate::icons::icon(icon_name)
                .size(px(15.))
                .text_color(p.muted),
        )
}

impl Render for FloatMonitor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.p;
        // The toggle icon mirrors the platform state after every fullscreen
        // switch (`cx.notify` below re-renders it).
        let fullscreen_icon = if window.is_fullscreen() {
            "shrink"
        } else {
            "maximize-2"
        };
        // Narrow icon strip: the whole bar drags the window (and double-click
        // maximizes it); the buttons sit right-aligned and are occluded (pit #1).
        let toolbar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_end()
            .gap(px(4.))
            .px(px(8.))
            .h(px(30.))
            .bg(p.surface)
            .window_control_area(WindowControlArea::Drag)
            .child(
                float_icon_button("float-redock", "panel-right", p)
                    .on_click(cx.listener(|this, _, window, cx| this.redock(window, cx))),
            )
            .child(
                float_icon_button("float-fullscreen", fullscreen_icon, p).on_click(cx.listener(
                    |_, _, window, cx| {
                        window.toggle_fullscreen();
                        cx.notify();
                    },
                )),
            )
            .child(
                float_icon_button("float-close", "x", p)
                    .on_click(cx.listener(|this, _, window, _cx| this.close(window))),
            );
        let viewport = if let Some(frame) = &self.frame {
            div().relative().flex_1().min_h_0().bg(p.term).child(
                img(frame.clone())
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            )
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(p.term)
                .text_size(px(12.))
                .text_color(p.muted)
                .child(crate::i18n::label("waitingForVideo"))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.bg)
            .child(toolbar)
            .child(viewport)
    }
}
