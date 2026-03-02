// Chat Controller with Full Features
use axum::{
    extract::{ws::{WebSocket, WebSocketUpgrade, Message}},
    response::IntoResponse,
    routing::get,
    Router,
};
use std::sync::Arc;
use futures::{sink::SinkExt, stream::StreamExt};
use tracing::{info, error};
use chrono::Utc;

use crate::server::{
    services::chat_services::{ChatService, ChatMessage, WsMessage, UserRole},
    EdgeServices,
};

pub struct ChatController;

impl ChatController {
    pub fn app() -> Router {
        Router::new()
            .route("/api/v1/chat/ws", get(Self::ws_handler))
    }
    
    pub async fn ws_handler(
        ws: WebSocketUpgrade,
        axum::extract::Extension(services): axum::extract::Extension<Arc<EdgeServices>>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| handle_socket(socket, services.chat.clone()))
    }
}

async fn handle_socket(socket: WebSocket, chat: Arc<ChatService>) {
    info!("New chat connection");
    
    let (mut sender, mut receiver) = socket.split();
    let mut current_user: Option<String> = None;
    let mut user_role: Option<UserRole> = None;
    let mut rx = chat.subscribe();
    
    // Forward broadcast messages
    let forward_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            let ws_msg = WsMessage::NewMessage { message: msg };
            let json = match serde_json::to_string(&ws_msg) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sender.send(Message::Text(json.into())).await.is_err() {
                break;
            }
        }
    });
    
    // Handle incoming messages
    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            match serde_json::from_str::<WsMessage>(&text) {
                Ok(WsMessage::Login { username, password }) => {
                    if let Some(user) = chat.login(&username, &password) {
                        current_user = Some(user.username.clone());
                        user_role = Some(user.role.clone());
                        let resp = WsMessage::AuthSuccess { 
                            username: user.username, 
                            role: user.role 
                        };
                        let _ = serde_json::to_string(&resp).map(|json| {
                            // Send join notification
                            let join_msg = ChatMessage {
                                id: format!("sys_{}", Utc::now().timestamp_millis()),
                                username: "System".to_string(),
                                content: format!("{} joined the chat", username),
                                timestamp: Utc::now().timestamp(),
                                is_announcement: false,
                                name_color: None,
                            };
                            let _ = chat.tx.send(join_msg);
                            json
                        });
                    } else {
                        let resp = WsMessage::AuthError { error: "Invalid credentials or user muted".to_string() };
                        if let Ok(json) = serde_json::to_string(&resp) {
                            let _ = chat.tx.send(ChatMessage {
                                id: format!("err_{}", Utc::now().timestamp_millis()),
                                username: "System".to_string(),
                                content: json,
                                timestamp: 0,
                                is_announcement: false,
                                name_color: None,
                            });
                        }
                    }
                }
                Ok(WsMessage::Register { username, password }) => {
                    if let Some(user) = chat.register(&username, &password) {
                        current_user = Some(user.username.clone());
                        user_role = Some(user.role.clone());
                        let resp = WsMessage::AuthSuccess { 
                            username: user.username, 
                            role: user.role 
                        };
                        let _ = serde_json::to_string(&resp);
                    } else {
                        let resp = WsMessage::AuthError { error: "Username already taken".to_string() };
                        let _ = serde_json::to_string(&resp);
                    }
                }
                Ok(WsMessage::Chat { content }) => {
                    if let Some(ref name) = current_user {
                        // Check if user is muted
                        let is_muted = chat.users.lock().map(|users| {
                            users.get(name).and_then(|u| u.muted_until).map(|m| Utc::now().timestamp() < m).unwrap_or(false)
                        }).unwrap_or(false);
                        
                        if !is_muted {
                            let msg = ChatMessage {
                                id: format!("msg_{}", Utc::now().timestamp_millis()),
                                username: name.clone(),
                                content,
                                timestamp: Utc::now().timestamp(),
                                is_announcement: false,
                                name_color: None,
                            };
                            let _ = chat.tx.send(msg);
                        }
                    }
                }
                Ok(WsMessage::Announcement { content }) => {
                    if let Some(ref name) = current_user {
                        if user_role == Some(UserRole::Admin) {
                            let msg = ChatMessage {
                                id: format!("announce_{}", Utc::now().timestamp_millis()),
                                username: name.clone(),
                                content,
                                timestamp: Utc::now().timestamp(),
                                is_announcement: true,
                                name_color: Some("#FFD700".to_string()),
                            };
                            let _ = chat.tx.send(msg);
                        }
                    }
                }
                Ok(WsMessage::Timeout { target_username, minutes }) => {
                    if let Some(ref _name) = current_user {
                        if user_role == Some(UserRole::Admin) {
                            chat.timeout_user(&target_username, minutes);
                            let msg = ChatMessage {
                                id: format!("sys_{}", Utc::now().timestamp_millis()),
                                username: "System".to_string(),
                                content: format!("{} has been timed out for {} minutes", target_username, minutes),
                                timestamp: Utc::now().timestamp(),
                                is_announcement: false,
                                name_color: None,
                            };
                            let _ = chat.tx.send(msg);
                        }
                    }
                }
                Err(e) => {
                    error!("Parse error: {}", e);
                }
                _ => {}
            }
        }
    }
    
    forward_task.abort();
    info!("User disconnected: {:?}", current_user);
}
