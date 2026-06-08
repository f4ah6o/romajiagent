use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Instant,
};

use arboard::Clipboard;
use anyhow::anyhow;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppConfig {
    shortcut_macos: String,
    shortcut_other: String,
    sidecar_command: Option<String>,
    sidecar_args: Vec<String>,
    model_path: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            shortcut_macos: "Cmd+Shift+J".into(),
            shortcut_other: "Ctrl+Shift+J".into(),
            sidecar_command: None,
            sidecar_args: vec![],
            model_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransformContext {
    timestamp: DateTime<Utc>,
    os: String,
    app_name: Option<String>,
    process_id: Option<u32>,
    window_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransformRequest {
    raw: String,
    memory: String,
    context: TransformContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransformResponse {
    converted: String,
    refined: String,
    confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransformResult {
    id: String,
    raw: String,
    converted: String,
    refined: String,
    final_text: String,
    confidence: f32,
    timings_ms: StageTimings,
    context: TransformContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StageTimings {
    normalize: u128,
    convert_refine: u128,
    full_roundtrip: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PathsDto {
    base_dir: String,
    config: String,
    memory: String,
    database: String,
    ops_dir: String,
    models_dir: String,
}

fn base_dir() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".romaji-agent"))
        .ok_or_else(|| "Could not resolve home directory".to_string())
}

fn ensure_layout() -> Result<PathBuf, String> {
    let base = base_dir()?;
    fs::create_dir_all(base.join("ops")).map_err(|e| e.to_string())?;
    fs::create_dir_all(base.join("models").join("lfm")).map_err(|e| e.to_string())?;

    let config_path = base.join("config.toml");
    if !config_path.exists() {
        let config = toml::to_string_pretty(&AppConfig::default()).map_err(|e| e.to_string())?;
        fs::write(&config_path, config).map_err(|e| e.to_string())?;
    }

    let memory_path = base.join("memory.md");
    if !memory_path.exists() {
        fs::write(
            &memory_path,
            "# Terminology\n\nmtg -> ミーティング\ntodo -> TODO\n\n# Style\n\nconcise sentences\n\n# Names\n\nzed -> Zed\ntauri -> Tauri\n",
        )
        .map_err(|e| e.to_string())?;
    }

    init_db(&base)?;
    Ok(base)
}

fn load_config(base: &PathBuf) -> AppConfig {
    let path = base.join("config.toml");
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn init_db(base: &PathBuf) -> Result<(), String> {
    let conn = Connection::open(base.join("db.sqlite")).map_err(|e| e.to_string())?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS transforms (
          id TEXT PRIMARY KEY,
          created_at TEXT NOT NULL,
          os TEXT NOT NULL,
          app_name TEXT,
          process_id INTEGER,
          window_title TEXT,
          raw TEXT NOT NULL,
          converted TEXT NOT NULL,
          refined TEXT NOT NULL,
          final TEXT NOT NULL,
          accepted INTEGER NOT NULL DEFAULT 0,
          edited_after_accept INTEGER NOT NULL DEFAULT 0,
          parent_id TEXT,
          change_id TEXT
        );

        CREATE TABLE IF NOT EXISTS corrections (
          id TEXT PRIMARY KEY,
          created_at TEXT NOT NULL,
          before TEXT NOT NULL,
          after TEXT NOT NULL,
          app_name TEXT
        );
        "#,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn normalize(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn context_now() -> TransformContext {
    TransformContext {
        timestamp: Utc::now(),
        os: std::env::consts::OS.to_string(),
        app_name: None,
        process_id: None,
        window_title: None,
    }
}

fn fallback_convert(raw: &str, memory: &str) -> TransformResponse {
    let mut converted = raw.to_string();
    for line in memory.lines() {
        if let Some((before, after)) = line.split_once("->") {
            let before = before.trim();
            let after = after.trim();
            if !before.is_empty() && !after.is_empty() {
                converted = converted.replace(before, after);
            }
        }
    }

    TransformResponse {
        refined: if converted.ends_with('。') {
            converted.clone()
        } else {
            format!("{converted}。")
        },
        converted,
        confidence: 0.25,
    }
}

fn infer_with_sidecar(
    config: &AppConfig,
    request: &TransformRequest,
) -> Result<TransformResponse, String> {
    let Some(command) = &config.sidecar_command else {
        return Err("sidecar_command is not configured".into());
    };

    let mut child = Command::new(command)
        .args(&config.sidecar_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start sidecar: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open sidecar stdin".to_string())?;
    let payload = serde_json::to_string(request).map_err(|e| e.to_string())?;
    writeln!(stdin, "{payload}").map_err(|e| e.to_string())?;
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open sidecar stdout".to_string())?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("sidecar exited with {status}"));
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("invalid sidecar json: {e}"))
}

fn save_transform(base: &PathBuf, result: &TransformResult, accepted: bool) -> Result<(), String> {
    let conn = Connection::open(base.join("db.sqlite")).map_err(|e| e.to_string())?;
    conn.execute(
        r#"
        INSERT OR REPLACE INTO transforms
        (id, created_at, os, app_name, process_id, window_title, raw, converted, refined, final, accepted, edited_after_accept, parent_id, change_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, NULL, ?12)
        "#,
        params![
            result.id,
            result.context.timestamp.to_rfc3339(),
            result.context.os,
            result.context.app_name,
            result.context.process_id,
            result.context.window_title,
            result.raw,
            result.converted,
            result.refined,
            result.final_text,
            if accepted { 1 } else { 0 },
            Uuid::new_v4().to_string(),
        ],
    )
    .map_err(|e| e.to_string())?;

    let ops_path = base
        .join("ops")
        .join(format!("{}.jsonl", Utc::now().format("%Y-%m-%d")));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(ops_path)
        .map_err(|e| e.to_string())?;
    let line = serde_json::json!({
        "type": if accepted { "accepted" } else { "preview" },
        "result": result,
    });
    writeln!(file, "{line}").map_err(|e| e.to_string())?;
    Ok(())
}

fn set_clipboard(text: &str) -> Result<(), String> {
    Clipboard::new()
        .map_err(|e| e.to_string())?
        .set_text(text.to_string())
        .map_err(|e| e.to_string())
}

fn get_clipboard() -> Result<String, String> {
    Clipboard::new()
        .map_err(|e| e.to_string())?
        .get_text()
        .map_err(|e| e.to_string())
}

fn paste_active_app() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("osascript")
            .args(["-e", "tell application \"System Events\" to keystroke \"v\" using command down"])
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v')"])
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("sh")
            .args(["-c", "command -v xdotool >/dev/null && xdotool key ctrl+v"])
            .status()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
}

#[tauri::command]
fn app_paths() -> Result<PathsDto, String> {
    let base = ensure_layout()?;
    Ok(PathsDto {
        base_dir: base.display().to_string(),
        config: base.join("config.toml").display().to_string(),
        memory: base.join("memory.md").display().to_string(),
        database: base.join("db.sqlite").display().to_string(),
        ops_dir: base.join("ops").display().to_string(),
        models_dir: base.join("models").join("lfm").display().to_string(),
    })
}

#[tauri::command]
fn transform_text(raw: String) -> Result<TransformResult, String> {
    let started = Instant::now();
    let base = ensure_layout()?;
    let config = load_config(&base);
    let memory = fs::read_to_string(base.join("memory.md")).unwrap_or_default();

    let normalize_started = Instant::now();
    let normalized = normalize(&raw);
    let normalize_ms = normalize_started.elapsed().as_millis();

    let request = TransformRequest {
        raw: normalized.clone(),
        memory: memory.clone(),
        context: context_now(),
    };

    let infer_started = Instant::now();
    let response =
        infer_with_sidecar(&config, &request).unwrap_or_else(|_| fallback_convert(&normalized, &memory));
    let infer_ms = infer_started.elapsed().as_millis();

    let result = TransformResult {
        id: Uuid::new_v4().to_string(),
        raw: normalized,
        converted: response.converted.clone(),
        refined: response.refined.clone(),
        final_text: response.refined,
        confidence: response.confidence,
        timings_ms: StageTimings {
            normalize: normalize_ms,
            convert_refine: infer_ms,
            full_roundtrip: started.elapsed().as_millis(),
        },
        context: request.context,
    };
    save_transform(&base, &result, false)?;
    Ok(result)
}

#[tauri::command]
fn accept_transform(result: TransformResult, final_text: String, paste: bool) -> Result<(), String> {
    let base = ensure_layout()?;
    let mut accepted = result;
    accepted.final_text = final_text.clone();
    save_transform(&base, &accepted, true)?;
    set_clipboard(&final_text)?;
    if paste {
        paste_active_app()?;
    }
    Ok(())
}

#[tauri::command]
fn transform_clipboard_selection() -> Result<TransformResult, String> {
    transform_text(get_clipboard()?)
}

fn register_shortcut(app: &AppHandle) -> Result<(), String> {
    let shortcut = if cfg!(target_os = "macos") {
        Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyJ)
    } else {
        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyJ)
    };

    app.global_shortcut()
        .on_shortcut(shortcut, {
            let app = app.clone();
            move |_app, _shortcut, event| {
                if event.state == ShortcutState::Pressed {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                        let _ = window.emit("romaji-shortcut", ());
                    }
                    let _ = app.emit("romaji-shortcut", ());
                }
            }
        })
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            ensure_layout().map_err(|e| tauri::Error::Anyhow(anyhow!(e)))?;
            register_shortcut(app.handle()).map_err(|e| tauri::Error::Anyhow(anyhow!(e)))?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_paths,
            transform_text,
            accept_transform,
            transform_clipboard_selection
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  kyou   mtg\tde\nhanasita todo  "), "kyou mtg de hanasita todo");
    }

    #[test]
    fn fallback_convert_applies_memory_terms() {
        let memory = "# Terminology\nmtg -> ミーティング\ntodo -> TODO\n";
        let response = fallback_convert("kyou mtg de todo", memory);
        assert_eq!(response.converted, "kyou ミーティング de TODO");
        assert_eq!(response.refined, "kyou ミーティング de TODO。");
    }

    #[test]
    fn config_roundtrips_toml() {
        let config = AppConfig {
            sidecar_command: Some("/path/to/sidecar".into()),
            sidecar_args: vec!["--model".into(), "/path/to/model.gguf".into()],
            ..Default::default()
        };
        let text = toml::to_string(&config).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.shortcut_macos, "Cmd+Shift+J");
        assert_eq!(parsed.sidecar_args.len(), 2);
    }

    #[test]
    fn db_schema_initializes() {
        let temp = std::env::temp_dir().join(format!("romajiagent-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp).unwrap();
        init_db(&temp).unwrap();
        assert!(temp.join("db.sqlite").exists());
        fs::remove_dir_all(temp).unwrap();
    }
}
