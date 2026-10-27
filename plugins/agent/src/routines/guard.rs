//! Deterministic checks on what mining learns. The mining model is a small
//! local one and it does get on/off backwards ("зажигай" for an off call,
//! "Зажёг" as the reply), so nothing it writes is trusted unchecked.
//!
//! All texts here are already [`crate::smalltalk::normalize`]d, except
//! replies, which are normalized on the way in.

use serde_json::Value;

use crate::smalltalk::normalize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    On,
    Off,
}

/// Param keys whose value says whether the call switches something on or off.
const SWITCH_KEYS: &[&str] = &["value", "state", "on", "enabled", "enable", "power"];
/// Param keys that name the operation of a multi-command tool.
const COMMAND_KEYS: &[&str] = &["command", "action", "op", "method"];
const READ_VERBS: &[&str] = &["get", "read", "status", "state", "list", "query", "info", "ping"];

/// Word stems; off is checked first because "выключи" contains "вкл".
const OFF_STEMS: &[&str] = &["выкл", "откл", "погас", "погаш", "потуш", "выруб"];
const ON_STEMS: &[&str] = &["вкл", "зажг", "зажег", "зажиг", "зажеч", "вруб"];

/// Words that make a phrase lean on earlier conversation or bend its
/// meaning ("выключи всё кроме светодиода").
const CONTEXT_WORDS: &[&str] = &[
    "его", "ее", "их", "это", "этот", "эту", "эти", "тот", "ту", "те", "там", "тоже", "снова",
    "опять", "еще", "обратно", "кроме", "не", "но", "только", "то", "же", "самое", "it", "that",
    "again", "too", "not", "same",
];

/// A call that reads state: its answer depends on the result, so no reply
/// written in advance can be right.
pub fn is_read_call(tool: &str, params: &Value) -> bool {
    let reads = |s: &str| {
        let s = s.to_ascii_lowercase();
        READ_VERBS.iter().any(|v| s == *v || s.starts_with(&format!("{v}_")))
    };
    // `phone.get_battery`: the part after the plugin prefix names the operation
    reads(tool.rsplit('.').next().unwrap_or(tool))
        || COMMAND_KEYS
            .iter()
            .filter_map(|k| params.get(*k).and_then(Value::as_str))
            .any(reads)
}

/// Searches the top level, then one level down (`{"params": {"value": 0}}`).
pub fn call_polarity(params: &Value) -> Option<Polarity> {
    fn direct(m: &serde_json::Map<String, Value>) -> Option<Polarity> {
        SWITCH_KEYS.iter().find_map(|k| match m.get(*k)? {
            Value::Bool(b) => Some(if *b { Polarity::On } else { Polarity::Off }),
            Value::Number(n) => match n.as_f64()? {
                0.0 => Some(Polarity::Off),
                1.0 => Some(Polarity::On),
                _ => None,
            },
            Value::String(s) => match s.to_ascii_lowercase().as_str() {
                "on" | "true" | "1" => Some(Polarity::On),
                "off" | "false" | "0" => Some(Polarity::Off),
                _ => None,
            },
            _ => None,
        })
    }
    let m = params.as_object()?;
    direct(m).or_else(|| m.values().filter_map(Value::as_object).find_map(direct))
}

/// `None` when the text names no switch, or names both directions.
pub fn text_polarity(text: &str) -> Option<Polarity> {
    let (mut on, mut off) = (false, false);
    for w in normalize(text).split(' ') {
        if w == "off" || OFF_STEMS.iter().any(|s| w.starts_with(s)) {
            off = true;
        } else if w == "on" || ON_STEMS.iter().any(|s| w.starts_with(s)) {
            on = true;
        }
    }
    match (on, off) {
        (true, false) => Some(Polarity::On),
        (false, true) => Some(Polarity::Off),
        _ => None,
    }
}

/// "светодiод", "вturned": the small models mix in Latin look-alikes, and
/// qwen drifts into Chinese ("яркость关") — any letter outside Latin and
/// Cyrillic rejects the word.
fn mixed_script(w: &str) -> bool {
    let foreign = w.chars().any(|c| c.is_alphabetic() && !c.is_ascii_alphabetic() && !matches!(c, 'а'..='я' | 'ё'));
    foreign || w.chars().any(|c| c.is_ascii_alphabetic()) && w.chars().any(|c| matches!(c, 'а'..='я'))
}

/// A trigger phrase: at least two words, standalone, and for a switch call
/// it must say the call's own direction. This, not the model, decides
/// whether a phrase stands without earlier conversation.
pub fn phrase_ok(phrase: &str, call: Option<Polarity>) -> bool {
    let words: Vec<&str> = phrase.split(' ').filter(|w| !w.is_empty()).collect();
    if words.len() < 2 || words.iter().any(|w| CONTEXT_WORDS.contains(w) || mixed_script(w)) {
        return false;
    }
    call.is_none_or(|p| text_polarity(phrase) == Some(p))
}

/// Words that carry no object: "выключи, пожалуйста, светодиод".
const FILLER_WORDS: &[&str] = &["пожалуйста", "мне", "а", "ка", "давай", "please"];

/// Strips the inflection ending, so "лампу" and "лампа" meet.
fn stem(w: &str) -> &str {
    w.trim_end_matches(|c| "аеиоуыэюяйь".contains(c))
}

/// "светодиода" ~ "светодиод", but "свет" is not "светодиод".
fn same_word(a: &str, b: &str) -> bool {
    let (a, b) = (stem(a), stem(b));
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    !short.is_empty() && long.starts_with(short) && long.chars().count() - short.chars().count() <= 2
}

/// The words of a phrase that name what it acts on (not the switch verb).
fn objects(phrase: &str) -> Vec<&str> {
    phrase
        .split(' ')
        .filter(|w| !w.is_empty() && !FILLER_WORDS.contains(w) && text_polarity(w).is_none())
        .collect()
}

/// A model paraphrase may change the verb and the word order, never what is
/// acted on: it must name exactly the objects of one phrase the user really
/// said. Otherwise "выключи светодиод" grows "выключи свет", which later
/// switches the LED off when a real lamp is meant.
pub fn anchored(paraphrase: &str, seen: &[String]) -> bool {
    let mine = objects(paraphrase);
    seen.iter().any(|t| {
        let theirs = objects(t);
        mine.iter().all(|m| theirs.iter().any(|o| same_word(m, o)))
            && theirs.iter().all(|o| mine.iter().any(|m| same_word(m, o)))
    })
}

/// Stems of words that report a failure ("Такой команды нет").
const FAILURE_STEMS: &[&str] =
    &["не", "нет", "ошиб", "невозм", "нельз", "отказ", "error", "fail", "cannot", "not", "no"];

/// A reply is said only after the call succeeded, so it must never report a
/// failure; for a switch call it must also name the new state, because the
/// small model's "neutral" replies were mostly off-topic ("Команда на
/// лампочку"). Past two words it must name something the user named
/// (`seen`), or it drifts to other devices ("Да, свет выключен").
pub fn reply_ok(reply: &str, call: Option<Polarity>, seen: &[String]) -> bool {
    let text = normalize(reply);
    let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    let failure = words.iter().any(|w| {
        mixed_script(w) || FAILURE_STEMS.iter().any(|s| if s.len() <= 3 { w == s } else { w.starts_with(s) })
    });
    let named = |t: &String| objects(t).into_iter().any(|o| words.iter().any(|w| same_word(w, o)));
    let on_topic = words.len() <= 2 || seen.iter().any(named);
    // The user's words it uses must come from one phrase: across a misheard
    // "включи диот светозерк" and "включи светодиод" the model wrote
    // "Включил светодиод светозерк."
    let used: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| seen.iter().flat_map(|t| objects(t)).any(|o| same_word(w, o)))
        .collect();
    let one_phrase = used.is_empty()
        || seen.iter().any(|t| used.iter().all(|w| objects(t).iter().any(|o| same_word(w, o))));
    // With several phrases, only words the user said more than once: a
    // misheard "светозерк" said once gave "Диод светозерк включен".
    let in_texts = |w: &str| seen.iter().filter(|t| objects(t).iter().any(|o| same_word(w, o))).count();
    let repeated = seen.len() < 2
        || used.iter().all(|w| in_texts(w) >= 2)
        || !seen.iter().flat_map(|t| objects(t)).any(|o| in_texts(o) >= 2);
    // A place the user never named is invented: "Светодиод на двери включен".
    let no_new_place = words
        .iter()
        .all(|w| !PREPOSITIONS.contains(w) || seen.iter().any(|t| t.split(' ').any(|s| s == *w)));
    !failure
        && words.len() <= MAX_REPLY_WORDS
        && on_topic
        && one_phrase
        && repeated
        && no_new_place
        && call.is_none_or(|p| text_polarity(&text) == Some(p))
}

const PREPOSITIONS: &[&str] = &["в", "во", "на", "у", "под", "над", "возле", "около", "из", "от", "при", "для"];
/// A spoken confirmation; longer replies were where the details got invented.
const MAX_REPLY_WORDS: usize = 4;

/// Correct by construction; tops up what the model's replies left.
pub fn stock_replies(call: Option<Polarity>) -> &'static [&'static str] {
    match call {
        Some(Polarity::On) => &["Включил.", "Готово, включено.", "Включено."],
        Some(Polarity::Off) => &["Выключил.", "Готово, выключено.", "Выключено."],
        None => &["Готово.", "Сделано.", "Есть."],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use Polarity::*;

    #[test]
    fn read_calls_are_recognised() {
        let p = |c: &str| json!({"command": c, "device_id": "x"});
        assert!(is_read_call("mqtt_device_command", &p("get_state")));
        assert!(is_read_call("mqtt_device_command", &p("status")));
        assert!(!is_read_call("mqtt_device_command", &p("set_pin")));
        assert!(is_read_call("phone.get_battery", &json!({})));
        assert!(!is_read_call("pixel-6pro.flashlight", &json!({"on": true})));
    }

    #[test]
    fn call_polarity_reads_nested_switch_values() {
        assert_eq!(call_polarity(&json!({"params": {"name": "led_d1", "value": 0}})), Some(Off));
        assert_eq!(call_polarity(&json!({"on": true})), Some(On));
        assert_eq!(call_polarity(&json!({"state": "OFF"})), Some(Off));
        assert_eq!(call_polarity(&json!({"level": 40})), None);
        assert_eq!(call_polarity(&json!({"value": 55})), None);
    }

    #[test]
    fn text_polarity_does_not_read_vykl_as_vkl() {
        assert_eq!(text_polarity("выключи светодиод"), Some(Off));
        assert_eq!(text_polarity("Выключил"), Some(Off));
        assert_eq!(text_polarity("включи свет"), Some(On));
        assert_eq!(text_polarity("Зажег"), Some(On));
        assert_eq!(text_polarity("Теперь включен"), Some(On));
        assert_eq!(text_polarity("погасить"), Some(Off));
        assert_eq!(text_polarity("Готово"), None);
        assert_eq!(text_polarity("включи и выключи"), None);
        assert_eq!(text_polarity("lights off"), Some(Off));
        assert_eq!(text_polarity("online"), None);
    }

    /// The exact output qwen2.5:3b produced for a set_pin value 0 call.
    #[test]
    fn live_mining_output_for_an_off_call_is_filtered() {
        let kept: Vec<&str> = [
            "выключи",
            "выключи его",
            "выключи светодиод",
            "зажигай",
            "только светодиод",
            "выключи все кроме светодиода",
            "погасить",
            "светодиод выключить",
        ]
        .into_iter()
        .filter(|p| phrase_ok(p, Some(Off)))
        .collect();
        assert_eq!(kept, vec!["выключи светодиод", "светодиод выключить"]);

        let replies: Vec<&str> = [
            "Готово",
            "Зажег",
            "Выключил",
            "Теперь включен",
            "Заранее выключен",
            "Конечно, выкл светодиод",
        ]
        .into_iter()
        .filter(|r| reply_ok(r, Some(Off), &["выключи светодиод".to_string()]))
        .collect();
        assert_eq!(replies, vec!["Выключил", "Заранее выключен", "Конечно, выкл светодиод"]);
    }

    /// The second live run, after the polarity filter.
    #[test]
    fn paraphrases_must_keep_the_users_objects() {
        let seen = vec!["выключи светодиод".to_string()];
        let kept: Vec<&str> =
            ["выключи светодиод", "выключи свет", "выключи лампу", "погась свет", "светодиод погаси", "отключи светодиода"]
                .into_iter()
                .filter(|p| anchored(p, &seen))
                .collect();
        assert_eq!(kept, vec!["выключи светодиод", "светодиод погаси", "отключи светодиода"]);
        assert!(!phrase_ok("выключи то же самое", Some(Off)));
        assert!(anchored("включи лампу", &["зажги лампа".to_string()]));
        assert!(!anchored("включи лампу на кухне", &["включи лампу".to_string()]));
    }

    #[test]
    fn neutral_call_keeps_neutral_phrases_but_not_context_ones() {
        assert!(phrase_ok("яркость на половину", None));
        assert!(!phrase_ok("сделай тоже", None));
        assert!(!phrase_ok("ярче", None));
        assert!(!reply_ok("Не включил", Some(Off), &[]));
        assert!(!reply_ok("Не получилось", Some(Off), &[]));
        // the third live run, for the same off call
        assert!(!reply_ok("Такой команды нет", Some(Off), &[]));
        assert!(!reply_ok("Команда на лампочку", Some(Off), &[]));
        let seen = ["выключи светодиод".to_string()];
        assert!(reply_ok("Светодиод выключен", Some(Off), &seen));
        assert!(!reply_ok("Да, свет выключен, совсем тихо.", Some(Off), &seen));
        assert!(!reply_ok("Светодiод выключен", Some(Off), &seen), "Latin i");
        assert!(!reply_ok("Светодиод вturned OFF.", Some(Off), &seen));
        assert!(!phrase_ok("выключи 光照灯关", Some(Off)));
        assert!(!reply_ok("Светодиод 关 выключен", Some(Off), &seen));
        assert!(reply_ok("Выключил.", Some(Off), &[]));
        let misheard = vec!["включи диот светозерк".to_string(), "включи светодиод".to_string()];
        assert!(!reply_ok("Включил светодиод светозерк.", Some(On), &misheard), "words of two phrases");
        assert!(reply_ok("Включил светодиод.", Some(On), &misheard));
        // the third live run, once the mixing rule was in
        let live = vec!["включи диот светозерк".to_string(), "включи светодиод".to_string(), "светодиод включить".to_string()];
        let kept: Vec<&str> = [
            "Светодиод в доме включен.",
            "Диод светозерк включен.",
            "Диод на светодиоде включен.",
            "Светодиод на двери включен.",
            "В доме светодиод включен, свет есть.",
            "Светодиод дома включен, всё хорошо.",
            "Светодиод включен.",
        ]
        .into_iter()
        .filter(|r| reply_ok(r, Some(On), &live))
        .collect();
        assert_eq!(kept, vec!["Светодиод включен."]);
        let kitchen = vec!["включи свет на кухне".to_string()];
        assert!(reply_ok("Свет на кухне включен.", Some(On), &kitchen), "a place the user named");
        assert!(reply_ok("Готово.", None, &[]));
        assert!(!reply_ok("Ошибка устройства", None, &[]));
        assert!(reply_ok("Нормально, яркость 40", None, &["яркость 40".to_string()]), "\"нормально\" is not \"не\"");
        for p in [Some(On), Some(Off), None] {
            assert!(stock_replies(p).iter().all(|r| reply_ok(r, p, &[])));
        }
    }
}
