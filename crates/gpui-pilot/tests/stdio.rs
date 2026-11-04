//! The server over stdio without an app: the tool list and every failure that needs no host.

mod common;

use common::{Pilot, text};
use serde_json::json;

#[tokio::test]
async fn lists_the_seventeen_tools_with_read_only_hints() {
    let pilot = Pilot::start(None).await;
    assert_eq!(
        pilot.tools().await,
        [
            "click",
            "clipboard",
            "close",
            "dialog",
            "drag",
            "drop",
            "frames",
            "hover",
            "key",
            "launch",
            "pick_file",
            "resize",
            "screenshot",
            "scroll",
            "tree",
            "type",
            "wait"
        ]
    );
    let tools = pilot.client.list_all_tools().await.unwrap();
    for tool in tools
        .iter()
        .filter(|t| t.name == "screenshot" || t.name == "tree" || t.name == "wait")
    {
        let hint = tool.annotations.as_ref().and_then(|a| a.read_only_hint);
        assert_eq!(hint, Some(true), "{}", tool.name);
    }
}

#[tokio::test]
async fn failures_are_tool_results_with_a_code_and_a_one_line_summary() {
    let pilot = Pilot::start(None).await;
    let result = pilot
        .try_call("click", json!({"target": {"id": ["count"]}}))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["code"],
        json!("NotRunning")
    );
    assert!(text(&result).starts_with("NotRunning: ") && !text(&result).contains('\n'));
    assert_eq!(
        pilot.err("launch", json!({"mode": "headless"})).await["code"],
        json!("InvalidConfig")
    );
    let unsupported = pilot
        .err("launch", json!({"mode": "headless", "network": "pasta"}))
        .await;
    assert_eq!(
        (
            &unsupported["code"],
            &unsupported["mode"],
            &unsupported["tool"]
        ),
        (&json!("Unsupported"), &json!("headless"), &json!("launch"))
    );
    assert_eq!(
        pilot.err("close", json!({})).await["code"],
        json!("NotRunning")
    );
    assert_eq!(
        pilot.err("frames", json!({"action": "stop"})).await["code"],
        json!("NotRunning")
    );
}

#[tokio::test]
async fn malformed_parameters_are_unstructured_error_results() {
    let pilot = Pilot::start(None).await;
    for (tool, args) in [
        ("click", json!({})),
        ("click", json!({"target": {"id": ["a"], "x": 1, "y": 2}})),
        ("launch", json!({"mode": "remote"})),
        ("pick_file", json!({})),
        ("pick_file", json!({"paths": ["a"], "cancel": true})),
    ] {
        let result = pilot.try_call(tool, args.clone()).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{tool} {args}");
        assert_eq!(result.structured_content, None, "{tool} {args}");
        assert!(!text(&result).is_empty());
    }
    let shaped = pilot
        .try_call("click", json!({"target": {"point": [1, 2]}}))
        .await
        .unwrap();
    assert_eq!(shaped.is_error, Some(true));
    let message = text(&shaped);
    assert!(
        message.contains(r#"{"x": number, "y": number}"#)
            && message.contains("id_suffix")
            && !message.contains("untagged"),
        "{message}"
    );
    assert!(pilot.try_call("no_such_tool", json!({})).await.is_err());
}

#[tokio::test]
async fn launch_refuses_bad_settle_and_env_before_reading_the_config() {
    let pilot = Pilot::start(None).await;
    let slow = pilot
        .err("launch", json!({"mode": "headless", "settle_ms": 60_000}))
        .await;
    assert_eq!(slow["code"], json!("InvalidArgument"));
    let home = pilot
        .err("launch", json!({"mode": "headless", "env": {"HOME": "/x"}}))
        .await;
    assert_eq!(home["code"], json!("InvalidArgument"));
}

#[test]
fn an_unknown_command_exits_with_usage() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_gpui-pilot"))
        .arg("bogus")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}
