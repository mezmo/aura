//! Positive smoke test for `#[derive(Event)]`. The negative side
//! (forbidden field types fail to compile) lives in
//! `tests/compile_fail.rs` and `tests/compile_fail/*.rs`.

use aura_telemetry::{
    properties::{DeploymentMethod, OsFamily, Source},
    Event,
};

#[derive(Event)]
#[aura_event(name = "server_started")]
struct ServerStarted {
    aura_source: Source,
    os_family: OsFamily,
    deployment_method: DeploymentMethod,
    default_agent_set: bool,
}

#[test]
fn payload_carries_event_name_and_properties() {
    let event = ServerStarted {
        aura_source: Source::WebServer,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::Local,
        default_agent_set: true,
    };

    let payload = event.into_payload();

    assert_eq!(payload.name, "server_started");
    assert_eq!(<ServerStarted as Event>::NAME, "server_started");

    let map: std::collections::HashMap<_, _> = payload
        .properties
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_json()))
        .collect();

    assert_eq!(map["aura_source"], serde_json::json!("web-server"));
    assert_eq!(map["os_family"], serde_json::json!("linux"));
    assert_eq!(map["deployment_method"], serde_json::json!("local"));
    assert_eq!(map["default_agent_set"], serde_json::json!(true));
}
