use std::collections::HashSet;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientInfo, Implementation};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::json;

type Client = RunningService<rmcp::RoleClient, ClientInfo>;

#[tokio::test(flavor = "multi_thread")]
async fn image_honors_the_computer_contract() {
    let Some(base) = std::env::var("TOAD_COMPUTER_URL").ok() else {
        eprintln!("skipped: set TOAD_COMPUTER_URL to run the container contract test");
        return;
    };
    let token = std::env::var("TOAD_COMPUTER_TOKEN").unwrap_or_default();
    let health = reqwest::get(format!("{base}/health"))
        .await
        .expect("health request");
    assert!(health.status().is_success(), "/health is open");

    if !token.is_empty() {
        let naked = reqwest::Client::new()
            .post(format!("{base}/mcp"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .send()
            .await
            .expect("unauthenticated request");
        assert_eq!(naked.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            naked.json::<serde_json::Value>().await.unwrap()["error"],
            "unauthorized"
        );
    }

    let client = connect(&base, &token, "contract").await;
    let listed = client.list_all_tools().await.expect("tools/list");
    let names: HashSet<_> = listed.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        HashSet::from([
            "capture", "input", "browser", "shell", "files", "windows", "wait", "state"
        ])
    );

    let shell = call(
        &client,
        "shell",
        json!({"command":"sh","args":["-c","echo computer-says-42"]}),
    )
    .await;
    assert!(
        text(&shell).contains("computer-says-42"),
        "{}",
        text(&shell)
    );

    let navigated = call(
        &client,
        "browser",
        json!({"action":"navigate","url":"data:text/html,<title>Proof</title><h1>Rust computer proof</h1>"}),
    )
    .await;
    assert!(!navigated.is_error.unwrap_or(false), "{}", text(&navigated));
    let page = call(&client, "browser", json!({"action":"text"})).await;
    assert!(
        text(&page).contains("Rust computer proof"),
        "{}",
        text(&page)
    );

    let capture = call(&client, "capture", json!({})).await;
    assert!(
        capture
            .content
            .iter()
            .any(|block| block.as_image().is_some())
    );
    assert!(
        text(&capture).contains("Rust computer proof"),
        "the accessibility tree reaches into the page:\n{}",
        text(&capture)
    );
    let windows = call(&client, "windows", json!({"action":"list"})).await;
    let window_list: Vec<serde_json::Value> =
        serde_json::from_str(&text(&windows)).expect("window JSON");
    assert!(
        !window_list.is_empty(),
        "Chromium is a visible desktop window"
    );

    // The person can close the browser from the viewer; the next browser
    // call must start a fresh one instead of waiting on the old one forever.
    let closed = call(
        &client,
        "shell",
        json!({"command":"sh","args":["-c","pkill -x chromium; sleep 2; echo closed"]}),
    )
    .await;
    assert!(text(&closed).contains("closed"), "{}", text(&closed));
    let reopened = call(
        &client,
        "browser",
        json!({"action":"navigate","url":"data:text/html,<h1>Back after close</h1>"}),
    )
    .await;
    assert!(!reopened.is_error.unwrap_or(false), "{}", text(&reopened));
    let page = call(&client, "browser", json!({"action":"text"})).await;
    assert!(text(&page).contains("Back after close"), "{}", text(&page));

    // Destroying the window from outside leaves Chromium running with pages
    // and no window. The desktop reports the loss and the next browser call
    // is a browser someone can see.
    let windows = call(&client, "windows", json!({"action":"list"})).await;
    let window_list: Vec<serde_json::Value> =
        serde_json::from_str(&text(&windows)).expect("window JSON");
    let browser_window = window_list[0]["id"].as_str().expect("window id").to_owned();
    let closed = call(
        &client,
        "windows",
        json!({"action":"close","window_id":browser_window}),
    )
    .await;
    assert!(!closed.is_error.unwrap_or(false), "{}", text(&closed));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let reopened = call(
        &client,
        "browser",
        json!({"action":"navigate","url":"data:text/html,<h1>Back after window close</h1>"}),
    )
    .await;
    assert!(!reopened.is_error.unwrap_or(false), "{}", text(&reopened));
    // The X title trails the page load by a beat.
    let mut windows = String::new();
    for _ in 0..20 {
        windows = text(&call(&client, "windows", json!({"action":"list"})).await);
        if windows.contains("Back after window close") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        windows.contains("Back after window close"),
        "a browser window is on the desktop again: {windows}"
    );

    let path = "/home/agent/contract/probe.txt";
    let put = call(
        &client,
        "files",
        json!({"action":"put","path":path,"content":"over-mcp"}),
    )
    .await;
    assert!(!put.is_error.unwrap_or(false), "{}", text(&put));
    let get = call(&client, "files", json!({"action":"get","path":path})).await;
    assert_eq!(text(&get), "over-mcp");
    let escaped = call(
        &client,
        "files",
        json!({"action":"get","path":"/etc/passwd"}),
    )
    .await;
    assert!(escaped.is_error.unwrap_or(false));
    assert!(text(&escaped).contains("path must be under"));

    let alice = connect(&base, &token, "alice").await;
    let mallory = connect(&base, &token, "mallory").await;
    let taken = call(&alice, "state", json!({"action":"control","duration":60})).await;
    assert!(text(&taken).contains("alice"));
    let blocked = call(&mallory, "input", json!({"action":"key","combo":"Escape"})).await;
    assert!(blocked.is_error.unwrap_or(false));
    assert!(text(&blocked).contains("alice"));
    let released = call(&alice, "state", json!({"action":"release"})).await;
    assert!(text(&released).contains("\"released\":true"));

    // The viewer: the page is open, the socket wants the token, the first
    // frame is the whole screen as PNG, and a person's keystroke lands in
    // the page and holds the machine against the agent for a moment.
    let page = reqwest::get(format!("{base}/"))
        .await
        .expect("viewer page")
        .text()
        .await
        .expect("viewer html");
    assert!(page.contains("<canvas"), "the viewer page is served at /");
    let ws_base = base.replacen("http", "ws", 1);
    if !token.is_empty() {
        let refused = tokio_tungstenite::connect_async(format!("{ws_base}/ws")).await;
        assert!(
            matches!(
                refused,
                Err(tokio_tungstenite::tungstenite::Error::Http(ref response))
                    if response.status() == 401
            ),
            "the socket refuses a missing token"
        );
    }
    let typing = call(
        &client,
        "browser",
        json!({"action":"navigate","url":"data:text/html,<title>Typing</title><textarea id=t autofocus></textarea>"}),
    )
    .await;
    assert!(!typing.is_error.unwrap_or(false), "{}", text(&typing));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("{ws_base}/ws?token={token}"))
        .await
        .expect("viewer socket");
    let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("a frame within five seconds")
        .expect("a frame")
        .expect("a frame");
    let bytes = first.into_data();
    let header: Vec<u16> = (0..6)
        .map(|index| u16::from_le_bytes([bytes[index * 2], bytes[index * 2 + 1]]))
        .collect();
    assert_eq!(
        &header[..4],
        &[0, 0, header[4], header[5]],
        "the first frame is the whole screen"
    );
    assert_eq!(&bytes[12..16], b"\x89PNG", "the frame is a PNG");
    for down in [true, false] {
        socket
            .send(tokio_tungstenite::tungstenite::Message::text(
                json!({"t":"key","key":"x","down":down}).to_string(),
            ))
            .await
            .expect("send key");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let typed = call(
        &client,
        "browser",
        json!({"action":"eval","js":"document.getElementById('t').value"}),
    )
    .await;
    assert!(
        text(&typed).contains('x'),
        "the keystroke reached the page: {}",
        text(&typed)
    );
    let held = call(&client, "input", json!({"action":"key","combo":"Escape"})).await;
    assert!(held.is_error.unwrap_or(false), "{}", text(&held));
    assert!(text(&held).contains("person"), "{}", text(&held));
    socket.close(None).await.ok();

    client.cancel().await.ok();
    alice.cancel().await.ok();
    mallory.cancel().await.ok();
}

async fn connect(base: &str, token: &str, holder: &str) -> Client {
    let mut headers = HeaderMap::new();
    if !token.is_empty() {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
    }
    headers.insert(
        HeaderName::from_static("x-computer-holder"),
        HeaderValue::from_str(holder).unwrap(),
    );
    let http = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let transport = StreamableHttpClientTransport::with_client(
        http,
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")),
    );
    ClientInfo::new(
        Default::default(),
        Implementation::new("toad-computer-contract", "1"),
    )
    .serve(transport)
    .await
    .expect("MCP handshake")
}

async fn call(
    client: &Client,
    name: &str,
    arguments: serde_json::Value,
) -> rmcp::model::CallToolResult {
    client
        .peer()
        .call_tool(
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(arguments.as_object().expect("object").clone()),
        )
        .await
        .unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|content| content.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}
