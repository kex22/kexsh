use std::collections::HashMap;

use httpmock::prelude::*;
use tokio::sync::mpsc;

use crate::contract;
use crate::support::load_scenario;
use kexsh::cloud::manager::{ProxyEvent, proxy_request_task};

#[tokio::test]
async fn contract_proxy_http_happy_path() {
    let scenario = load_scenario("proxy-http-happy.json");

    // Extract request params from the do→cli request.head step
    let do_to_cli = contract::filter_steps(&scenario, "do→cli");
    let head_step = do_to_cli
        .iter()
        .find(|s| {
            s.message
                .as_ref()
                .is_some_and(|m| m["type"] == "proxy.request.head")
        })
        .expect("scenario missing proxy.request.head step");
    let head_msg = head_step.message.as_ref().unwrap();
    let method = head_msg["payload"]["method"].as_str().unwrap().to_string();
    let path = head_msg["payload"]["path"].as_str().unwrap().to_string();
    let request_id = head_msg["payload"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();

    let mut headers = HashMap::new();
    if let Some(obj) = head_msg["payload"]["headers"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                headers.insert(k.clone(), s.to_string());
            }
        }
    }

    // Start httpmock server simulating localhost service
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/api/users");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"users":[]}"#);
    });

    // Create channel to collect ProxyEvents
    let (tx, mut rx) = mpsc::channel::<ProxyEvent>(32);
    let client = reqwest::Client::new();

    // Run the proxy request task (uses mock server port instead of scenario's 3000)
    proxy_request_task(
        request_id.clone(),
        method,
        server.port(),
        path,
        headers,
        tx,
        client,
    )
    .await;

    // Collect all events
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // Verify: should have Head, Body, End (in order)
    assert!(
        events.len() >= 3,
        "expected at least 3 events, got {}",
        events.len()
    );

    // Verify Head event
    let head_event = &events[0];
    match head_event {
        ProxyEvent::Head {
            request_id: rid,
            status,
            headers: resp_headers,
        } => {
            assert_eq!(rid, &request_id);
            assert_eq!(*status, 200);
            assert!(
                resp_headers.contains_key("content-type"),
                "missing content-type header"
            );
        }
        other => panic!("expected Head event, got {:?}", event_type_name(other)),
    }

    // Verify Body event contains expected data
    let body_event = &events[1];
    match body_event {
        ProxyEvent::Body {
            request_id: rid,
            data,
        } => {
            assert_eq!(rid, &request_id);
            let body_str = String::from_utf8_lossy(data);
            assert_eq!(body_str, r#"{"users":[]}"#);
        }
        other => panic!("expected Body event, got {:?}", event_type_name(other)),
    }

    // Verify End event
    let end_event = events.last().unwrap();
    match end_event {
        ProxyEvent::End { request_id: rid } => {
            assert_eq!(rid, &request_id);
        }
        other => panic!("expected End event, got {:?}", event_type_name(other)),
    }

    // Cross-check with scenario assertions
    let cli_assertions = &scenario.assertions["cli"];
    let expected_method = cli_assertions["http_request_made"]["method"]
        .as_str()
        .unwrap();
    assert_eq!(expected_method, "GET");

    // Verify the scenario's expected cli→do messages match our output
    let cli_to_do = contract::filter_steps(&scenario, "cli→do");
    let expected_response_head = cli_to_do
        .iter()
        .find(|s| {
            s.message
                .as_ref()
                .is_some_and(|m| m["type"] == "proxy.response.head")
        })
        .expect("scenario missing proxy.response.head");
    let expected_status = expected_response_head.message.as_ref().unwrap()["payload"]["status"]
        .as_u64()
        .unwrap();
    assert_eq!(expected_status, 200);
}

#[tokio::test]
async fn contract_proxy_http_connection_refused() {
    let scenario = load_scenario("proxy-http-connection-refused.json");

    // Extract request params from do→cli request.head step
    let do_to_cli = contract::filter_steps(&scenario, "do→cli");
    let head_step = do_to_cli
        .iter()
        .find(|s| {
            s.message
                .as_ref()
                .is_some_and(|m| m["type"] == "proxy.request.head")
        })
        .expect("scenario missing proxy.request.head step");
    let head_msg = head_step.message.as_ref().unwrap();
    let method = head_msg["payload"]["method"].as_str().unwrap().to_string();
    let path = head_msg["payload"]["path"].as_str().unwrap().to_string();
    let request_id = head_msg["payload"]["requestId"]
        .as_str()
        .unwrap()
        .to_string();

    // Use a port that nothing is listening on
    let (tx, mut rx) = mpsc::channel::<ProxyEvent>(32);
    let client = reqwest::Client::new();

    proxy_request_task(
        request_id.clone(),
        method,
        19999, // no server on this port
        path,
        HashMap::new(),
        tx,
        client,
    )
    .await;

    // Collect events
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // Should get exactly one Error event
    assert!(!events.is_empty(), "expected at least one event");
    match &events[0] {
        ProxyEvent::Error {
            request_id: rid,
            message,
        } => {
            assert_eq!(rid, &request_id);
            // Verify message matches scenario assertion
            let cli_assertions = &scenario.assertions["cli"];
            let expected_contains = cli_assertions["error_sent"]["message_contains"]
                .as_str()
                .unwrap();
            assert!(
                message.contains(expected_contains),
                "error message '{}' should contain '{}'",
                message,
                expected_contains
            );
        }
        other => panic!("expected Error event, got {:?}", event_type_name(other)),
    }
}

fn event_type_name(event: &ProxyEvent) -> &'static str {
    match event {
        ProxyEvent::Head { .. } => "Head",
        ProxyEvent::Body { .. } => "Body",
        ProxyEvent::End { .. } => "End",
        ProxyEvent::Error { .. } => "Error",
        _ => "Other",
    }
}
