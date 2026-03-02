// Chat Service with Auth, Admin, Moderation
use serde::{Serialize, Deserialize};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserRole {
    User,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatUser {
    pub username: String,
    pub password: String,
    pub role: UserRole,
    pub muted_until: Option<i64>,
    pub name_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub username: String,
    pub content: String,
    pub timestamp: i64,
    pub is_announcement: bool,
    pub name_color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsMessage {
    #[serde(rename = "login")]
    Login { username: String, password: String },
    #[serde(rename = "register")]
    Register { username: String, password: String },
    #[serde(rename = "auth_success")]
    AuthSuccess { username: String, role: UserRole },
    #[serde(rename = "auth_error")]
    AuthError { error: String },
    #[serde(rename = "chat")]
    Chat { content: String },
    #[serde(rename = "announcement")]
    Announcement { content: String },
    #[serde(rename = "timeout")]
    Timeout { target_username: String, minutes: i64 },
    #[serde(rename = "new_message")]
    NewMessage { message: ChatMessage },
}

pub struct ChatService {
    pub tx: broadcast::Sender<ChatMessage>,
    pub users: Arc<Mutex<HashMap<String, ChatUser>>>,
}

impl ChatService {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(1000);
        let mut users = HashMap::new();
        
        // Pre-configured admins
        users.insert("Sultan".to_string(), ChatUser {
            username: "Sultan".to_string(),
            password: "Reedstreams11".to_string(),
            role: UserRole::Admin,
            muted_until: None,
            name_color: Some("#FFD700".to_string()),
        });
        
        users.insert("Reed".to_string(), ChatUser {
            username: "Reed".to_string(),
            password: "Reedsstreams333".to_string(),
            role: UserRole::Admin,
            muted_until: None,
            name_color: Some("#FF4500".to_string()),
        });
        
        Self {
            tx,
            users: Arc::new(Mutex::new(users)),
        }
    }
    
    pub fn login(&self, username: &str, password: &str) -> Option<ChatUser> {
        let users = self.users.lock().ok()?;
        let user = users.get(username)?;
        if user.password == password {
            // Check if muted
            if let Some(muted_until) = user.muted_until {
                if chrono::Utc::now().timestamp() < muted_until {
                    return None; // Still muted
                }
            }
            Some(user.clone())
        } else {
            None
        }
    }
    
    pub fn register(&self, username: &str, password: &str) -> Option<ChatUser> {
        let mut users = self.users.lock().ok()?;
        if users.contains_key(username) {
            return None; // Username taken
        }
        let user = ChatUser {
            username: username.to_string(),
            password: password.to_string(),
            role: UserRole::User,
            muted_until: None,
            name_color: None,
        };
        users.insert(username.to_string(), user.clone());
        Some(user)
    }
    
    pub fn timeout_user(&self, target: &str, minutes: i64) -> bool {
        let mut users = match self.users.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        if let Some(user) = users.get_mut(target) {
            user.muted_until = Some(chrono::Utc::now().timestamp() + (minutes * 60));
            true
        } else {
            false
        }
    }
    
    pub fn subscribe(&self) -> broadcast::Receiver<ChatMessage> {
        self.tx.subscribe()
    }
}
