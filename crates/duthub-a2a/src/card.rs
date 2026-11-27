//! Minimal per-DUT Agent Card (ADR-8).
//!
//! The card is a self-description of the endpoint, never of the device it
//! serves: no DUT name/ID, no COM port, no filesystem path, no hostname, no
//! token. Serialization must stay <= [`MAX_CARD_BYTES`]; the crate e2e test
//! asserts the forbidden-key and size gates on the wire format.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, HttpAuthSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_JSONRPC,
};

/// Card `name` is a fixed product word, never the DUT name (ADR-8).
pub const CARD_NAME: &str = "duthub";

/// Security scheme referenced by `securityRequirements`; every JSON-RPC
/// method requires it (the HTTP layer enforces Bearer per request).
pub const CARD_SECURITY_SCHEME: &str = "duthub-bearer";

/// Hard serialization budget; a card above it is an implementation defect.
pub const MAX_CARD_BYTES: usize = 16 * 1024;

const CARD_DESCRIPTION: &str =
    "duthub local evidence endpoint for one DUT; read-only diagnostic skills over A2A JSON-RPC.";

const CARD_BEARER_DESCRIPTION: &str =
    "Per-DUT bearer credential issued by duthub; keep it out of exported files.";

/// Evidence skills advertised on the card, mirroring the server-side
/// allowlist ([`crate::evidence::EVIDENCE_SKILLS`]). Descriptions stay generic
/// so no DUT context leaks into the card.
const CARD_SKILLS: [(&str, &str); 5] = [
    (
        "dut_list",
        "List the DUT served by this endpoint with its connection status.",
    ),
    (
        "dut_status",
        "Read the bound DUT's connection, relay capabilities, lease and heartbeat status.",
    ),
    (
        "serial_read",
        "Read recent serial evidence lines from the bound DUT session.",
    ),
    (
        "log_segments",
        "List archived serial log segments of the bound DUT.",
    ),
    (
        "log_segment_read",
        "Read one archived serial log segment of the bound DUT.",
    ),
];

/// Build the card for a listener bound to `127.0.0.1:<port>`.
pub fn build_agent_card(port: u16) -> AgentCard {
    build_agent_card_at(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Build the card for a listener bound to `<addr>:<port>`. Non-loopback
/// callers are rejected upstream (ADR-2); the card itself stays descriptive
/// of the actual bound address, which is always a loopback literal here.
pub fn build_agent_card_at(addr: IpAddr, port: u16) -> AgentCard {
    let mut security_schemes = HashMap::new();
    security_schemes.insert(
        CARD_SECURITY_SCHEME.to_owned(),
        SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "bearer".to_owned(),
            description: Some(CARD_BEARER_DESCRIPTION.to_owned()),
            bearer_format: Some("opaque-token".to_owned()),
        }),
    );

    let skills = CARD_SKILLS
        .iter()
        .map(|(id, description)| AgentSkill {
            id: (*id).to_owned(),
            name: (*id).to_owned(),
            description: (*description).to_owned(),
            tags: vec!["duthub".to_owned(), "evidence".to_owned()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        })
        .collect();

    AgentCard {
        name: CARD_NAME.to_owned(),
        description: CARD_DESCRIPTION.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        supported_interfaces: vec![AgentInterface::new(
            format!("http://{addr}:{port}"),
            TRANSPORT_PROTOCOL_JSONRPC,
        )],
        capabilities: AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: Some(false),
        },
        default_input_modes: vec!["application/json".to_owned()],
        default_output_modes: vec!["application/json".to_owned()],
        skills,
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: Some(security_schemes),
        security_requirements: Some(vec![HashMap::from([(
            CARD_SECURITY_SCHEME.to_owned(),
            Vec::<String>::new(),
        )])]),
        signatures: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_is_small_and_loopback_only() {
        let card = build_agent_card(8790);
        let json = serde_json::to_vec(&card).unwrap();
        assert!(json.len() <= MAX_CARD_BYTES, "card is {} bytes", json.len());
        let text = String::from_utf8(json).unwrap();
        assert!(text.contains("http://127.0.0.1:8790"));
        assert!(text.contains("JSONRPC"));
        assert!(!text.contains("http://0.0.0.0"));
    }

    #[test]
    fn card_name_is_fixed_product_word() {
        let card = build_agent_card(1);
        assert_eq!(card.name, CARD_NAME);
    }
}
