//! Desktop UI pages decomposed by functional domain:
//! - `connection`: per-DUT connection configuration (serial, logs, heartbeat, USB debug, MCP, A2A, flash)
//! - `relay`: shared relay boards and per-DUT channel mapping
//! - `agent_panel`: per-DUT Agent session, inspector, runtime, registry and env keyring editor
//! - `settings`: global settings, tool downloads, proxy, alerts, workspace dir, Agent access, A2A peers
//! - `serial_settings`: serial prompt highlight rules, keyboard, mouse, and scrollback settings
//! - `tools`: flash tool presets and USB debug device discovery

mod agent_panel;
mod connection;
mod relay;
mod serial_settings;
mod settings;
mod tools;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use duthub_a2a::listener::A2aStatus;
use duthub_core::model::{RelayChannels, RelayConfig};
use duthub_core::{DutAction, LeasePhase};
use duthub_desktop::agent_host::bundle;
use duthub_desktop::agent_host::session::{SessionStatus, TranscriptEntry, TranscriptKind};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext, Context, Div, InteractiveElement, IntoElement, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, div, px,
};

use crate::components::{
    BtnVariant, btn, chip, copy_text, field, icon_btn, panel_head, subtle, toggle,
};
use crate::icons::icon;
use crate::theme::Palette;
use crate::workspace::{FlashPresetTest, In, Workbench};

pub const BAUD_OPTIONS: [&str; 20] = [
    "110", "300", "600", "1200", "2400", "4800", "9600", "14400", "19200", "38640", "38400",
    "56000", "57600", "115200", "128000", "230400", "256000", "460800", "921600", "1500000",
];

// External exports used by workspace/components/side_panel.
pub(crate) use agent_panel::{agent_inspector_panel, agent_runtime_panel};
pub use connection::render_connection;
pub(crate) use connection::{ConnectionPanel, collapsible_panel, panel_shell, render_about};
pub(crate) use relay::render_relay_boards;
pub(crate) use serial_settings::prompt_rule_description;
pub use serial_settings::{hex_to_rgba, render_serial_settings};
pub(crate) use settings::{a2a_panel, agent_access_panel};
pub use settings::{render_settings, render_tool_downloads};
pub use tools::{render_flash_tools, render_usb_debug};

// Cross-submodule helpers shared inside `pages::*` via `use super::*`.
use agent_panel::connection_agent_panel;
#[cfg(test)]
use connection::usb_debug_display;
use relay::relay_panel;
use settings::{
    alerts_panel, connection_lease_action, detected_lan_ip, setting_row, settings_panel,
    settings_panel_with_header,
};
