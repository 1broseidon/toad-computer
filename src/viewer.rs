//! The viewer: the page a person opens to see and drive the desktop, and the
//! WebSocket behind it.
//!
//! The desk puts the bearer in the URL's fragment, which never leaves the
//! browser; the page hands it back as the socket's `token` query. Frames go
//! down as binary messages, input comes up as JSON. A person's input holds
//! the machine for a few seconds at a time, so a teammate's mutating tools
//! are refused while someone is driving, and nobody has to remember to give
//! the desktop back.

use std::sync::Arc;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::App;
use crate::display::Display;
use crate::serve::same_secret;

const PAGE: &str = include_str!("viewer.html");
/// The holder name a person's input takes the machine under.
pub const PERSON: &str = "person";
/// Seconds the machine stays the person's after their last input.
const HOLD: u64 = 10;

pub async fn page() -> Html<&'static str> {
    Html(PAGE)
}

#[derive(Deserialize)]
pub struct Ticket {
    #[serde(default)]
    token: String,
}

pub async fn socket(
    State(app): State<App>,
    Query(ticket): Query<Ticket>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(expected) = app.config.token.as_deref()
        && !same_secret(ticket.token.as_bytes(), expected.as_bytes())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response();
    }
    let Some(display) = app.display.clone() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this computer has no display",
        )
            .into_response();
    };
    upgrade.on_upgrade(move |ws| drive(ws, app, display))
}

async fn drive(mut ws: WebSocket, app: App, display: Arc<Display>) {
    let mut frames = display.screen.subscribe();
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if ws.send(Message::Binary(frame.encode().into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => display.screen.request_full(),
                Err(RecvError::Closed) => break,
            },
            message = ws.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Err(error) = handle(&text, &display, &app).await {
                        eprintln!("toad-computer: viewer: {error}");
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    let _ = app.access.release(PERSON).await;
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Input {
    Move {
        x: i16,
        y: i16,
    },
    Button {
        b: u8,
        down: bool,
    },
    /// One notch: `dy` −1 is up and 1 is down; `dx` −1 is left and 1 is right.
    Wheel {
        #[serde(default)]
        dx: i8,
        #[serde(default)]
        dy: i8,
    },
    Key {
        key: String,
        down: bool,
    },
}

async fn handle(text: &str, display: &Display, app: &App) -> Result<(), String> {
    let input: Input = serde_json::from_str(text).map_err(|error| format!("input: {error}"))?;
    app.access.seize(PERSON, HOLD).await;
    let mut hands = display
        .hands
        .lock()
        .map_err(|_| "the hands are poisoned".to_owned())?;
    match input {
        Input::Move { x, y } => hands.move_to(x, y),
        Input::Button { b, down } => hands.button(b, down),
        Input::Wheel { dx, dy } => {
            for button in [(dy < 0, 4), (dy > 0, 5), (dx < 0, 6), (dx > 0, 7)]
                .into_iter()
                .filter_map(|(turned, button)| turned.then_some(button))
            {
                hands.button(button, true)?;
                hands.button(button, false)?;
            }
            Ok(())
        }
        Input::Key { key, down } => hands.key(&key, down),
    }
}
