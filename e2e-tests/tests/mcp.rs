//! The Model Context Protocol endpoint at `/mcp`, with read-only tools for AI agents.
//!
//! Binary has to be built with `unstable-mcp`.

#![cfg(feature = "test-mcp")]

use martin_e2e_tests::{Martin, TestResponse, mbtiles_fixture};
use serde_json::{Value, json};

/// MCP clients accept both a plain JSON answer and an event stream.
const ACCEPT: (&str, &str) = ("accept", "application/json, text/event-stream");

async fn martin_with_cities(dir: &tempfile::TempDir) -> Martin {
    let cities = mbtiles_fixture(dir.path(), "world_cities").await;
    Martin::builder()
        .arg("--mcp")
        .arg(&cities)
        .start()
        .await
        .expect("failed to start martin")
}

async fn post(martin: &Martin, method: &str, params: &Value) -> TestResponse {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    martin
        .post_json_with_headers("/mcp", &[ACCEPT], request.to_string().as_bytes())
        .await
}

/// Calls a tool and returns the text it answers with.
async fn call_tool(martin: &Martin, name: &str, arguments: &Value) -> String {
    let response = post(
        martin,
        "tools/call",
        &json!({"name": name, "arguments": arguments}),
    )
    .await;
    assert_eq!(response.status(), 200, "{name} failed: {}", response.text());
    let result = &response.json()["result"];
    assert_eq!(
        result["isError"],
        json!(false),
        "{name} answered an error: {result}"
    );
    result["content"][0]["text"]
        .as_str()
        .expect("the tool must answer with text")
        .to_owned()
}

#[tokio::test]
async fn is_off_unless_enabled() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let cities = mbtiles_fixture(dir.path(), "world_cities").await;
    let mut martin = Martin::builder()
        .arg(&cities)
        .start()
        .await
        .expect("failed to start martin");
    assert_eq!(post(&martin, "tools/list", &json!({})).await.status(), 404);
    martin.stop().await;
}

#[tokio::test]
async fn refuses_requests_for_other_hosts() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut martin = martin_with_cities(&dir).await;
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
    let response = martin
        .post_json_with_headers(
            "/mcp",
            &[ACCEPT, ("host", "example.com")],
            request.to_string().as_bytes(),
        )
        .await;
    assert_eq!(response.status(), 403);
    martin.stop().await;
}

#[tokio::test]
async fn lists_its_tools() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut martin = martin_with_cities(&dir).await;
    let response = post(&martin, "tools/list", &json!({})).await;
    assert_eq!(response.status(), 200, "{}", response.text());
    insta::assert_json_snapshot!(response.json()["result"]);
    martin.stop().await;
}

#[tokio::test]
async fn lists_the_sources() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut martin = martin_with_cities(&dir).await;
    insta::assert_snapshot!(call_tool(&martin, "list_sources", &json!({})).await);
    martin.stop().await;
}

#[tokio::test]
async fn describes_a_source() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut martin = martin_with_cities(&dir).await;
    insta::assert_snapshot!(
        call_tool(
            &martin,
            "describe_source",
            &json!({"source": "world_cities"})
        )
        .await
    );
    martin.stop().await;
}

#[tokio::test]
async fn reads_the_features_of_a_tile() {
    let dir = tempfile::tempdir().expect("failed to create a temp dir");
    let mut martin = martin_with_cities(&dir).await;
    let arguments = json!({"source": "world_cities", "longitude": 12.57, "latitude": 55.68, "zoom": 0, "limit": 2});
    insta::assert_snapshot!(call_tool(&martin, "get_features", &arguments).await);
    martin.stop().await;
}
