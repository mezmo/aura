//! Positive smoke test for `#[derive(Event)]`. The negative side
//! (forbidden field types fail to compile) lives in
//! `tests/compile_fail.rs` and `tests/compile_fail/*.rs`.

use aura_telemetry::{
    properties::{DeploymentMethod, OsFamily, Source},
    Event,
};

#[derive(Event)]
#[aura_event(name = "smoke_event")]
struct SmokeEvent {
    aura_source: Source,
    os_family: OsFamily,
    deployment_method: DeploymentMethod,
    interactive: bool,
}

#[test]
fn payload_carries_event_name_and_properties() {
    let event = SmokeEvent {
        aura_source: Source::Cli,
        os_family: OsFamily::Linux,
        deployment_method: DeploymentMethod::StandaloneCli,
        interactive: true,
    };

    let payload = event.into_payload();

    assert_eq!(payload.name, "smoke_event");
    assert_eq!(<SmokeEvent as Event>::NAME, "smoke_event");

    let map: std::collections::HashMap<_, _> = payload
        .properties
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_json()))
        .collect();

    assert_eq!(map["aura_source"], serde_json::json!("cli"));
    assert_eq!(map["os_family"], serde_json::json!("linux"));
    assert_eq!(
        map["deployment_method"],
        serde_json::json!("standalone-cli")
    );
    assert_eq!(map["interactive"], serde_json::json!(true));
}
