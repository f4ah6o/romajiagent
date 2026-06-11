use std::io::{self, BufRead, Write};

use anyhow::{bail, Context, Result};
use romajiagent_lib::protocol::{TransformRequest, TransformResponse};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn run() -> Result<()> {
    bail!("Apple Foundation Models is only available on macOS")
}

#[cfg(target_os = "macos")]
fn run() -> Result<()> {
    use ringo_fm::{GenerationOptions, LanguageModelSession, SystemLanguageModel};

    let options = SidecarOptions::parse()?;
    let model = SystemLanguageModel::default().context("creating system language model")?;
    let (is_available, unavailable_reason) = model.is_available();
    if !is_available {
        bail!(
            "Apple Foundation Models unavailable: {:?}",
            unavailable_reason.unwrap_or(ringo_fm::UnavailableReason::Unknown)
        );
    }

    let session = LanguageModelSession::new(
        Some(&model),
        Some(
            "You are a Japanese text conversion engine for Romaji Agent. Return only structured JSON matching the requested schema.",
        ),
        Vec::new(),
    )
    .context("creating language model session")?;
    session
        .prewarm(Some("Convert the following romaji"))
        .context("prewarming language model session")?;

    let generation_options = GenerationOptions::new()
        .with_temperature(options.temperature)
        .with_maximum_response_tokens(options.max_tokens);
    let schema = transform_response_schema_json();

    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let runtime = tokio::runtime::Runtime::new().context("creating tokio runtime")?;

    for line in stdin.lock().lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }

        let request: TransformRequest =
            serde_json::from_str(&line).context("parsing transform request")?;
        let response = runtime
            .block_on(infer(&session, &generation_options, &schema, &request))
            .context("running Apple Foundation Models inference")?;
        serde_json::to_writer(&mut stdout, &response).context("writing response json")?;
        writeln!(stdout).context("writing response newline")?;
        stdout.flush().context("flushing response")?;
    }

    Ok(())
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct SidecarOptions {
    max_tokens: u32,
    temperature: f64,
}

#[cfg(target_os = "macos")]
impl SidecarOptions {
    fn parse() -> Result<Self> {
        let mut max_tokens = 256;
        let mut temperature = 0.1;

        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
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
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                _ => bail!("unknown argument: {arg}"),
            }
        }

        if max_tokens == 0 {
            bail!("--max-tokens must be greater than zero");
        }
        if !(0.0..=2.0).contains(&temperature) {
            bail!("--temperature must be between 0.0 and 2.0");
        }

        Ok(Self {
            max_tokens,
            temperature,
        })
    }
}

#[cfg(target_os = "macos")]
fn required_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

#[cfg(target_os = "macos")]
fn print_help() {
    eprintln!("Usage: romaji-agent-apple-fm-sidecar [--max-tokens 256] [--temperature 0.1]");
}

#[cfg(target_os = "macos")]
async fn infer(
    session: &ringo_fm::LanguageModelSession,
    options: &ringo_fm::GenerationOptions,
    schema: &str,
    request: &TransformRequest,
) -> Result<TransformResponse> {
    let prompt = build_prompt(request);
    let json = match session
        .respond_with_json_schema(prompt.clone(), schema, options)
        .await
    {
        Ok(generated) => generated
            .to_json()
            .context("serializing generated content")?,
        Err(structured_error) => {
            let text = session
                .respond_with(prompt, options)
                .await
                .with_context(|| format!("requesting text response after structured response failed: {structured_error}"))?;
            extract_json_object(&text)
                .with_context(|| format!("structured response failed: {structured_error}; text response was not valid transform JSON"))?
        }
    };

    let mut response = parse_transform_response(&json)?;
    response.confidence = response.confidence.clamp(0.0, 1.0);
    Ok(response)
}

#[cfg(target_os = "macos")]
fn parse_transform_response(text: &str) -> Result<TransformResponse> {
    serde_json::from_str(text.trim()).with_context(|| format!("invalid response json: {text}"))
}

#[cfg(target_os = "macos")]
fn extract_json_object(text: &str) -> Result<String> {
    let trimmed = text.trim();
    if parse_transform_response(trimmed).is_ok() {
        return Ok(trimmed.to_string());
    }

    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("missing json object start"))?;
    let end = trimmed
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("missing json object end"))?;
    let candidate = &trimmed[start..=end];
    parse_transform_response(candidate)?;
    Ok(candidate.to_string())
}

#[cfg(target_os = "macos")]
fn transform_response_schema_json() -> String {
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
    .to_string()
}

#[cfg(target_os = "macos")]
fn build_prompt(request: &TransformRequest) -> String {
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
- Treat Raw as typed romaji input, not a free semantic prompt.
- Prefer the Kana candidate as the phonetic anchor when it is present.
- Fix only obvious typos when the phonetic context supports the correction.
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
