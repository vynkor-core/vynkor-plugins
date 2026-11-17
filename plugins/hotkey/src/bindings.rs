//! The binding store and trigger grammar.
//!
//! A binding is an operator-chosen id (`"ptt"`, `"mute"`) mapped to a
//! global key combo plus a human description. Ids are the stable handle
//! every consumer keys on — the daemon's `DAEMON_PLUGIN_PTT_BINDING` must
//! equal a binding id here — while triggers are what the OS actually
//! watches.
//!
//! Triggers arrive in operator spelling (`"Super+X"`, `"Ctrl+Shift+Space"`)
//! and normalize to the XDG portal's `<MOD>+key` form (`LOGO+x`,
//! `CTRL+SHIFT+space`). Normalization is total: anything it can't map is a
//! hard error at bind time, never a silently-dead shortcut.

use std::sync::Mutex;

/// One registered global shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Stable handle consumers subscribe by. Immutable after creation.
    pub id: String,
    /// Operator-spelling trigger (`"Super+X"`), as bound.
    pub trigger: String,
    /// Human label surfaced in portal UIs and `hotkey_list`.
    pub description: String,
}

/// Order-stable binding registry shared between action handlers and the
/// portal worker (which needs the full list on every rebind).
#[derive(Default)]
pub struct BindingStore {
    inner: Mutex<Vec<Binding>>,
}

impl BindingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a binding. Re-binding an existing id replaces
    /// its trigger/description in place (id order stays stable).
    pub fn set(&self, binding: Binding) {
        let mut inner = self.inner.lock().expect("binding store poisoned");
        match inner.iter_mut().find(|b| b.id == binding.id) {
            Some(slot) => *slot = binding,
            None => inner.push(binding),
        }
    }

    /// Drop a binding; `true` when something was actually removed.
    pub fn remove(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().expect("binding store poisoned");
        let before = inner.len();
        inner.retain(|b| b.id != id);
        inner.len() != before
    }

    pub fn get(&self, id: &str) -> Option<Binding> {
        self.inner
            .lock()
            .expect("binding store poisoned")
            .iter()
            .find(|b| b.id == id)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("binding store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Snapshot for `hotkey_list` and portal rebinds.
    pub fn snapshot(&self) -> Vec<Binding> {
        self.inner.lock().expect("binding store poisoned").clone()
    }
}

/// Validate an operator-supplied binding id: short, lowercase-ish, no
/// whitespace — it doubles as an event-payload field and a config key.
pub fn validate_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid binding id '{id}': use 1-64 chars of [a-z0-9_.-]"
        ))
    }
}

/// Parse boot bindings from `HOTKEY_PLUGIN_BINDINGS`:
/// `ptt=Ctrl+Shift+Space;mute=XF86AudioMute`. Entries without `=` or with
/// an invalid trigger abort the whole plugin config (fail loud at boot,
/// not silent at press time).
pub fn parse_env_bindings(spec: &str) -> Result<Vec<Binding>, String> {
    let mut out = Vec::new();
    for entry in spec.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((id, trigger)) = entry.split_once('=') else {
            return Err(format!(
                "invalid HOTKEY_PLUGIN_BINDINGS entry '{entry}': expected id=TRIGGER"
            ));
        };
        let id = id.trim();
        validate_id(id)?;
        let trigger = normalize_trigger(trigger.trim())?;
        out.push(Binding {
            id: id.to_string(),
            trigger: trigger.clone(),
            description: format!("hotkey {id}"),
        });
    }
    Ok(out)
}

/// Normalize an operator trigger into the XDG portal form.
///
/// Grammar: whitespace-separated/tolerant `MOD+MOD+KEY` where modifiers
/// are Ctrl/Control, Super/Meta/Logo/Win, Alt, Shift (case-insensitive)
/// and KEY maps through [`normalize_key`]. At least one modifier and
/// exactly one non-modifier key are required — bare-letter global binds
/// would fire on every keystroke in every app.
pub fn normalize_trigger(raw: &str) -> Result<String, String> {
    let mut mods: Vec<&'static str> = Vec::new();
    let mut key: Option<String> = None;

    for part in raw.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let lower = part.to_ascii_lowercase().replace([' ', '_', '-'], "");
        let modifier = match lower.as_str() {
            "ctrl" | "control" | "cmdorctrl" => Some("CTRL"),
            "super" | "meta" | "logo" | "win" => Some("LOGO"),
            "alt" | "option" => Some("ALT"),
            "shift" => Some("SHIFT"),
            _ => None,
        };
        if let Some(m) = modifier {
            if !mods.contains(&m) {
                mods.push(m);
            }
            continue;
        }
        if key.replace(normalize_key(part)?).is_some() {
            return Err(format!(
                "trigger '{raw}' has more than one non-modifier key"
            ));
        }
    }

    let key = key.ok_or_else(|| format!("trigger '{raw}' needs one non-modifier key"))?;
    if mods.is_empty() {
        return Err(format!(
            "trigger '{raw}' needs at least one modifier (Ctrl/Super/Alt/Shift)"
        ));
    }
    let mut out = mods.join("+");
    out.push('+');
    out.push_str(&key);
    Ok(out)
}

/// Map one key name to its XDG keysym spelling. Covers what a voice
/// pipeline realistically binds: letters/digits, F-keys, navigation and
/// punctuation clusters, XF86 media keys (also passed through verbatim so
/// exotic ones don't need code changes).
fn normalize_key(raw: &str) -> Result<String, String> {
    let key = raw.trim();
    let punctuation = match key {
        ")" => "parenright",
        "!" => "exclam",
        "@" => "at",
        "#" => "numbersign",
        "$" => "dollar",
        "%" => "percent",
        "^" => "asciicircum",
        "&" => "ampersand",
        "*" => "asterisk",
        "(" => "parenleft",
        ":" => "colon",
        ";" => "semicolon",
        "+" => "plus",
        "=" => "equal",
        "<" => "less",
        "," => "comma",
        "_" => "underscore",
        "-" => "minus",
        ">" => "greater",
        "." => "period",
        "?" => "question",
        "/" => "slash",
        "~" => "asciitilde",
        "`" => "grave",
        "{" => "braceleft",
        "]" => "bracketright",
        "[" => "bracketleft",
        "|" => "bar",
        "\\" => "backslash",
        "}" => "braceright",
        "\"" => "quotedbl",
        "'" => "apostrophe",
        _ => "",
    };
    if !punctuation.is_empty() {
        return Ok(punctuation.to_string());
    }

    let normalized = key.to_ascii_lowercase().replace([' ', '-', '/'], "");
    if let Some(mapped) = static_key(&normalized) {
        return Ok(mapped.to_string());
    }
    let n = normalized.as_str();
    if n.len() == 1 && n.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(n.to_string());
    }
    if n.len() >= 2
        && n.starts_with('f')
        && n[1..]
            .parse::<u8>()
            .map(|n| (1..=35).contains(&n))
            .unwrap_or(false)
    {
        return Ok(n.to_ascii_uppercase());
    }
    if n.starts_with("num") && n.len() == 4 && n[3..].parse::<u8>().is_ok() {
        return Ok(format!("KP_{}", &n[3..]));
    }
    Err(format!("unsupported key '{other_key}'", other_key = key))
}

/// Keys whose portal spelling differs from their lowercase name. Anything
/// outside this table falls through to the letter/digit/F-key/numpad rules.
fn static_key(normalized: &str) -> Option<&'static str> {
    Some(match normalized {
        "space" => "space",
        "tab" => "Tab",
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page_Up",
        "pagedown" => "Page_Down",
        "left" | "leftarrow" => "Left",
        "right" | "rightarrow" => "Right",
        "up" | "uparrow" => "Up",
        "down" | "downarrow" => "Down",
        "volumeup" => "XF86AudioRaiseVolume",
        "volumedown" => "XF86AudioLowerVolume",
        "volumemute" | "mute" => "XF86AudioMute",
        "medianext" | "medianexttrack" => "XF86AudioNext",
        "mediaprev" | "mediaprevioustrack" => "XF86AudioPrev",
        "mediaplaypause" => "XF86AudioPlay",
        "printscreen" => "Print",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_the_charset() {
        assert!(validate_id("ptt").is_ok());
        assert!(validate_id("voice-push_1.x").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("Push To Talk").is_err());
        assert!(validate_id("UPPER").is_err());
        assert!((1..100).any(|n| validate_id(&"a".repeat(n)).is_err()));
    }

    #[test]
    fn env_bindings_parse_into_a_store_shaped_list() {
        let parsed = parse_env_bindings("ptt=Ctrl+Shift+Space; mute=Super+M ;").expect("parse ok");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "ptt");
        assert_eq!(parsed[0].trigger, "CTRL+SHIFT+space");
        assert_eq!(parsed[1].id, "mute");
        assert_eq!(parsed[1].trigger, "LOGO+m");
    }

    #[test]
    fn env_bindings_fail_loud_on_garbage() {
        let err = parse_env_bindings("noequals").unwrap_err();
        assert!(err.contains("expected id=TRIGGER"), "{err}");
        let err = parse_env_bindings("ptt=Space").unwrap_err();
        assert!(err.contains("at least one modifier"), "{err}");
    }

    #[test]
    fn triggers_normalize_to_portal_form() {
        assert_eq!(
            normalize_trigger("Ctrl+Shift+Space").unwrap(),
            "CTRL+SHIFT+space"
        );
        assert_eq!(normalize_trigger("super + f9").unwrap(), "LOGO+F9");
        assert_eq!(normalize_trigger("Meta+A").unwrap(), "LOGO+a");
        assert_eq!(
            normalize_trigger("Alt+VolumeUp").unwrap(),
            "ALT+XF86AudioRaiseVolume"
        );
        assert_eq!(normalize_trigger("Ctrl+?").unwrap(), "CTRL+question");
        // Duplicate modifiers collapse instead of erroring.
        assert_eq!(normalize_trigger("Ctrl+Control+a").unwrap(), "CTRL+a");
    }

    #[test]
    fn triggers_reject_unusable_combos() {
        assert!(
            normalize_trigger("Space").is_err(),
            "bare letter fires everywhere"
        );
        assert!(
            normalize_trigger("Ctrl").is_err(),
            "modifier alone binds nothing"
        );
        assert!(
            normalize_trigger("Ctrl+a+b").is_err(),
            "two keys is not a chord"
        );
        assert!(normalize_trigger("Ctrl+F36").is_err(), "out-of-range F-key");
        assert!(normalize_trigger("Ctrl+NumpadUnknown").is_err());
    }

    #[test]
    fn store_set_replaces_in_place_and_removes() {
        let store = BindingStore::new();
        store.set(Binding {
            id: "ptt".into(),
            trigger: "CTRL+a".into(),
            description: "first".into(),
        });
        store.set(Binding {
            id: "mute".into(),
            trigger: "LOGO+m".into(),
            description: String::new(),
        });
        store.set(Binding {
            id: "ptt".into(),
            trigger: "CTRL+b".into(),
            description: "second".into(),
        });
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2, "replace must not duplicate");
        assert_eq!(snap[0].trigger, "CTRL+b", "ptt keeps its slot");
        assert_eq!(snap[0].description, "second");

        assert!(store.remove("ptt"));
        assert!(!store.remove("ptt"), "double remove reports false");
        assert_eq!(store.get("mute").unwrap().trigger, "LOGO+m");
        assert!(store.get("ptt").is_none());
    }
}
