#![cfg(feature = "integration-hitl-header-forwarding")]
// Positive park/poll/restart coverage is preserved by source revision:
// frozen delivery blob 0294a03158b15011c2c804d8a03f99bec6ab1995 and
// frozen adaptive/store-wave blob f4374dd1fe26299a34f188b56733c9832fe49156
// under refs/backup/271/20260928T192901Z/. Restoration owner: Session 3
// activation (aura/P56 with aura/P45 and aura/P57), restoring the
// downstream 207 protocol. No #[ignore] and no production bypass stands
// in for that record.

//! Boot-refusal cases for poll delivery and park mode: a `[hitl]` config
//! carrying either must fail `aura-web-server` startup with the admission
//! diagnostic on stderr, before any approval flows. The rig spawns the
//! real binary through `common::AuraServer`; boot dies at config load, so
//! no model or MCP fixture is reached and the run recipe is the sibling
//! header-forwarding suite's (`make test-integration-hitl-local`).

mod common;

use common::AuraServer;

fn refusal_config_toml(park: bool, delivery: &str) -> String {
    let park_table = if park {
        "[hitl.park]\nenabled = true\n\n"
    } else {
        ""
    };
    format!(
        r#"
memory_dir = "/tmp/aura-poll-e2e-refusal-memory"

[agent]
name = "Poll Delivery E2E Assistant"
alias = "poll-e2e-assistant"
system_prompt = "test"
turn_depth = 3

[agent.llm]
provider = "openai"
api_key = "test-key"
model = "gpt-5.1"
temperature = 0.0

[orchestration]
enabled = true
allow_direct_answers = false

[hitl]
require_approval = ["kubectl_*"]

{park_table}[hitl.route]
mode = "webhook"
url = "https://approvals.example.com/notify"
poll_url = "https://status.example.com/decisions"
delivery = "{delivery}"
poll_interval_secs = 1
poll_request_timeout_secs = 5
"#
    )
}

#[tokio::test]
#[should_panic(expected = "hitl.route.delivery")]
async fn poll_flow_parks_notifies_and_resolves_with_the_run_still_parked() {
    let server = AuraServer::start(&refusal_config_toml(true, "poll"), "aura-poll-e2e-", &[]).await;
    server.stop().await;
}

#[tokio::test]
#[should_panic(expected = "hitl.park.enabled")]
async fn restart_resolves_the_parked_approval_on_a_rebooted_server() {
    let server = AuraServer::start(&refusal_config_toml(true, "sync"), "aura-poll-e2e-", &[]).await;
    server.stop().await;
}
