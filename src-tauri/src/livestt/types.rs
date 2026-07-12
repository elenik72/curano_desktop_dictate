use serde::Serialize;
use specta::Type;

#[derive(Debug, Clone, Serialize, Type)]
pub struct LiveSttAuthStatus {
    pub is_authenticated: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}
