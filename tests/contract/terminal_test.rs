use base64::Engine;

use crate::contract;
use crate::support::load_scenario;
use kexsh::cloud::manager::{encode_binary_frame, envelope};

#[test]
fn contract_terminal_sync_message_format() {
    let scenario = load_scenario("terminal-sync-happy.json");

    // Verify cli→do sync message matches envelope format
    let cli_to_do = contract::filter_steps(&scenario, "cli→do");
    let sync_step = cli_to_do
        .iter()
        .find(|s| {
            s.message
                .as_ref()
                .is_some_and(|m| m["type"] == "terminal.sync")
        })
        .expect("scenario missing terminal.sync step");

    let sync_msg = sync_step.message.as_ref().unwrap();
    let terminal_id = sync_msg["payload"]["terminalId"].as_str().unwrap();
    let name = sync_msg["payload"]["name"].as_str().unwrap();

    // Build the same message using CLI's envelope function
    let actual = envelope(
        "terminal.sync",
        serde_json::json!({ "terminalId": terminal_id, "name": name }),
    );
    let actual_val: serde_json::Value = serde_json::from_str(&actual).unwrap();

    contract::assert_message_matches(&actual_val, sync_msg);
}

#[test]
fn contract_terminal_sync_binary_frame_encoding() {
    let scenario = load_scenario("terminal-sync-happy.json");

    // Find the cli→do binary frame step
    let cli_to_do = contract::filter_steps(&scenario, "cli→do");
    let binary_step = cli_to_do
        .iter()
        .find(|s| s.binary.is_some())
        .expect("scenario missing cli→do binary step");

    let bin = binary_step.binary.as_ref().unwrap();

    // Parse scenario values
    let frame_type = u8::from_str_radix(bin.frame_type.trim_start_matches("0x"), 16).unwrap();
    let expected_payload = base64::engine::general_purpose::STANDARD
        .decode(&bin.payload_base64)
        .unwrap();

    // Build frame using CLI's encode function
    let frame = encode_binary_frame(&bin.id, frame_type, &expected_payload);

    // Verify frame structure: [1B type][8B id][payload]
    assert_eq!(frame[0], frame_type);
    assert_eq!(
        std::str::from_utf8(&frame[1..9])
            .unwrap()
            .trim_end_matches('\0'),
        bin.id
    );
    assert_eq!(&frame[9..], &expected_payload);
}

#[test]
fn contract_terminal_sync_ok_response_format() {
    let scenario = load_scenario("terminal-sync-happy.json");

    // Verify do→cli sync.ok message format
    let do_to_cli = contract::filter_steps(&scenario, "do→cli");
    let sync_ok_step = do_to_cli
        .iter()
        .find(|s| {
            s.message
                .as_ref()
                .is_some_and(|m| m["type"] == "terminal.sync.ok")
        })
        .expect("scenario missing terminal.sync.ok step");

    let sync_ok_msg = sync_ok_step.message.as_ref().unwrap();

    // Verify the expected response has correct structure
    assert_eq!(sync_ok_msg["v"], 1);
    assert_eq!(sync_ok_msg["type"], "terminal.sync.ok");
    assert!(
        sync_ok_msg["payload"]["terminalId"].is_string(),
        "sync.ok must have terminalId"
    );
}

#[test]
fn contract_terminal_sync_assertions_match() {
    let scenario = load_scenario("terminal-sync-happy.json");

    // Verify CLI-side assertion: terminal_synced
    let cli_assertions = &scenario.assertions["cli"];
    let synced = &cli_assertions["terminal_synced"];
    assert!(
        synced["id"].is_string(),
        "assertion must specify terminal id"
    );

    // Verify D.O.-side assertion: terminal_output_received
    let do_assertions = &scenario.assertions["do"];
    let output = &do_assertions["terminal_output_received"];
    assert!(output["id"].is_string());
    assert!(output["payload_base64"].is_string());

    // Decode and verify the expected output payload
    let expected_b64 = output["payload_base64"].as_str().unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(expected_b64)
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&decoded),
        "hello world",
        "scenario payload should decode to 'hello world'"
    );
}
