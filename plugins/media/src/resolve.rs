//! Which player does a request without `player` mean? Pure decision logic,
//! fed a snapshot of every MPRIS player's state.
//!
//! There is no configured default: "pause the music" means whatever is
//! playing right now, so the answer is read from live state each call.

use std::time::Instant;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    /// MPRIS `PlaybackStatus` (`Playing` / `Paused` / `Stopped`).
    pub status: String,
    /// Whether the player's audio stream is actually heard (running,
    /// unmuted, volume > 0). `None` when streams were not consulted or no
    /// stream matched.
    pub audible: Option<bool>,
    /// Last time the watcher saw its `PlaybackStatus` change.
    pub last_change: Option<Instant>,
}

impl Candidate {
    fn playing(&self) -> bool {
        self.status == "Playing"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Act on exactly one player (next, seek, volume, status, ...).
    One,
    /// Act on every playing player (pause, stop). May be empty — pausing
    /// when nothing plays is a no-op, not an error.
    AllPlaying,
}

/// Playing players narrowed to the audible ones when that narrows without
/// emptying — MPRIS alone reports a muted browser tab as `Playing`.
fn heard(cands: &[Candidate]) -> Vec<&Candidate> {
    let playing: Vec<&Candidate> = cands.iter().filter(|c| c.playing()).collect();
    let audible: Vec<&Candidate> = playing.iter().copied().filter(|c| c.audible == Some(true)).collect();
    if audible.is_empty() { playing } else { audible }
}

/// True when [`choose`] would need stream info to break a tie — lets the
/// caller skip the `pw-dump` spawn in the common single-player case.
pub fn needs_audibility(cands: &[Candidate]) -> bool {
    cands.iter().filter(|c| c.playing()).count() > 1
}

pub fn choose(cands: &[Candidate], intent: Intent) -> Result<Vec<String>, String> {
    if cands.is_empty() {
        return Err("ERR_MEDIA_NO_PLAYERS: no MPRIS players available".to_string());
    }
    let names = |v: &[&Candidate]| v.iter().map(|c| c.name.clone()).collect::<Vec<_>>();

    let heard = heard(cands);
    if intent == Intent::AllPlaying {
        // Pause/stop every *playing* player, audible or not: a muted tab
        // still counts as music the user asked to stop.
        return Ok(cands.iter().filter(|c| c.playing()).map(|c| c.name.clone()).collect());
    }
    match heard.as_slice() {
        [one] => return Ok(vec![one.name.clone()]),
        [] => {}
        many => {
            return Err(format!(
                "ERR_MEDIA_AMBIGUOUS: several players are playing, pass `player`: {:?}",
                names(many)
            ))
        }
    }

    // Nothing plays: the player most recently active is what "resume" /
    // "next track" refers to.
    let mut paused: Vec<&Candidate> = cands.iter().filter(|c| c.status == "Paused").collect();
    paused.sort_by(|a, b| b.last_change.cmp(&a.last_change).then_with(|| a.name.cmp(&b.name)));
    if let Some(first) = paused.first() {
        return Ok(vec![first.name.clone()]);
    }
    if let [only] = cands {
        return Ok(vec![only.name.clone()]);
    }
    Err(format!(
        "ERR_MEDIA_AMBIGUOUS: no player is playing or paused, pass `player`: {:?}",
        cands.iter().map(|c| c.name.as_str()).collect::<Vec<_>>()
    ))
}

/// Resolve an explicit `player` param against the available bus names:
/// exact name, `spotify` for `org.mpris.MediaPlayer2.spotify`, or a unique
/// instance for `firefox` -> `org.mpris.MediaPlayer2.firefox.instance_1_424`.
pub fn match_requested(requested: &str, available: &[String]) -> Result<String, String> {
    const PREFIX: &str = "org.mpris.MediaPlayer2.";
    let r = requested.trim();
    if available.iter().any(|a| a == r) {
        return Ok(r.to_string());
    }
    let short = r.strip_prefix(PREFIX).unwrap_or(r).to_ascii_lowercase();
    let full = format!("{PREFIX}{short}").to_ascii_lowercase();
    if let Some(a) = available.iter().find(|a| a.to_ascii_lowercase() == full) {
        return Ok(a.clone());
    }
    let instance_prefix = format!("{full}.");
    let hits: Vec<&String> = available
        .iter()
        .filter(|a| a.to_ascii_lowercase().starts_with(&instance_prefix))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!("ERR_MEDIA_PLAYER_NOT_FOUND: '{r}' not available (have: {available:?})")),
        many => Err(format!("ERR_MEDIA_AMBIGUOUS: '{r}' matches several players: {many:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn c(name: &str, status: &str) -> Candidate {
        Candidate { name: name.into(), status: status.into(), audible: None, last_change: None }
    }

    #[test]
    fn empty_is_no_players() {
        assert!(choose(&[], Intent::One).unwrap_err().starts_with("ERR_MEDIA_NO_PLAYERS"));
    }

    #[test]
    fn single_playing_wins_over_alphabet() {
        // The old default picked `mpd` here because it sorts first.
        let cands = [c("mpd", "Paused"), c("spotify", "Playing")];
        assert_eq!(choose(&cands, Intent::One).unwrap(), vec!["spotify"]);
    }

    #[test]
    fn two_playing_is_ambiguous_without_stream_info() {
        let cands = [c("firefox", "Playing"), c("spotify", "Playing")];
        let err = choose(&cands, Intent::One).unwrap_err();
        assert!(err.starts_with("ERR_MEDIA_AMBIGUOUS"), "{err}");
        assert!(err.contains("firefox") && err.contains("spotify"), "{err}");
    }

    #[test]
    fn audibility_breaks_the_tie() {
        let mut ff = c("firefox", "Playing");
        ff.audible = Some(false); // muted tab
        let mut sp = c("spotify", "Playing");
        sp.audible = Some(true);
        assert_eq!(choose(&[ff, sp], Intent::One).unwrap(), vec!["spotify"]);
    }

    #[test]
    fn all_silent_players_stay_ambiguous() {
        let mut a = c("a", "Playing");
        a.audible = Some(false);
        let mut b = c("b", "Playing");
        b.audible = Some(false);
        assert!(choose(&[a, b], Intent::One).unwrap_err().starts_with("ERR_MEDIA_AMBIGUOUS"));
    }

    #[test]
    fn all_playing_returns_every_playing_player() {
        let mut ff = c("firefox", "Playing");
        ff.audible = Some(false);
        let cands = [ff, c("mpd", "Paused"), c("spotify", "Playing")];
        assert_eq!(choose(&cands, Intent::AllPlaying).unwrap(), vec!["firefox", "spotify"]);
    }

    #[test]
    fn all_playing_is_empty_when_nothing_plays() {
        let cands = [c("mpd", "Paused")];
        assert!(choose(&cands, Intent::AllPlaying).unwrap().is_empty());
    }

    #[test]
    fn most_recently_paused_is_resumed() {
        let now = Instant::now();
        let mut old = c("aaa", "Paused");
        old.last_change = Some(now - Duration::from_secs(60));
        let mut recent = c("zzz", "Paused");
        recent.last_change = Some(now);
        assert_eq!(choose(&[old, recent], Intent::One).unwrap(), vec!["zzz"]);
    }

    #[test]
    fn paused_without_history_falls_back_to_name() {
        let cands = [c("zzz", "Paused"), c("aaa", "Paused"), c("stopped", "Stopped")];
        assert_eq!(choose(&cands, Intent::One).unwrap(), vec!["aaa"]);
    }

    #[test]
    fn lone_stopped_player_is_used() {
        assert_eq!(choose(&[c("mpd", "Stopped")], Intent::One).unwrap(), vec!["mpd"]);
    }

    #[test]
    fn several_stopped_players_are_ambiguous() {
        let cands = [c("a", "Stopped"), c("b", "Stopped")];
        assert!(choose(&cands, Intent::One).unwrap_err().starts_with("ERR_MEDIA_AMBIGUOUS"));
    }

    #[test]
    fn needs_audibility_only_for_multiple_playing() {
        assert!(!needs_audibility(&[c("a", "Playing"), c("b", "Paused")]));
        assert!(needs_audibility(&[c("a", "Playing"), c("b", "Playing")]));
    }

    fn avail() -> Vec<String> {
        [
            "org.mpris.MediaPlayer2.firefox.instance_1_424",
            "org.mpris.MediaPlayer2.mpd",
            "org.mpris.MediaPlayer2.spotify",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn requested_exact_and_short_names() {
        assert_eq!(match_requested("org.mpris.MediaPlayer2.mpd", &avail()).unwrap(), "org.mpris.MediaPlayer2.mpd");
        assert_eq!(match_requested("Spotify", &avail()).unwrap(), "org.mpris.MediaPlayer2.spotify");
        assert_eq!(
            match_requested("firefox", &avail()).unwrap(),
            "org.mpris.MediaPlayer2.firefox.instance_1_424"
        );
    }

    #[test]
    fn requested_multiple_instances_is_ambiguous() {
        let mut a = avail();
        a.push("org.mpris.MediaPlayer2.firefox.instance_1_999".into());
        assert!(match_requested("firefox", &a).unwrap_err().starts_with("ERR_MEDIA_AMBIGUOUS"));
    }

    #[test]
    fn requested_unknown_is_not_found() {
        assert!(match_requested("vlc", &avail()).unwrap_err().starts_with("ERR_MEDIA_PLAYER_NOT_FOUND"));
    }
}
