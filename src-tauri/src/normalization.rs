use serde::{Deserialize, Serialize};
use wana_kana::ConvertJapanese;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NormalizedInput {
    pub raw: String,
    pub normalized_raw: String,
    pub kana_candidate: String,
}

pub fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn normalize_input(input: &str) -> NormalizedInput {
    let normalized_raw = normalize_whitespace(input);
    let kana_candidate = romaji_runs_to_hiragana(&normalized_raw);
    NormalizedInput {
        raw: input.to_string(),
        normalized_raw,
        kana_candidate,
    }
}

fn romaji_runs_to_hiragana(input: &str) -> String {
    let mut output = String::new();
    let mut romaji_run = String::new();

    for ch in input.chars() {
        if ch.is_ascii_alphabetic() || ch == '\'' || ch == '-' {
            romaji_run.push(ch);
            continue;
        }

        if !romaji_run.is_empty() {
            output.push_str(&hiragana_or_original(&romaji_run));
            romaji_run.clear();
        }
        output.push(ch);
    }

    if !romaji_run.is_empty() {
        output.push_str(&hiragana_or_original(&romaji_run));
    }

    output
}

fn hiragana_or_original(romaji_run: &str) -> String {
    let converted = romaji_run.to_hiragana();
    if converted.chars().any(|ch| ch.is_ascii_alphabetic()) {
        romaji_run.to_string()
    } else {
        converted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn romaji_to_kana_keeps_phonetic_anchor() {
        let input = normalize_input("konoguraino kakikata anara dou??");
        assert!(input.kana_candidate.contains("このぐらいの"));
        assert!(input.kana_candidate.contains("かきかた"));
        assert!(input.kana_candidate.contains("あなら"));
        assert!(input.kana_candidate.contains("どう"));
    }

    #[test]
    fn preserves_punctuation_and_existing_japanese() {
        let input = normalize_input("今日は konogurai??");
        assert!(input.kana_candidate.contains("今日は"));
        assert!(input.kana_candidate.ends_with("??"));
    }

    #[test]
    fn keeps_memory_terms_available_in_raw_input() {
        let input = normalize_input("kyou mtg de todo");
        assert!(input.normalized_raw.contains("mtg"));
        assert!(input.raw.contains("mtg"));
    }

    #[test]
    fn preserves_english_like_tokens() {
        let input = normalize_input("romajinara english wo include suru kakikata ki naru.");
        assert!(input.kana_candidate.contains("ろまじなら"));
        assert!(input.kana_candidate.contains("english"));
        assert!(input.kana_candidate.contains("を"));
        assert!(input.kana_candidate.contains("include"));
        assert!(input.kana_candidate.contains("する"));
    }
}
