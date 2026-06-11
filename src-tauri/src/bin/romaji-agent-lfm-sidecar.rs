use std::{
    io::{self, BufRead, Write},
    num::NonZeroU32,
    path::PathBuf,
};

use anyhow::{anyhow, bail, Context, Result};
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaModel},
    sampling::LlamaSampler,
};
use romajiagent_lib::protocol::{TransformRequest, TransformResponse};

#[derive(Debug)]
struct SidecarOptions {
    model: PathBuf,
    ctx_size: u32,
    max_tokens: usize,
    temperature: f32,
    threads: Option<i32>,
}

impl SidecarOptions {
    fn parse() -> Result<Self> {
        let mut model = None;
        let mut ctx_size = 2048;
        let mut max_tokens = 256;
        let mut temperature = 0.1;
        let mut threads = None;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--model" => model = Some(PathBuf::from(required_value(&mut args, "--model")?)),
                "--ctx-size" => {
                    ctx_size = required_value(&mut args, "--ctx-size")?
                        .parse()
                        .context("invalid --ctx-size")?;
                }
                "--max-tokens" => {
                    max_tokens = required_value(&mut args, "--max-tokens")?
                        .parse()
                        .context("invalid --max-tokens")?;
                }
                "--temperature" => {
                    temperature = required_value(&mut args, "--temperature")?
                        .parse()
                        .context("invalid --temperature")?;
                }
                "--threads" => {
                    threads = Some(
                        required_value(&mut args, "--threads")?
                            .parse()
                            .context("invalid --threads")?,
                    );
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ if arg.starts_with("--model=") => {
                    model = Some(PathBuf::from(arg.trim_start_matches("--model=")));
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        let model = model.ok_or_else(|| anyhow!("--model is required"))?;
        if !model.exists() {
            bail!("model does not exist: {}", model.display());
        }
        if max_tokens == 0 {
            bail!("--max-tokens must be greater than zero");
        }

        Ok(Self {
            model,
            ctx_size,
            max_tokens,
            temperature,
            threads,
        })
    }
}

fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

fn print_help() {
    eprintln!(
        "Usage: romaji-agent-lfm-sidecar --model <path.gguf> [--ctx-size 2048] [--max-tokens 256] [--temperature 0.1] [--threads N]"
    );
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let options = SidecarOptions::parse()?;
    let backend = LlamaBackend::init().context("initializing llama backend")?;

    let mut model_params = LlamaModelParams::default();
    #[cfg(target_os = "macos")]
    {
        model_params = model_params.with_n_gpu_layers(u32::MAX);
    }

    let model = LlamaModel::load_from_file(&backend, &options.model, &model_params)
        .with_context(|| format!("loading model {}", options.model.display()))?;

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }

        let request: TransformRequest =
            serde_json::from_str(&line).context("parsing transform request")?;
        let response =
            infer(&backend, &model, &options, &request).context("running transform inference")?;
        serde_json::to_writer(&mut stdout, &response).context("writing response json")?;
        writeln!(stdout).context("writing response newline")?;
        stdout.flush().context("flushing response")?;
    }

    Ok(())
}

fn infer(
    backend: &LlamaBackend,
    model: &LlamaModel,
    options: &SidecarOptions,
    request: &TransformRequest,
) -> Result<TransformResponse> {
    let prompt = build_prompt(request);
    let mut tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .context("tokenizing prompt")?;
    if tokens.is_empty() {
        bail!("prompt produced no tokens");
    }

    let ctx_size = NonZeroU32::new(options.ctx_size).ok_or_else(|| anyhow!("ctx size is zero"))?;
    let mut context_params = LlamaContextParams::default()
        .with_n_ctx(Some(ctx_size))
        .with_n_batch(options.ctx_size)
        .with_n_ubatch(options.ctx_size.min(512));
    if let Some(threads) = options.threads {
        context_params = context_params
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
    }

    let mut context = model
        .new_context(backend, context_params)
        .context("creating llama context")?;
    let mut batch = LlamaBatch::new(options.ctx_size as usize, 1);
    batch
        .add_sequence(&tokens, 0, false)
        .context("preparing prompt batch")?;
    context.decode(&mut batch).context("decoding prompt")?;

    let mut sampler = if options.temperature <= 0.0 {
        LlamaSampler::greedy()
    } else {
        LlamaSampler::chain_simple([
            LlamaSampler::temp(options.temperature),
            LlamaSampler::top_p(0.9, 1),
            LlamaSampler::dist(42),
        ])
    };

    let mut generated = String::new();
    let mut decoder = encoding_rs::UTF_8.new_decoder();
    for _ in 0..options.max_tokens {
        let token = sampler.sample(&context, -1);
        if model.is_eog_token(token) {
            break;
        }
        sampler.accept(token);
        tokens.push(token);
        generated.push_str(
            &model
                .token_to_piece(token, &mut decoder, false, None)
                .context("decoding generated token")?,
        );

        batch.clear();
        let pos = i32::try_from(tokens.len() - 1).context("token position overflow")?;
        batch
            .add(token, pos, &[0], true)
            .context("preparing generated token batch")?;
        context
            .decode(&mut batch)
            .context("decoding generated token")?;

        if generated.contains('}') && generated.contains("\"refined\"") {
            let full = format!(r#"{{"converted":"{generated}"#);
            if parse_response(&full).is_ok() {
                break;
            }
        }
    }

    let full_json = format!(r#"{{"converted":"{generated}"#);
    parse_response(&full_json)
        .or_else(|_| repair_response(&full_json))
        .with_context(|| format!("model did not emit valid response JSON: {full_json:?}"))
}

fn filter_memory(memory: &str, raw: &str) -> String {
    let raw_lower = raw.to_lowercase();
    memory
        .lines()
        .filter(|line| {
            if let Some((before, _)) = line.split_once("->") {
                let key = before.trim().to_lowercase();
                !key.is_empty() && raw_lower.contains(&key)
            } else {
                false
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_prompt(request: &TransformRequest) -> String {
    let relevant = filter_memory(&request.memory, &request.raw);
    let memory_section = if relevant.is_empty() {
        String::new()
    } else {
        format!("\nREPLACE: {relevant}\n")
    };
    format!(
        r#"<|startoftext|>Romaji to Japanese. Each romaji word maps to one Japanese word. Convert every word faithfully.
{memory_section}
INPUT: sumimasen chotto okuremasu
OUTPUT: {{"converted":"すみませんちょっと遅れます","refined":"すみません、ちょっと遅れます。","confidence":0.95}}

INPUT: ashita made ni repo wo dasanakya
OUTPUT: {{"converted":"明日までにレポを出さなきゃ","refined":"明日までにレポを出さなきゃ。","confidence":0.9}}

INPUT: kyou no kaigi de shiryou wo kakunin shita
OUTPUT: {{"converted":"今日の会議で資料を確認した","refined":"今日の会議で資料を確認した。","confidence":0.95}}

INPUT: {raw}
OUTPUT: {{"converted":""#,
        memory_section = memory_section,
        raw = request.raw
    )
}

fn parse_response(text: &str) -> Result<TransformResponse> {
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("missing JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("missing JSON object end"))?;
    let response: TransformResponse =
        serde_json::from_str(&text[start..=end]).context("parsing response JSON")?;
    if response.converted.trim().is_empty() {
        bail!("converted is empty");
    }
    if response.refined.trim().is_empty() {
        bail!("refined is empty");
    }
    Ok(TransformResponse {
        converted: response.converted,
        refined: response.refined,
        confidence: response.confidence.clamp(0.0, 1.0),
    })
}

fn repair_response(text: &str) -> Result<TransformResponse> {
    let converted = extract_json_string_field(text, "converted")
        .ok_or_else(|| anyhow!("missing converted field"))?;
    let refined = extract_json_string_field(text, "refined").unwrap_or_else(|| converted.clone());
    let confidence = extract_json_number_field(text, "confidence").unwrap_or(0.5);

    if converted.trim().is_empty() || refined.trim().is_empty() {
        bail!("repaired response is empty");
    }

    Ok(TransformResponse {
        converted,
        refined,
        confidence: confidence.clamp(0.0, 1.0),
    })
}

fn extract_json_string_field(text: &str, field: &str) -> Option<String> {
    let marker = format!("\"{field}\"");
    let marker_index = text.find(&marker)?;
    let after_marker = &text[marker_index + marker.len()..];
    let colon_index = after_marker.find(':')?;
    let mut chars = after_marker[colon_index + 1..].chars().peekable();
    while matches!(chars.peek(), Some(ch) if ch.is_whitespace()) {
        chars.next();
    }
    if chars.next()? != '"' {
        return None;
    }

    let mut value = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            match ch {
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                '/' => value.push('/'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                other => value.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(value),
            other => value.push(other),
        }
    }
    Some(value)
}

fn extract_json_number_field(text: &str, field: &str) -> Option<f32> {
    let marker = format!("\"{field}\"");
    let marker_index = text.find(&marker)?;
    let after_marker = &text[marker_index + marker.len()..];
    let colon_index = after_marker.find(':')?;
    let after_colon = after_marker[colon_index + 1..].trim_start();
    let end = after_colon
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '-'))
        .unwrap_or(after_colon.len());
    after_colon[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_from_model_text() {
        let response = parse_response(
            r#"Here is JSON: {"converted":"今日 mtg","refined":"今日のミーティング。","confidence":1.2}"#,
        )
        .unwrap();

        assert_eq!(response.converted, "今日 mtg");
        assert_eq!(response.refined, "今日のミーティング。");
        assert_eq!(response.confidence, 1.0);
    }

    #[test]
    fn repairs_partial_json_from_model_text() {
        let response = repair_response(
            r#"{"converted":"あすという着が定着してしまった","refined":"あすという着が定着してしまった","confidence":100"#,
        )
        .unwrap();

        assert_eq!(response.converted, "あすという着が定着してしまった");
        assert_eq!(response.refined, "あすという着が定着してしまった");
        assert_eq!(response.confidence, 1.0);
    }

    #[test]
    fn prompt_contains_request_fields() {
        let request = TransformRequest {
            raw: "kyou mtg".into(),
            memory: "mtg -> ミーティング".into(),
            context: romajiagent_lib::protocol::TransformContext {
                timestamp: chrono::Utc::now(),
                os: "macos".into(),
                app_name: None,
                process_id: None,
                window_title: None,
            },
            kana_candidate: Some("きょう mtg".into()),
        };

        let prompt = build_prompt(&request);
        assert!(prompt.contains("kyou mtg"));
        assert!(prompt.contains("mtg -> ミーティング"));
    }
}
