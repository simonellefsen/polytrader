//! Journal domain models (reflections, decisions, experiment records).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reflection {
    pub id: uuid::Uuid,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub summary: String,
    pub metrics: serde_json::Value,
    pub recommendations: Vec<String>,
    pub created_at: DateTime<Utc>,
}
