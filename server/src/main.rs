mod rooms;
mod source;

use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State, WebSocketUpgrade, rejection::JsonRejection, ws::{Message, Utf8Bytes, WebSocket}},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, get_service, post},
};
use serde::Deserialize;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, broadcast::error::RecvError},
    time::{Instant, interval, sleep, timeout},
};
use tower_http::{services::{ServeDir, ServeFile}, set_header::SetResponseHeaderLayer};

use rooms::{Admission, ClientMessage, CreateFailure, Rooms, ServerMessage, Update, WireError, encode};
use source::Source;

const MAX_MESSAGE_BYTES: usize = 8192;
const MAX_SOCKETS: usize = 512;
const MAX_MESSAGES_PER_WINDOW: u8 = 20;
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKET_IDLE: Duration = Duration::from_secs(120);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct App {
    rooms: Arc<Rooms>,
    sockets: Arc<Semaphore>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let static_dir = PathBuf::from(env::var_os("STATIC_DIR").unwrap_or_else(|| "web/dist".into()));
    let listener = TcpListener::bind(bind_addr).await?;
    let rooms = Arc::new(Rooms::new());
    let app = App { rooms: Arc::clone(&rooms), sockets: Arc::new(Semaphore::new(MAX_SOCKETS)) };
    tokio::spawn(async move {
        let mut ticks = interval(SWEEP_INTERVAL);
        loop {
            ticks.tick().await;
            rooms.prune();
        }
    });

    let router = Router::new()
        .route("/api/health", get(|| async { StatusCode::OK }))
        .route("/api/rooms", post(create_room).layer(DefaultBodyLimit::max(MAX_MESSAGE_BYTES)))
        .route("/api/rooms/{room_id}/ws", get(upgrade))
        .route("/r/{room_id}", get_service(ServeFile::new(static_dir.join("index.html"))))
        .route("/i/{token}", get(open_invitation))
        .fallback_service(ServeDir::new(static_dir))
        // Keep the room capability out of referrers without breaking YouTube embeds.
        .layer(SetResponseHeaderLayer::overriding(header::REFERRER_POLICY, HeaderValue::from_static("strict-origin")))
        .with_state(app);
    eprintln!("Share Video listening on {}", listener.local_addr()?);
    axum::serve(listener, router).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRoom {
    url: String,
}

async fn create_room(State(app): State<App>, body: Result<Json<CreateRoom>, JsonRejection>) -> Response {
    let Json(CreateRoom { url }) = match body {
        Ok(body) => body,
        Err(error) => return (error.status(), Json(WireError::INVALID_MESSAGE)).into_response(),
    };
    let Some(source) = Source::parse(&url) else {
        return (StatusCode::BAD_REQUEST, Json(WireError::INVALID_SOURCE)).into_response();
    };
    match app.rooms.create(source) {
        Ok(created) => (StatusCode::CREATED, [(header::CACHE_CONTROL, "no-store")], Json(created)).into_response(),
        Err(CreateFailure::Full) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(WireError::new("room_capacity", "Нет места для новой комнаты.")),
        ).into_response(),
        Err(CreateFailure::Entropy) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(WireError::new("internal_error", "Не удалось создать комнату.")),
        ).into_response(),
    }
}

async fn open_invitation(State(app): State<App>, Path(token): Path<String>) -> Response {
    let cache = [(header::CACHE_CONTROL, "no-store")];
    match app.rooms.invite_room(&token) {
        Some(room_id) => (cache, Redirect::temporary(&format!("/r/{room_id}?invite={token}"))).into_response(),
        None => (StatusCode::NOT_FOUND, cache, "Приглашение недействительно или комната закрыта.").into_response(),
    }
}

fn origin_matches_host(headers: &HeaderMap, uri: &Uri) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    // A browser WebSocket always carries Origin; non-browser clients need not send it.
    let Some(origin) = origins.next() else { return true };
    if origins.next().is_some() {
        return false;
    }
    let Some((scheme, origin_host)) = origin.to_str().ok().and_then(|s| s.split_once("://")) else {
        return false;
    };
    let default_port = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return false,
    };
    let mut hosts = headers.get_all(header::HOST).iter();
    let host = match hosts.next() {
        Some(host) => host.to_str().ok(),
        None => uri.authority().map(|authority| authority.as_str()),
    };
    if hosts.next().is_some() {
        return false;
    }
    match (split_host(origin_host, default_port), host.and_then(|host| split_host(host, default_port))) {
        (Some((origin, origin_port)), Some((host, port))) => origin.eq_ignore_ascii_case(host) && origin_port == port,
        _ => false,
    }
}

fn split_host(authority: &str, default_port: u16) -> Option<(&str, u16)> {
    let (host, suffix) = if authority.starts_with('[') {
        let (host, suffix) = authority.split_at(authority.find(']')? + 1);
        host[1..host.len() - 1].parse::<std::net::Ipv6Addr>().ok()?;
        (host, suffix)
    } else {
        let (host, suffix) = authority.split_at(authority.find(':').unwrap_or(authority.len()));
        if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b)) {
            return None;
        }
        (host, suffix)
    };
    let port = if suffix.is_empty() { default_port } else { suffix.strip_prefix(':')?.parse().ok()? };
    Some((host, port))
}

async fn upgrade(
    State(app): State<App>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_matches_host(&headers, &uri) {
        return (StatusCode::FORBIDDEN, Json(WireError::new("forbidden", "Недопустимый источник соединения."))).into_response();
    }
    let Ok(slot) = app.sockets.try_acquire_owned() else {
        return (StatusCode::TOO_MANY_REQUESTS, Json(WireError::new("connection_capacity", "Слишком много подключений."))).into_response();
    };
    ws.read_buffer_size(MAX_MESSAGE_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_MESSAGE_BYTES * 2)
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _slot = slot;
            serve_socket(socket, app.rooms, room_id).await;
        })
}

async fn send(socket: &mut WebSocket, text: Utf8Bytes) -> bool {
    matches!(timeout(WRITE_TIMEOUT, socket.send(Message::Text(text))).await, Ok(Ok(())))
}

fn error_message(error: WireError) -> Utf8Bytes {
    encode(ServerMessage::Error { error })
}

async fn close_with_error(socket: &mut WebSocket, error: WireError) {
    if send(socket, error_message(error)).await {
        let _ = timeout(WRITE_TIMEOUT, socket.send(Message::Close(None))).await;
    }
}

async fn serve_socket(mut socket: WebSocket, rooms: Arc<Rooms>, room_id: String) {
    let first = match timeout(JOIN_TIMEOUT, socket.recv()).await {
        Err(_) => {
            return close_with_error(&mut socket, WireError::new("invalid_message", "Время ожидания входа истекло.")).await;
        }
        Ok(Some(Ok(message))) => message,
        _ => return,
    };
    let (host_token, guest_token) = match first {
        Message::Text(text) => match serde_json::from_str(text.as_str()) {
            Ok(ClientMessage::Join { host_token, guest_token }) => (host_token, guest_token),
            _ => return close_with_error(&mut socket, WireError::INVALID_MESSAGE).await,
        },
        Message::Close(_) => return,
        _ => return close_with_error(&mut socket, WireError::INVALID_MESSAGE).await,
    };
    let Admission { participant, mut updates, snapshot } = match rooms.join(room_id, host_token.as_deref(), guest_token.as_deref()) {
        Ok(admission) => admission,
        Err(error) => return close_with_error(&mut socket, error).await,
    };
    drop((host_token, guest_token));
    if !send(&mut socket, snapshot).await {
        return;
    }

    let idle = sleep(SOCKET_IDLE);
    tokio::pin!(idle);
    let mut window = (Instant::now(), 0);
    loop {
        let outgoing = tokio::select! {
            _ = &mut idle => {
                let _ = timeout(WRITE_TIMEOUT, socket.send(Message::Close(None))).await;
                break;
            }
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                if matches!(message, Message::Close(_)) {
                    break;
                }
                let now = Instant::now();
                if now - window.0 >= Duration::from_secs(1) {
                    window = (now, 0);
                }
                window.1 += 1;
                if window.1 > MAX_MESSAGES_PER_WINDOW {
                    close_with_error(&mut socket, WireError::new("invalid_message", "Слишком много сообщений.")).await;
                    break;
                }
                idle.as_mut().reset(now + SOCKET_IDLE);
                match message {
                    Message::Ping(_) | Message::Pong(_) => None,
                    Message::Text(text) => match serde_json::from_str(text.as_str()) {
                        Ok(ClientMessage::Ping { client_now_ms }) => Some(encode(ServerMessage::Pong {
                            client_now_ms,
                            server_now_ms: rooms.now_ms(),
                        })),
                        Ok(ClientMessage::Join { .. }) | Err(_) => Some(error_message(WireError::INVALID_MESSAGE)),
                        Ok(command) => participant.command(command).err().map(error_message),
                    },
                    _ => Some(error_message(WireError::INVALID_MESSAGE)),
                }
            }
            update = updates.recv() => match update {
                Ok(Update::Playback(text)) => Some(text),
                Ok(Update::AccessChanged) => Some(participant.access()),
                Err(RecvError::Lagged(_)) => {
                    let (fresh, access, state) = participant.resync();
                    updates = fresh;
                    if !send(&mut socket, access).await {
                        break;
                    }
                    Some(state)
                }
                Err(RecvError::Closed) => break,
            }
        };
        if let Some(text) = outgoing && !send(&mut socket, text).await {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_checks_browser_origin_against_the_real_request_host() {
        let uri = "/api/rooms/id/ws".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("localhost:5173"));
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://localhost:5173"));
        assert!(origin_matches_host(&headers, &uri));
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://localhost:8080"));
        headers.insert("x-forwarded-host", HeaderValue::from_static("localhost:8080"));
        assert!(!origin_matches_host(&headers, &uri));
        headers.insert(header::HOST, HeaderValue::from_static("watch.example.com:443"));
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://watch.example.com"));
        assert!(origin_matches_host(&headers, &uri));
        headers.append(header::ORIGIN, HeaderValue::from_static("https://watch.example.com"));
        assert!(!origin_matches_host(&headers, &uri));
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://watch.example.com/path"));
        assert!(!origin_matches_host(&headers, &uri));
        headers.insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!origin_matches_host(&headers, &uri));
    }
}
