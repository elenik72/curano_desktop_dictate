use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use specta::Type;

/// What the current doctor may do with a command or phrase. Comes from the
/// server; the UI must follow it instead of deriving rules locally.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationCapabilities {
    pub can_edit: bool,
    pub can_delete: bool,
    pub can_disable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationPhrase {
    pub id: i64,
    pub command_id: i64,
    pub phrase: String,
    pub normalized_phrase: Option<String>,
    pub language: Option<String>,
    /// `global` or `user`. A phrase may be personal even on a global command.
    pub source: String,
    pub enabled: bool,
    pub disabled_by_doctor: bool,
    pub capabilities: DictationCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationCommand {
    pub id: i64,
    pub name: String,
    pub operation_type: String,
    pub replacement_value: Option<String>,
    pub source: String,
    pub enabled: bool,
    pub disabled_by_doctor: bool,
    pub sort_order: i64,
    pub phrase_count: i64,
    pub enabled_phrase_count: i64,
    pub capabilities: DictationCapabilities,
    pub phrases: Vec<DictationPhrase>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationFacets {
    #[serde(default)]
    pub languages: HashMap<String, i64>,
    #[serde(default)]
    pub sources: HashMap<String, i64>,
    #[serde(default)]
    pub operation_types: HashMap<String, i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationCommandList {
    pub items: Vec<DictationCommand>,
    pub total: i64,
    pub next_cursor: Option<String>,
    pub facets: DictationFacets,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationListQuery {
    pub search: Option<String>,
    pub language: Option<String>,
    pub source: Option<String>,
    pub enabled: Option<bool>,
    pub operation_type: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationPhraseInput {
    pub phrase: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationCommandCreate {
    pub name: String,
    pub operation_type: String,
    pub replacement_value: Option<String>,
    pub phrases: Vec<DictationPhraseInput>,
}

/// PATCH payload for the doctor's own command; `None` fields are omitted.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationCommandUpdate {
    pub name: Option<String>,
    pub operation_type: Option<String>,
    /// `Some(None)` is not representable here; the server treats an explicit
    /// null as "clear", which only makes sense for newline/paragraph — the
    /// command layer sends null automatically for those types.
    pub replacement_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DictationPhraseUpdate {
    pub phrase: Option<String>,
    pub language: Option<String>,
}
