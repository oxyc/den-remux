//! Language tags and the audio track a session plays.
//!
//! Releases tag tracks every way there is: Matroska's ISO 639-2 (`eng`, `ger`), its newer BCP-47 (`en`,
//! `de-DE`), MP4's ISO 639-2/T (`deu`), and a browser asks in BCP-47 (`navigator.languages`). Everything
//! is compared as the ISO 639-1 code where there is one.

use crate::probe::AudioTrack;

/// ISO 639-1, ISO 639-2/B, ISO 639-2/T, English name — the languages releases actually carry.
const LANGS: &[(&str, &str, &str, &str)] = &[
    ("en", "eng", "eng", "English"),
    ("fi", "fin", "fin", "Finnish"),
    ("sv", "swe", "swe", "Swedish"),
    ("no", "nor", "nor", "Norwegian"),
    ("nb", "nob", "nob", "Norwegian"),
    ("nn", "nno", "nno", "Norwegian"),
    ("da", "dan", "dan", "Danish"),
    ("is", "ice", "isl", "Icelandic"),
    ("de", "ger", "deu", "German"),
    ("fr", "fre", "fra", "French"),
    ("es", "spa", "spa", "Spanish"),
    ("it", "ita", "ita", "Italian"),
    ("pt", "por", "por", "Portuguese"),
    ("nl", "dut", "nld", "Dutch"),
    ("pl", "pol", "pol", "Polish"),
    ("cs", "cze", "ces", "Czech"),
    ("sk", "slo", "slk", "Slovak"),
    ("hu", "hun", "hun", "Hungarian"),
    ("ro", "rum", "ron", "Romanian"),
    ("bg", "bul", "bul", "Bulgarian"),
    ("hr", "hrv", "hrv", "Croatian"),
    ("sr", "srp", "srp", "Serbian"),
    ("sl", "slv", "slv", "Slovenian"),
    ("el", "gre", "ell", "Greek"),
    ("tr", "tur", "tur", "Turkish"),
    ("ru", "rus", "rus", "Russian"),
    ("uk", "ukr", "ukr", "Ukrainian"),
    ("et", "est", "est", "Estonian"),
    ("lv", "lav", "lav", "Latvian"),
    ("lt", "lit", "lit", "Lithuanian"),
    ("ar", "ara", "ara", "Arabic"),
    ("he", "heb", "heb", "Hebrew"),
    ("fa", "per", "fas", "Persian"),
    ("hi", "hin", "hin", "Hindi"),
    ("ta", "tam", "tam", "Tamil"),
    ("te", "tel", "tel", "Telugu"),
    ("ja", "jpn", "jpn", "Japanese"),
    ("ko", "kor", "kor", "Korean"),
    ("zh", "chi", "zho", "Chinese"),
    ("th", "tha", "tha", "Thai"),
    ("vi", "vie", "vie", "Vietnamese"),
    ("id", "ind", "ind", "Indonesian"),
    ("ms", "may", "msa", "Malay"),
];

fn entry(tag: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    let primary = tag.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
    LANGS.iter().find(|(a, b, t, _)| primary == *a || primary == *b || primary == *t)
}

/// The tag as it is compared: ISO 639-1 where known (Bokmål and Nynorsk count as Norwegian), else its
/// primary subtag, lower-cased.
pub fn canonical(tag: &str) -> String {
    match entry(tag) {
        Some(("nb" | "nn", ..)) => "no".to_string(),
        Some((a, ..)) => a.to_string(),
        None => tag.split(['-', '_']).next().unwrap_or("").to_ascii_lowercase(),
    }
}

/// A name to show for the tag: English where known, else the tag.
pub fn name(tag: &str) -> String {
    entry(tag).map(|e| e.3.to_string()).unwrap_or_else(|| tag.to_string())
}

/// The audio track to play, counting audio tracks only: the first of `prefs` (most wanted first) that a
/// track is in — among those, the one flagged default — and never a commentary. Without a match, the
/// first default track that is not a commentary, then any that is not.
pub fn pick_audio(tracks: &[AudioTrack], prefs: &[String]) -> usize {
    let usable = || tracks.iter().enumerate().filter(|(_, t)| !t.commentary);
    for p in prefs {
        let want = canonical(p);
        let best = usable()
            .filter(|(_, t)| t.language.as_deref().map(canonical).as_deref() == Some(want.as_str()))
            .min_by_key(|(i, t)| (!t.default, *i));
        if let Some((i, _)) = best {
            return i;
        }
    }
    usable().min_by_key(|(i, t)| (!t.default, *i)).map_or(0, |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(lang: Option<&str>, default: bool, commentary: bool) -> AudioTrack {
        AudioTrack {
            codec: "A_AC3".into(),
            language: lang.map(String::from),
            channels: 6,
            name: None,
            default,
            commentary,
        }
    }

    #[test]
    fn every_spelling_of_a_language_compares_equal() {
        for tag in ["de", "DE-at", "ger", "deu", "de_CH"] {
            assert_eq!(canonical(tag), "de", "{tag}");
        }
        assert_eq!(canonical("nob"), "no");
        assert_eq!(canonical("nn-NO"), "no");
        assert_eq!(canonical("xyz"), "xyz", "an unknown tag compares as itself");
        assert_eq!(name("fin"), "Finnish");
        assert_eq!(name("xyz"), "xyz");
    }

    #[test]
    fn the_first_preference_a_track_is_in_wins() {
        let tracks = [
            track(Some("rus"), true, false),
            track(Some("eng"), false, false),
            track(Some("fin"), false, false),
        ];
        let prefs = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_audio(&tracks, &prefs(&["fi", "en"])), 2);
        assert_eq!(pick_audio(&tracks, &prefs(&["sv", "en-US"])), 1, "no Swedish, so English");
        assert_eq!(pick_audio(&tracks, &prefs(&["sv"])), 0, "nothing matches: the default track");
        assert_eq!(pick_audio(&tracks, &[]), 0);
    }

    #[test]
    fn a_commentary_is_never_picked_for_its_language() {
        let tracks = [
            track(Some("eng"), false, true),
            track(Some("fre"), true, false),
            track(Some("eng"), false, false),
        ];
        assert_eq!(pick_audio(&tracks, &["en".to_string()]), 2);
        // Among matches, the default-flagged one.
        let tracks = [track(Some("eng"), false, false), track(Some("eng"), true, false)];
        assert_eq!(pick_audio(&tracks, &["en".to_string()]), 1);
        // Only commentaries: track 0 rather than nothing.
        assert_eq!(pick_audio(&[track(None, true, true)], &[]), 0);
    }
}
