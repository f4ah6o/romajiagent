use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformContext {
    pub timestamp: DateTime<Utc>,
    pub os: String,
    pub app_name: Option<String>,
    pub process_id: Option<u32>,
    pub window_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformRequest {
    pub raw: String,
    pub memory: String,
    pub context: TransformContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformResponse {
    pub converted: String,
    pub refined: String,
    pub confidence: f32,
}
