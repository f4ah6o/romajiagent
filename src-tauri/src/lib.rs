use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use arboard::Clipboard;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use uuid::Uuid;

pub mod normalization;
pub mod protocol;

use normalization::normalize_input;
use protocol::{TransformContext, TransformRequest, TransformResponse};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    backend: String,
    shortcut_macos: String,
    shortcut_other: String,
    sidecar_command: Option<String>,
    sidecar_args: Vec<String>,
    model_path: Option<String>,
    codex_command: String,
    codex_args: Vec<String>,
    codex_model: Option<String>,
    codex_timeout_ms: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            backend: "sidecar".into(),
            shortcut_macos: "Cmd+Shift+J".into(),
            shortcut_other: "Ctrl+Shift+J".into(),
            sidecar_command: None,
            sidecar_args: vec![],
            model_path: None,
            codex_command: "codex".into(),
            codex_args: vec!["app-server".into()],
            codex_model: None,
            codex_timeout_ms: 90_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Sidecar,
    CodexAppServer,
    Fallback,
}

impl Backend {
    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex_app_server" | "codex-app-server" | "codex" => Self::CodexAppServer,
            "fallback" => Self::Fallback,
            _ => Self::Sidecar,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformResult {
    pub id: String,
    pub raw: String,
    pub normalized_raw: String,
    pub kana_candidate: String,
    pub converted: String,
    pub refined: String,
    pub final_text: String,
    pub confidence: f32,
    pub timings_ms: StageTimings,
    pub context: TransformContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcceptOutcome {
    clipboard_updated: bool,
    pasted: bool,
    paste_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StageTimings {
    pub normalize: u128,
    pub convert_refine: u128,
    pub full_roundtrip: u128,
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

fn sidecar_args(config: &AppConfig) -> Vec<String> {
    let mut args = config.sidecar_args.clone();
    let has_model_arg = args
        .iter()
        .any(|arg| arg == "--model" || arg.starts_with("--model="));
    if !has_model_arg {
        if let Some(model_path) = config.model_path.as_ref().filter(|path| !path.is_empty()) {
            args.push("--model".into());
            args.push(model_path.clone());
        }
    }
    args
}

fn infer_with_sidecar(
    config: &AppConfig,
    request: &TransformRequest,
) -> Result<TransformResponse, String> {
    let Some(command) = &config.sidecar_command else {
        return Err("sidecar_command is not configured".into());
    };

    let mut child = Command::new(command)
        .args(sidecar_args(config))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start sidecar: {e}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open sidecar stdin".to_string())?;
    let payload = serde_json::to_string(request).map_err(|e| e.to_string())?;
    writeln!(stdin, "{payload}").map_err(|e| e.to_string())?;
    drop(stdin);

    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let stderr_text = stderr_text.trim();
        if stderr_text.is_empty() {
            return Err(format!("sidecar exited with {}", output.status));
        }
        return Err(format!(
            "sidecar exited with {}: {stderr_text}",
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| "sidecar produced no stdout".to_string())?;
    serde_json::from_str(line.trim()).map_err(|e| format!("invalid sidecar json: {e}"))
}

fn codex_prompt(request: &TransformRequest) -> String {
    let kana_section = request
        .kana_candidate
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!("\nKana candidate:\n{value}\n"))
        .unwrap_or_default();
    format!(
        r#"Convert the following romaji, typo-heavy, or unconverted Japanese draft into natural Japanese.

Rules:
- Return only one JSON object matching this schema: {{"converted": string, "refined": string, "confidence": number}}.
- Do not use tools, shell commands, file reads, or network browsing.
- Treat Raw as typed romaji input, not a free semantic prompt.
- Prefer the Kana candidate as the phonetic anchor when it is present.
- Preserve intended meaning from the phonetic input. Use memory terms when they clearly apply.
- "converted" may be a direct conversion. "refined" should be natural, polished Japanese.
- "confidence" must be between 0 and 1.

Raw:
{raw}
{kana_section}

Memory:
{memory}
"#,
        raw = request.raw,
        kana_section = kana_section,
        memory = request.memory
    )
}

fn transform_response_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["converted", "refined", "confidence"],
        "properties": {
            "converted": { "type": "string" },
            "refined": { "type": "string" },
            "confidence": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0
            }
        }
    })
}

struct ChildCleanup {
    child: Option<Child>,
}

impl ChildCleanup {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn child_mut(&mut self) -> Result<&mut Child, String> {
        self.child
            .as_mut()
            .ok_or_else(|| "child process is already cleaned up".to_string())
    }
}

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_json_rpc_line(stdin: &mut impl Write, value: &Value) -> Result<(), String> {
    writeln!(stdin, "{value}").map_err(|e| e.to_string())
}

fn start_stdout_reader(stdout: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn recv_json_until(
    rx: &Receiver<String>,
    deadline: Instant,
    stdin: &mut impl Write,
    expected_id: u64,
) -> Result<Value, String> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("codex app-server timed out".into());
        }

        let line = rx
            .recv_timeout(deadline.saturating_duration_since(now))
            .map_err(|_| "codex app-server timed out".to_string())?;
        let value: Value =
            serde_json::from_str(&line).map_err(|e| format!("invalid app-server json: {e}"))?;

        if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
            if let Some(error) = value.get("error") {
                return Err(format!("codex app-server error: {error}"));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }

        reject_server_request(stdin, &value)?;
    }
}

fn reject_server_request(stdin: &mut impl Write, value: &Value) -> Result<(), String> {
    if value.get("method").and_then(Value::as_str).is_some() && value.get("id").is_some() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": value.get("id").cloned().unwrap_or(Value::Null),
            "error": {
                "code": -32601,
                "message": "Romaji Agent does not allow app-server initiated actions"
            }
        });
        write_json_rpc_line(stdin, &response)?;
    }
    Ok(())
}

fn text_from_codex_notification(
    value: &Value,
    turn_id: &str,
    accumulated: &mut String,
) -> Option<String> {
    let method = value.get("method").and_then(Value::as_str)?;
    let params = value.get("params")?;
    match method {
        "item/agentMessage/delta"
            if params.get("turnId").and_then(Value::as_str) == Some(turn_id) =>
        {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                accumulated.push_str(delta);
            }
            None
        }
        "item/completed" if params.get("turnId").and_then(Value::as_str) == Some(turn_id) => {
            let item = params.get("item")?;
            if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                item.get("text").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn wait_for_codex_turn(
    rx: &Receiver<String>,
    deadline: Instant,
    stdin: &mut impl Write,
    turn_id: &str,
) -> Result<String, String> {
    let mut accumulated = String::new();
    let mut final_text = None;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err("codex app-server timed out".into());
        }

        let line = rx
            .recv_timeout(deadline.saturating_duration_since(now))
            .map_err(|_| "codex app-server timed out".to_string())?;
        let value: Value =
            serde_json::from_str(&line).map_err(|e| format!("invalid app-server json: {e}"))?;

        if let Some(text) = text_from_codex_notification(&value, turn_id, &mut accumulated) {
            final_text = Some(text);
        }

        if value.get("method").and_then(Value::as_str) == Some("turn/completed")
            && value
                .get("params")
                .and_then(|params| params.get("turn"))
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                == Some(turn_id)
        {
            return final_text
                .or_else(|| (!accumulated.trim().is_empty()).then_some(accumulated))
                .ok_or_else(|| "codex app-server completed without an agent message".to_string());
        }

        if value.get("method").and_then(Value::as_str) == Some("error") {
            return Err(format!("codex app-server notification error: {value}"));
        }

        reject_server_request(stdin, &value)?;
    }
}

fn parse_transform_response_text(text: &str) -> Result<TransformResponse, String> {
    let trimmed = text.trim();
    serde_json::from_str(trimmed)
        .or_else(|_| {
            let start = trimmed.find('{').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing json object",
                ))
            })?;
            let end = trimmed.rfind('}').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing json object",
                ))
            })?;
            serde_json::from_str(&trimmed[start..=end])
        })
        .map_err(|e| format!("invalid codex transform json: {e}; text={trimmed:?}"))
}

fn infer_with_codex_app_server(
    config: &AppConfig,
    base: &std::path::Path,
    request: &TransformRequest,
) -> Result<TransformResponse, String> {
    let workdir = base.join("codex-workdir");
    fs::create_dir_all(&workdir).map_err(|e| e.to_string())?;

    let child = Command::new(&config.codex_command)
        .args(&config.codex_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start codex app-server: {e}"))?;
    let mut child = ChildCleanup::new(child);

    let stdout = child
        .child_mut()?
        .stdout
        .take()
        .ok_or_else(|| "failed to open codex app-server stdout".to_string())?;
    let mut stdin = child
        .child_mut()?
        .stdin
        .take()
        .ok_or_else(|| "failed to open codex app-server stdin".to_string())?;
    let rx = start_stdout_reader(stdout);
    let deadline = Instant::now() + Duration::from_millis(config.codex_timeout_ms.max(1_000));

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "romajiagent",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true,
                "requestAttestation": false,
                "optOutNotificationMethods": []
            }
        }
    });
    write_json_rpc_line(&mut stdin, &initialize)?;
    recv_json_until(&rx, deadline, &mut stdin, 1)?;

    let mut thread_params = serde_json::json!({
        "ephemeral": true,
        "cwd": workdir.display().to_string(),
        "runtimeWorkspaceRoots": [],
        "approvalPolicy": "never",
        "sandbox": "read-only",
        "baseInstructions": "You are a Japanese text conversion service for Romaji Agent. Never use tools, shell commands, file reads, or network browsing. Return only valid JSON for the requested schema."
    });
    if let Some(model) = config
        .codex_model
        .as_ref()
        .filter(|model| !model.is_empty())
    {
        thread_params["model"] = Value::String(model.clone());
    }
    let thread_start = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "thread/start",
        "params": thread_params
    });
    write_json_rpc_line(&mut stdin, &thread_start)?;
    let thread_result = recv_json_until(&rx, deadline, &mut stdin, 2)?;
    let thread_id = thread_result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("thread/start did not return a thread id: {thread_result}"))?;

    let turn_start = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "turn/start",
        "params": {
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": codex_prompt(request),
                "text_elements": []
            }],
            "outputSchema": transform_response_schema()
        }
    });
    write_json_rpc_line(&mut stdin, &turn_start)?;
    let turn_result = recv_json_until(&rx, deadline, &mut stdin, 3)?;
    let turn_id = turn_result
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("turn/start did not return a turn id: {turn_result}"))?
        .to_string();

    let text = wait_for_codex_turn(&rx, deadline, &mut stdin, &turn_id)?;
    parse_transform_response_text(&text)
}

fn infer_with_config(
    config: &AppConfig,
    base: &std::path::Path,
    request: &TransformRequest,
) -> Result<TransformResponse, String> {
    match Backend::parse(&config.backend) {
        Backend::Sidecar => infer_with_sidecar(config, request),
        Backend::CodexAppServer => infer_with_codex_app_server(config, base, request),
        Backend::Fallback => Err("fallback backend selected".into()),
    }
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
            result.normalized_raw,
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

fn ensure_command_succeeded(status: ExitStatus, action: &str) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!("{action} exited with {status}"))
    }
}

fn paste_active_app() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to keystroke \"v\" using command down",
            ])
            .status()
            .map_err(|e| e.to_string())?;
        return ensure_command_succeeded(status, "paste command");
    }

    #[cfg(target_os = "windows")]
    {
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v')"])
            .status()
            .map_err(|e| e.to_string())?;
        return ensure_command_succeeded(status, "paste command");
    }

    #[cfg(target_os = "linux")]
    {
        let status = Command::new("sh")
            .args(["-c", "command -v xdotool >/dev/null && xdotool key ctrl+v"])
            .status()
            .map_err(|e| e.to_string())?;
        return ensure_command_succeeded(status, "paste command");
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

pub fn do_transform(raw: &str) -> Result<TransformResult, String> {
    do_transform_with_save(raw, false)
}

pub fn do_transform_with_save(raw: &str, save_preview: bool) -> Result<TransformResult, String> {
    let started = Instant::now();
    let base = ensure_layout()?;
    let config = load_config(&base);
    let memory = fs::read_to_string(base.join("memory.md")).unwrap_or_default();

    let normalize_started = Instant::now();
    let normalized = normalize_input(raw);
    let normalize_ms = normalize_started.elapsed().as_millis();

    let request = TransformRequest {
        raw: normalized.normalized_raw.clone(),
        memory: memory.clone(),
        context: context_now(),
        kana_candidate: Some(normalized.kana_candidate.clone()),
    };

    let infer_started = Instant::now();
    let response = infer_with_config(&config, &base, &request)
        .unwrap_or_else(|_| fallback_convert(&normalized.normalized_raw, &memory));
    let infer_ms = infer_started.elapsed().as_millis();

    let result = TransformResult {
        id: Uuid::new_v4().to_string(),
        raw: normalized.raw,
        normalized_raw: normalized.normalized_raw,
        kana_candidate: normalized.kana_candidate,
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

    if save_preview {
        save_transform(&base, &result, false)?;
    }

    Ok(result)
}

#[tauri::command]
fn transform_text(raw: String) -> Result<TransformResult, String> {
    do_transform_with_save(&raw, true)
}

#[tauri::command]
fn preview_text(raw: String) -> Result<TransformResult, String> {
    do_transform(&raw)
}

#[tauri::command]
fn accept_transform(
    result: TransformResult,
    final_text: String,
    paste: bool,
) -> Result<AcceptOutcome, String> {
    let base = ensure_layout()?;
    let mut accepted = result;
    accepted.final_text = final_text.clone();
    save_transform(&base, &accepted, true)?;
    set_clipboard(&final_text)?;
    let paste_error = if paste {
        paste_active_app().err()
    } else {
        None
    };
    Ok(AcceptOutcome {
        clipboard_updated: true,
        pasted: paste && paste_error.is_none(),
        paste_error,
    })
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
            preview_text,
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
        assert_eq!(
            normalization::normalize_whitespace("  kyou   mtg\tde\nhanasita todo  "),
            "kyou mtg de hanasita todo"
        );
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
            backend: "codex_app_server".into(),
            ..Default::default()
        };
        let text = toml::to_string(&config).unwrap();
        let parsed: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.shortcut_macos, "Cmd+Shift+J");
        assert_eq!(Backend::parse(&parsed.backend), Backend::CodexAppServer);
        assert_eq!(parsed.sidecar_args.len(), 2);
    }

    #[test]
    fn legacy_config_uses_new_defaults() {
        let text = r#"
shortcut_macos = "Cmd+Shift+K"
shortcut_other = "Ctrl+Shift+K"
sidecar_command = "/path/to/sidecar"
sidecar_args = ["--temperature", "0.1"]
model_path = "/path/to/model.gguf"
"#;
        let parsed: AppConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.shortcut_macos, "Cmd+Shift+K");
        assert_eq!(parsed.sidecar_command.as_deref(), Some("/path/to/sidecar"));
        assert_eq!(parsed.backend, "sidecar");
        assert_eq!(parsed.codex_command, "codex");
        assert_eq!(parsed.codex_args, vec!["app-server"]);
    }

    #[test]
    fn backend_parses_aliases() {
        assert_eq!(Backend::parse("codex"), Backend::CodexAppServer);
        assert_eq!(Backend::parse("codex-app-server"), Backend::CodexAppServer);
        assert_eq!(Backend::parse("fallback"), Backend::Fallback);
        assert_eq!(Backend::parse("sidecar"), Backend::Sidecar);
    }

    #[test]
    fn parses_codex_agent_message_notification() {
        let mut accumulated = String::new();
        let delta = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread",
                "turnId": "turn",
                "itemId": "item",
                "delta": "{\"converted\":\"今日\""
            }
        });
        assert!(text_from_codex_notification(&delta, "turn", &mut accumulated).is_none());
        assert_eq!(accumulated, "{\"converted\":\"今日\"");

        let completed = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "item/completed",
            "params": {
                "threadId": "thread",
                "turnId": "turn",
                "completedAtMs": 0,
                "item": {
                    "type": "agentMessage",
                    "id": "item",
                    "text": "{\"converted\":\"今日\",\"refined\":\"今日はよい天気です。\",\"confidence\":0.9}",
                    "phase": null,
                    "memoryCitation": null
                }
            }
        });

        let text = text_from_codex_notification(&completed, "turn", &mut accumulated).unwrap();
        let response = parse_transform_response_text(&text).unwrap();
        assert_eq!(response.converted, "今日");
        assert_eq!(response.refined, "今日はよい天気です。");
    }

    #[test]
    fn rejects_codex_server_request_but_ignores_notifications() {
        let mut output = Vec::new();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "exec/approval",
            "params": {}
        });
        reject_server_request(&mut output, &request).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(response["id"], 42);
        assert_eq!(response["error"]["code"], -32601);

        output.clear();
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "item/completed",
            "params": {}
        });
        reject_server_request(&mut output, &notification).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn parses_transform_response_from_wrapped_text() {
        let response = parse_transform_response_text(
            r#"Here is the JSON:
{"converted":"明日","refined":"明日です。","confidence":0.8}
Done."#,
        )
        .unwrap();

        assert_eq!(response.converted, "明日");
        assert_eq!(response.refined, "明日です。");
        assert_eq!(response.confidence, 0.8);
    }

    #[test]
    fn sidecar_args_adds_model_path_when_missing() {
        let config = AppConfig {
            sidecar_args: vec!["--max-tokens".into(), "64".into()],
            model_path: Some("/models/lfm.gguf".into()),
            ..Default::default()
        };

        assert_eq!(
            sidecar_args(&config),
            vec!["--max-tokens", "64", "--model", "/models/lfm.gguf"]
        );
    }

    #[test]
    fn sidecar_args_does_not_duplicate_explicit_model() {
        let config = AppConfig {
            sidecar_args: vec!["--model".into(), "/explicit/model.gguf".into()],
            model_path: Some("/config/model.gguf".into()),
            ..Default::default()
        };

        assert_eq!(
            sidecar_args(&config),
            vec!["--model", "/explicit/model.gguf"]
        );
    }

    #[test]
    fn db_schema_initializes() {
        let temp = std::env::temp_dir().join(format!("romajiagent-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp).unwrap();
        init_db(&temp).unwrap();
        assert!(temp.join("db.sqlite").exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn ensure_command_succeeded_reports_failure() {
        #[cfg(windows)]
        let status = Command::new("cmd")
            .args(["/C", "exit", "7"])
            .status()
            .unwrap();
        #[cfg(not(windows))]
        let status = Command::new("sh").args(["-c", "exit 7"]).status().unwrap();

        let error = ensure_command_succeeded(status, "paste command").unwrap_err();
        assert!(error.contains("paste command exited with"));
    }

    #[test]
    #[ignore = "loads the configured local GGUF model and writes to ~/.romaji-agent"]
    fn e2e_configured_sidecar_transform() {
        let result = transform_text("kyou mtg de hanasita todo".into()).unwrap();

        assert_eq!(result.raw, "kyou mtg de hanasita todo");
        assert!(!result.converted.trim().is_empty());
        assert!(!result.refined.trim().is_empty());
        assert!(
            result.confidence > 0.25,
            "expected configured sidecar, got fallback-like confidence {} with refined {:?}",
            result.confidence,
            result.refined
        );
    }
}
