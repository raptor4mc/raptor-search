// algorithm/tokenize.rs
//
// Stopword removal + stemming for building the *search index*, and nothing
// else. The old code ran this over the raw page text and then displayed the
// output as the user-facing snippet, which is why snippets showed up as
// "endtoend requir busi valu" instead of real sentences.
//
// Rule for this module: its output (`index_tokens`) must never be shown to
// a user. It exists purely to feed Postgres's tsvector / your own inverted
// index. Display text always comes from algorithm::extract, never from here.

use once_cell::sync::Lazy;
use rust_stemmers::{Algorithm, Stemmer};

static STEMMER: Lazy<Stemmer> = Lazy::new(|| Stemmer::create(Algorithm::English));

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "in", "on", "at", "to", "for", "of", "with", "by",
    "from", "is", "it", "its", "was", "are", "be", "been", "has", "had", "have", "will", "would",
    "could", "should", "may", "might", "do", "does", "did", "not", "no", "so", "if", "as", "up",
    "out", "about", "into", "than", "then", "that", "this", "these", "those", "they", "them",
    "their", "there", "when", "where", "which", "who", "how", "what", "we", "you", "i", "he",
    "she", "my", "your", "our", "can", "also",
];

/// Stopword-filtered, stemmed tokens for indexing/matching. Not for display.
pub fn index_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|w| {
            let clean: String = w
                .chars()
                .filter(|c| c.is_alphabetic())
                .collect::<String>()
                .to_lowercase();
            if clean.is_empty() || STOPWORDS.contains(&clean.as_str()) {
                None
            } else {
                Some(STEMMER.stem(&clean).to_string())
            }
        })
        .collect()
}

pub fn index_text(text: &str) -> String {
    index_tokens(text).join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_stopwords_and_stems() {
        let out = index_text("This is an end-to-end encrypted email service");
        // "this", "is", "an" are stopwords; the rest get stemmed.
        assert!(!out.contains("this"));
        assert!(out.contains("encrypt"));
    }
}
