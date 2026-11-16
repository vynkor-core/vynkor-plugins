//! Small-talk fast path: a bare greeting, thanks or goodbye skips the tool
//! loop entirely — one chat call with the persona only, no catalog, no
//! embedding filter, no memory recall or extraction.
//!
//! Detection is a closed vocabulary, not a classifier: the goal qualifies
//! only when *every* word is a known small-talk word, so anything carrying
//! an instruction ("привет, включи свет") always takes the normal path.
//! A miss costs a normal goal; a false hit would drop a real request, so
//! the list errs short.
//!
//! Opt-in (`AGENT_PLUGIN_SMALL_TALK=on`); `AGENT_PLUGIN_SMALL_TALK_AGENT_ID`
//! optionally routes these calls to a cheaper `ai` profile.

pub const SMALL_TALK_ENV: &str = "AGENT_PLUGIN_SMALL_TALK";
pub const SMALL_TALK_AGENT_ENV: &str = "AGENT_PLUGIN_SMALL_TALK_AGENT_ID";

/// Longest goal (in words) that can still be small talk.
const MAX_WORDS: usize = 5;

/// Lowercase, `ё`→`е`. Bare acknowledgements ("да", "ок") are deliberately
/// absent: with session context they answer a pending question.
const VOCABULARY: &[&str] = &[
    // greetings
    "привет", "приветик", "здравствуй", "здравствуйте", "здорово", "хай", "салют",
    "доброе", "добрый", "доброй", "утро", "утра", "день", "вечер", "ночи",
    "hi", "hello", "hey", "yo", "good", "morning", "evening", "night",
    // thanks
    "спасибо", "спс", "благодарю", "пасиб", "мерси", "thanks", "thank", "thx", "you",
    // goodbye
    "пока", "покеда", "бывай", "до", "свидания", "встречи", "завтра", "спокойной",
    "bye", "goodbye", "later", "see", "ya",
    // softeners / how-are-you
    "большое", "огромное", "всем", "тебе", "вам", "друг", "бро", "как", "дела",
    "ты", "у", "тебя", "жизнь", "so", "much", "how", "are", "it", "going",
];

pub fn enabled() -> bool {
    matches!(
        std::env::var(SMALL_TALK_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "on" | "true" | "1" | "yes"
    )
}

/// Optional `ai` agent profile for small talk; `None` keeps the goal's own.
pub fn agent_id() -> Option<String> {
    std::env::var(SMALL_TALK_AGENT_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Canonical form of a goal for exact matching: lowercase, `ё`→`е`,
/// anything not alphanumeric → space, whitespace collapsed. Shared with
/// learned routines so both sides of a match normalize identically.
pub fn normalize(text: &str) -> String {
    let spaced: String = text
        .to_lowercase()
        .replace('ё', "е")
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_small_talk(goal: &str) -> bool {
    let normalized = normalize(goal);
    let words: Vec<&str> = normalized.split_whitespace().collect();
    !words.is_empty() && words.len() <= MAX_WORDS && words.iter().all(|w| VOCABULARY.contains(w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greetings_thanks_and_goodbyes_qualify() {
        for g in [
            "Привет!",
            "привет, как дела?",
            "Доброе утро 👋",
            "спасибо большое",
            "Thanks!",
            "пока",
            "до завтра",
            "Здорово, бро",
        ] {
            assert!(is_small_talk(g), "{g:?} should be small talk");
        }
    }

    #[test]
    fn anything_with_an_instruction_does_not() {
        for g in [
            "привет, включи свет",
            "спасибо, а теперь выключи",
            "как дела с батареей",
            "да",
            "ок",
            "",
            "   ",
            "hi there what's the weather",
            "привет привет привет привет привет привет",
        ] {
            assert!(!is_small_talk(g), "{g:?} must take the normal path");
        }
    }
}
