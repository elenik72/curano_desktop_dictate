//! Tauri commands proxying the Curano dictation-commands CRUD API.
//!
//! All HTTP goes through Rust for the same reasons as `uploads`: the JWT
//! lives here and the webview would hit CORS. Errors are returned as
//! `"{status_code}:{detail}"` so the frontend can map known statuses
//! (409 duplicate phrase) to localized messages and show the server detail
//! for the rest.

use serde_json::{Map, Value};
use tauri::AppHandle;

use crate::uploads::api::{api_ctx, ApiCtx};

use super::types::{
    DictationCommand, DictationCommandCreate, DictationCommandList, DictationCommandUpdate,
    DictationListQuery, DictationPhrase, DictationPhraseInput, DictationPhraseUpdate,
};

fn error_detail(value: &Value) -> Option<String> {
    let detail = value.get("detail")?;
    let text = match detail {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("msg").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(300).collect())
    }
}

/// Send a request and parse the JSON response, mapping non-2xx to
/// `"{status}:{detail}"` strings.
async fn send_json(builder: reqwest::RequestBuilder, what: &str) -> Result<Value, String> {
    let response = builder
        .send()
        .await
        .map_err(|e| format!("0:{} request failed: {}", what, e.without_url()))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|_| format!("0:Failed to read {} response", what))?;

    let value = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);

    if !status.is_success() {
        log::warn!(
            "dictation {} failed: status={}, body={}",
            what,
            status,
            body.chars().take(300).collect::<String>()
        );
        let detail = error_detail(&value).unwrap_or_else(|| format!("{} failed", what));
        return Err(format!("{}:{}", status.as_u16(), detail));
    }

    Ok(value)
}

fn parse<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T, String> {
    serde_json::from_value(value)
        .map_err(|e| format!("0:Unexpected {} response shape: {}", what, e))
}

fn dictation_url(ctx: &ApiCtx, path: &str) -> String {
    format!("{}/api/dictation{}", ctx.base_url, path)
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_list_commands(
    app: AppHandle,
    query: DictationListQuery,
) -> Result<DictationCommandList, String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;

    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(v) = query.search.filter(|s| !s.trim().is_empty()) {
        params.push(("search", v));
    }
    if let Some(v) = query.language {
        params.push(("language", v));
    }
    if let Some(v) = query.source {
        params.push(("source", v));
    }
    if let Some(v) = query.enabled {
        params.push(("enabled", v.to_string()));
    }
    if let Some(v) = query.operation_type {
        params.push(("operation_type", v));
    }
    if let Some(v) = query.cursor {
        params.push(("cursor", v));
    }
    params.push((
        "limit",
        query.limit.unwrap_or(200).clamp(1, 200).to_string(),
    ));

    let value = send_json(
        ctx.get(&dictation_url(&ctx, "/commands")).query(&params),
        "command list",
    )
    .await?;

    parse(value, "command list")
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_create_command(
    app: AppHandle,
    payload: DictationCommandCreate,
) -> Result<DictationCommand, String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;

    let needs_replacement = !matches!(payload.operation_type.as_str(), "newline" | "paragraph");
    let body = serde_json::json!({
        "name": payload.name,
        "operation_type": payload.operation_type,
        "replacement_value": if needs_replacement { Value::from(payload.replacement_value) } else { Value::Null },
        "phrases": payload.phrases.iter().map(|p| {
            serde_json::json!({ "phrase": p.phrase, "language": p.language })
        }).collect::<Vec<_>>(),
    });

    let value = send_json(
        ctx.post(&dictation_url(&ctx, "/commands")).json(&body),
        "command create",
    )
    .await?;

    parse(value, "command create")
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_update_command(
    app: AppHandle,
    command_id: i64,
    payload: DictationCommandUpdate,
) -> Result<DictationCommand, String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;

    let mut body = Map::new();
    if let Some(name) = payload.name {
        body.insert("name".into(), Value::from(name));
    }
    if let Some(op) = payload.operation_type.clone() {
        // Switching to newline/paragraph requires clearing the replacement.
        if matches!(op.as_str(), "newline" | "paragraph") {
            body.insert("replacement_value".into(), Value::Null);
        }
        body.insert("operation_type".into(), Value::from(op));
    }
    if let Some(replacement) = payload.replacement_value {
        body.insert("replacement_value".into(), Value::from(replacement));
    }

    let value = send_json(
        ctx.patch(&dictation_url(&ctx, &format!("/commands/{}", command_id)))
            .json(&Value::Object(body)),
        "command update",
    )
    .await?;

    parse(value, "command update")
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_delete_command(app: AppHandle, command_id: i64) -> Result<(), String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;
    send_no_content(
        ctx.delete(&dictation_url(&ctx, &format!("/commands/{}", command_id))),
        "command delete",
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_add_phrase(
    app: AppHandle,
    command_id: i64,
    payload: DictationPhraseInput,
) -> Result<DictationPhrase, String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;

    let body = serde_json::json!({ "phrase": payload.phrase, "language": payload.language });
    let value = send_json(
        ctx.post(&dictation_url(
            &ctx,
            &format!("/commands/{}/phrases", command_id),
        ))
        .json(&body),
        "phrase add",
    )
    .await?;

    parse(value, "phrase add")
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_update_phrase(
    app: AppHandle,
    phrase_id: i64,
    payload: DictationPhraseUpdate,
) -> Result<DictationPhrase, String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;

    let mut body = Map::new();
    if let Some(phrase) = payload.phrase {
        body.insert("phrase".into(), Value::from(phrase));
    }
    if let Some(language) = payload.language {
        body.insert("language".into(), Value::from(language));
    }

    let value = send_json(
        ctx.patch(&dictation_url(&ctx, &format!("/phrases/{}", phrase_id)))
            .json(&Value::Object(body)),
        "phrase update",
    )
    .await?;

    parse(value, "phrase update")
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_delete_phrase(app: AppHandle, phrase_id: i64) -> Result<(), String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;
    send_no_content(
        ctx.delete(&dictation_url(&ctx, &format!("/phrases/{}", phrase_id))),
        "phrase delete",
    )
    .await
}

/// Disable (`disabled = true`) or re-enable a global default command for the
/// current doctor. Personal commands are deleted instead — the server
/// exposes no disable toggle for them (`capabilities.can_disable = false`).
#[tauri::command]
#[specta::specta]
pub async fn dictation_set_default_command_disabled(
    app: AppHandle,
    command_id: i64,
    disabled: bool,
) -> Result<(), String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;
    let url = dictation_url(&ctx, &format!("/default-commands/{}/disabled", command_id));
    let builder = if disabled {
        ctx.put(&url)
    } else {
        ctx.delete(&url)
    };
    send_no_content(builder, "default command toggle").await
}

#[tauri::command]
#[specta::specta]
pub async fn dictation_set_default_phrase_disabled(
    app: AppHandle,
    phrase_id: i64,
    disabled: bool,
) -> Result<(), String> {
    let ctx = api_ctx(&app).await.map_err(|e| format!("0:{}", e))?;
    let url = dictation_url(&ctx, &format!("/default-phrases/{}/disabled", phrase_id));
    let builder = if disabled {
        ctx.put(&url)
    } else {
        ctx.delete(&url)
    };
    send_no_content(builder, "default phrase toggle").await
}

async fn send_no_content(builder: reqwest::RequestBuilder, what: &str) -> Result<(), String> {
    match send_json(builder, what).await {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}
