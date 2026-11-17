//! Sentence segmentation for `tts_speak_stream` (EXI-02): split a paragraph
//! into speakable units so synthesis starts streaming after the first phrase
//! instead of after the whole text.
//!
//! Rules, in decision order for each ender run (`. ! ? …`, runs like `?!` /
//! `...` consume together):
//!
//! 1. A lone `.` with digits on both sides (`3.14`) never ends anything.
//! 2. A lone `.` after a single uppercase letter (`A.` in initials) continues.
//! 3. A lone `.` whose last word is a known RU/EN abbreviation (`т.д.`,
//!    `etc.`) continues.
//! 4. A lone `.` behind a short stem (≤3 chars) closes only when the next
//!    non-whitespace char looks like a sentence start (uppercase / digit);
//!    longer stems close unconditionally.
//! 5. Multi-char runs and `!` / `?` always close.
//! 6. Single `…` is lenient: it closes only when the next char does not look
//!    like a continuation (lowercase letter keeps the sentence flowing).

/// Split into sentences; each unit is whitespace-collapsed and trimmed.
pub fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if !matches!(c, '.' | '!' | '?' | '…') {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < chars.len() && matches!(chars[j + 1], '.' | '!' | '?' | '…') {
            j += 1;
        }
        let run_len = j - i + 1;
        let next_non_ws = (j + 1..chars.len()).find(|&k| !chars[k].is_whitespace());

        let closes = match c {
            '.' => {
                if i > 0 && chars[i - 1].is_ascii_digit() && run_len == 1 {
                    // Rule 1: decimals — digit right after the dot keeps flowing.
                    match next_non_ws {
                        Some(k) if chars[k].is_ascii_digit() => false,
                        _ => true,
                    }
                } else if run_len >= 2 {
                    // Rule 5: "..." / "?!" always close.
                    true
                } else {
                    match next_non_ws {
                        None => true,
                        Some(k) if chars[k] == '\n' => true,
                        Some(k) => {
                            let word = last_word(&chars[start..=j]);
                            if is_single_initial(&word) || is_known_abbreviation(&word) {
                                false
                            } else if word.chars().count() <= 3 {
                                // Rule 4: short stems wait for a capital.
                                chars[k].is_uppercase() || chars[k].is_ascii_digit()
                            } else {
                                true
                            }
                        }
                    }
                }
            }
            '…' if run_len == 1 => match next_non_ws {
                None => true,
                Some(k) => !chars[k].is_lowercase(),
            },
            _ => true,
        };

        if closes {
            let unit: String = chars[start..=j].iter().collect();
            let collapsed = unit.split_whitespace().collect::<Vec<_>>().join(" ");
            if !collapsed.is_empty() {
                out.push(collapsed);
            }
            start = j + 1;
        }
        i = j + 1;
    }
    let tail: String = chars[start.min(chars.len())..].iter().collect();
    let collapsed = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    if !collapsed.is_empty() {
        out.push(collapsed);
    }
    out
}

/// Last whitespace-delimited word of the window, lowercased, trailing dots
/// kept (`"т.д."` → `"т.д."`). Casing matters for [`is_single_initial`], so
/// the caller lowercases only here, at the boundary.
fn last_word(window: &[char]) -> String {
    let start = window
        .iter()
        .rposition(|&c| c.is_whitespace())
        .map_or(0, |p| p + 1);
    window[start..].iter().collect::<String>().to_lowercase()
}

fn is_single_initial(word_lower: &str) -> bool {
    let Some(stem) = word_lower.strip_suffix('.') else {
        return false;
    };
    stem.chars().count() == 1
}

fn is_known_abbreviation(word_lower: &str) -> bool {
    const ABBREVS: &[&str] = &[
        // Russian
        "т.д", "т.п", "т.е", "т.н", "т.к", "и т.д", "и т.п", "др", "пр", "г", "ул", "кв", "ст",
        "рис", "см", "табл", "стр", "ч", "мин", "сек", "проф", "акад", "доц", "им", "руб", "коп",
        // English
        "etc", "eg", "ie", "vs", "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "no", "fig",
        "approx", "dept", "est", "inc", "ltd", "co",
    ];
    match word_lower.strip_suffix('.') {
        Some(stem) => ABBREVS.contains(&stem),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        split_sentences(s)
    }

    #[test]
    fn splits_simple_ru_and_en() {
        assert_eq!(
            split("Привет мир. Как дела?"),
            vec!["Привет мир.", "Как дела?"]
        );
        assert_eq!(
            split("First one! Second one? Third one... done"),
            vec!["First one!", "Second one?", "Third one...", "done"]
        );
        assert_eq!(
            split("Один предложение. Два."),
            vec!["Один предложение.", "Два."]
        );
    }

    #[test]
    fn keeps_russian_abbreviations_together() {
        assert_eq!(
            split("Я приду в 5 ч. и т.д. Потом уйду."),
            vec!["Я приду в 5 ч. и т.д. Потом уйду."]
        );
        assert_eq!(
            split("Это т.н. случай. А это уже конец."),
            vec!["Это т.н. случай.", "А это уже конец."]
        );
        assert_eq!(
            split("Опоздал, т.к. проспал. Бывает."),
            vec!["Опоздал, т.к. проспал.", "Бывает."]
        );
    }

    #[test]
    fn keeps_decimals_and_initials() {
        assert_eq!(
            split("Pi is 3.14 roughly. Next"),
            vec!["Pi is 3.14 roughly.", "Next"]
        );
        assert_eq!(
            split("Цена 10 000.50 руб. Итог"),
            vec!["Цена 10 000.50 руб. Итог"]
        );
        assert_eq!(
            split("A. S. Pushkin wrote here. Done"),
            vec!["A. S. Pushkin wrote here.", "Done"]
        );
    }

    #[test]
    fn english_titles_stay_glued() {
        assert_eq!(
            split("Mr. Dr. Smith arrived. Late again."),
            vec!["Mr. Dr. Smith arrived.", "Late again."]
        );
        assert_eq!(
            split("We tested etc. and it worked. Fine"),
            vec!["We tested etc. and it worked.", "Fine"]
        );
    }

    #[test]
    fn short_lowercase_stems_wait_for_capitals() {
        assert_eq!(
            split("см. ниже раздел. Конец"),
            vec!["см. ниже раздел.", "Конец"]
        );
        assert_eq!(
            split("он вышел. мы остались"),
            vec!["он вышел.", "мы остались"],
            "long stems close even before lowercase"
        );
    }

    #[test]
    fn ellipsis_and_multiline() {
        assert_eq!(
            split("Думал… и решил. Вот так. Всё"),
            vec!["Думал… и решил.", "Вот так.", "Всё"]
        );
        #[rustfmt::skip]
        assert_eq!(split("Стоп! Хватит… Всё"), vec!["Стоп!", "Хватит…", "Всё"],
            "… + capital closes — fine for streaming latency");
        assert_eq!(
            split("Wait... what?! Really"),
            vec!["Wait...", "what?!", "Really"]
        );
    }

    #[test]
    fn empty_and_whitespace_only() {
        assert!(split("").is_empty());
        assert!(split("   \n\t ").is_empty());
        assert_eq!(split("no terminator"), vec!["no terminator"]);
    }
}
