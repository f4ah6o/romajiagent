use std::io::{self, BufRead, Write};

use anyhow::{bail, Context, Result};
use romajiagent_lib::{
    do_transform_with_save,
    normalization::{normalize_input, NormalizedInput},
    TransformResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Json,
    Text,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        bail!("missing command");
    };

    match command.as_str() {
        "kana" => run_kana(args.collect()),
        "transform" => run_transform(args.collect()),
        "--help" | "-h" | "help" => {
            print_help();
            Ok(())
        }
        _ => bail!("unknown command: {command}"),
    }
}

fn run_kana(args: Vec<String>) -> Result<()> {
    let (mode, text) = parse_text_command(args)?;
    let normalized = normalize_input(&text);
    match mode {
        OutputMode::Json => print_json(&normalized),
        OutputMode::Text => {
            println!("{}", normalized.kana_candidate);
            Ok(())
        }
    }
}

fn run_transform(args: Vec<String>) -> Result<()> {
    let mut mode = OutputMode::Json;
    let mut use_stdin = false;
    let mut save = false;
    let mut text_parts = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--text" => mode = OutputMode::Text,
            "--json" => mode = OutputMode::Json,
            "--stdin" => use_stdin = true,
            "--save" => save = true,
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            _ => text_parts.push(arg),
        }
    }

    if use_stdin {
        if !text_parts.is_empty() {
            bail!("transform --stdin does not accept positional text");
        }
        let stdin = io::stdin();
        let mut stdout = io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line.context("reading stdin")?;
            if line.trim().is_empty() {
                continue;
            }
            let result = do_transform_with_save(&line, save).map_err(anyhow::Error::msg)?;
            write_transform(&mut stdout, &result, mode)?;
        }
        return Ok(());
    }

    if text_parts.is_empty() {
        bail!("transform requires text or --stdin");
    }
    let result = do_transform_with_save(&text_parts.join(" "), save).map_err(anyhow::Error::msg)?;
    write_transform(&mut io::stdout().lock(), &result, mode)
}

fn parse_text_command(args: Vec<String>) -> Result<(OutputMode, String)> {
    let mut mode = OutputMode::Json;
    let mut text_parts = Vec::new();

    for arg in args {
        match arg.as_str() {
            "--text" => mode = OutputMode::Text,
            "--json" => mode = OutputMode::Json,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => text_parts.push(arg),
        }
    }

    if text_parts.is_empty() {
        bail!("kana requires text");
    }
    Ok((mode, text_parts.join(" ")))
}

fn write_transform(
    stdout: &mut impl Write,
    result: &TransformResult,
    mode: OutputMode,
) -> Result<()> {
    match mode {
        OutputMode::Json => {
            serde_json::to_writer(&mut *stdout, result).context("writing json")?;
            writeln!(stdout).context("writing newline")?;
        }
        OutputMode::Text => {
            writeln!(stdout, "raw: {}", result.raw).context("writing raw")?;
            writeln!(stdout, "normalized_raw: {}", result.normalized_raw)
                .context("writing normalized_raw")?;
            writeln!(stdout, "kana_candidate: {}", result.kana_candidate)
                .context("writing kana_candidate")?;
            writeln!(stdout, "converted: {}", result.converted).context("writing converted")?;
            writeln!(stdout, "refined: {}", result.refined).context("writing refined")?;
            writeln!(stdout, "confidence: {:.2}", result.confidence)
                .context("writing confidence")?;
        }
    }
    Ok(())
}

fn print_json(value: &NormalizedInput) -> Result<()> {
    serde_json::to_writer(io::stdout().lock(), value).context("writing json")?;
    println!();
    Ok(())
}

fn print_help() {
    eprintln!(
        "Usage:\n  romaji-agent-cli kana [--json|--text] <text>\n  romaji-agent-cli transform [--json|--text] [--save] <text>\n  romaji-agent-cli transform [--json|--text] [--save] --stdin"
    );
}
