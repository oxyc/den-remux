//! Subtitles from den-subtitles, as HLS WebVTT renditions — what AirPlay and Cast receivers show, which a
//! page's own `<track>` never reaches.
//!
//! The master playlist is fixed when the session starts, so it names one rendition per language the
//! browser asked for, and nothing is fetched until a player opens one. Then den-remux asks den-subtitles
//! for the title with the release's OpenSubtitles hash, size and filename — the hints the Apple TV sends,
//! so an exact-encode match (in sync by construction) ranks first — and serves the first subtitle in that
//! language as one WebVTT segment spanning the film. A language with nothing to offer serves an empty
//! document rather than an error: the playlist has already promised it.
//!
//! The release's own text tracks (SRT, ASS/SSA, WebVTT, `mov_text`) are the fallback for a language den-subtitles has
//! nothing in, and add the languages only the release carries. They are written by the same ffmpeg runs that copy the
//! video (`job::Spec::text`), so they arrive as the video does — a rendition is a WebVTT segment per video segment,
//! made once the runs have read past it. [`OwnCues`] keeps what the runs wrote and which stretches of the film they
//! have covered.

use std::collections::{BTreeSet, HashMap};

use serde::Deserialize;

use crate::probe::SubtitleTrack;

/// OpenSubtitles' hash reads this much from each end of the file.
pub const HASH_CHUNK: u64 = 64 * 1024;
/// Renditions per session. A single digit: `sub<N>` names them.
pub const MAX_LANGUAGES: usize = 8;
/// Larger than any real subtitle file.
pub const MAX_SUBTITLE: u64 = 4 << 20;
pub const MAX_LIST: u64 = 1 << 20;
/// What a rendition serves when there is nothing for its language.
pub const EMPTY: &str = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";
/// The pseudo-language `plan()` files a track with no usable language tag under: BCP-47's own code for
/// "undetermined", so it is still a valid `LANGUAGE` attribute in the playlist it ends up in.
pub const UNDETERMINED: &str = "und";

/// The most an own track's WebVTT file may grow to before it is left unread: a feature film's cues are a few hundred KB.
pub const MAX_OWN: u64 = 8 << 20;
/// How close to a cue's window its coverage has to reach, seconds. The same tolerance as the playlist's keyframes.
const SNAP: f64 = 0.05;
/// A run's own track is trusted this far behind the video it has finished (seconds): the demuxer hands over a
/// subtitle block with the cluster it is in, and a block a moment before a keyframe can trail it.
pub const TRAIL: f64 = 1.0;

/// One thing a player can pick: a language, and where its cues come from.
#[derive(Clone, Debug, PartialEq)]
pub struct Rendition {
    /// As `lang::canonical` gives it.
    pub lang: String,
    /// den-subtitles is asked for it: the browser named the language and sent an install.
    pub den: bool,
    /// The release's own text track for it, as its index among the subtitle tracks (ffmpeg's `0:s:N`).
    pub own: Option<usize>,
}

/// A full track, read start to end: the best a track of a language can be, when the session's audio is in some
/// other language and the viewer needs the whole dialogue translated.
pub(crate) const FULL_TIER: u8 = 0;
/// SDH: a full track plus sound descriptions. Ranked the same whichever language the audio is in — it is still a
/// full track, just one also meant for a deaf or hard-of-hearing viewer.
const SDH_TIER: u8 = 1;
/// Forced, or foreign-parts-only: cues only over whatever is NOT in the session's own audio language. The best a
/// track can be when the audio is a dub in this very language, where a full track would just repeat it.
pub(crate) const FORCED_TIER: u8 = 2;

/// A release's own track, before any audio-aware reordering: mirrors the Apple TV's `subtitleRank` (full, then
/// SDH, then forced).
pub(crate) fn own_tier(t: &SubtitleTrack) -> u8 {
    match (t.forced, t.hearing_impaired) {
        (true, _) => FORCED_TIER,
        (false, true) => SDH_TIER,
        (false, false) => FULL_TIER,
    }
}

/// `tier`, read in the session's audio context. `dub` is whether the session's audio is itself a dub in the
/// track's own language — not the title's original language, which this never learns: a subtitle in the same
/// language the audio is already spoken in is either a dub's captions (where only the untranslated, forced parts
/// are wanted) or a same-language accessibility track over original audio (where the full track is wanted and
/// there usually is no forced alternative to confuse it with). Only the full/forced ends swap; SDH sits in the
/// middle either way, since it is a full track regardless of context.
pub(crate) fn in_context(tier: u8, dub: bool) -> u8 {
    if dub {
        FORCED_TIER - tier
    } else {
        tier
    }
}

/// Does the release's own track win rendition `n`'s playlist over den-subtitles' best candidate, given whether
/// the session's audio is a dub in this language? Own wins ties: it is official, already in sync, and costs no
/// download, so den-subtitles only overrides it when den's candidate ranks strictly better here — e.g. the
/// release's only own track is forced/foreign-parts-only but the audio is not a dub, so a full download wins, or
/// the audio IS a dub and den's listed candidate is flagged foreign-parts-only/looks-dubbed while the release's
/// own track is a full one, so the flagged download — a better fit for a dub — wins instead.
pub(crate) fn own_wins(own: &SubtitleTrack, den: Option<&Entry>, dub: bool) -> bool {
    let own_rank = in_context(own_tier(own), dub);
    match den {
        Some(e) => own_rank <= in_context(e.tier(), dub),
        None => true,
    }
}

/// The index of the best of `tracks`' own candidates in `lang`, for a session whose audio is `audio_lang` — or
/// `None` when the release has no USABLE text track in `lang`. Ties (two tracks at the same tier) keep the lower
/// index, so an earlier track is not displaced by a later one that ranks no better.
///
/// Outside a dub context, a forced/foreign-parts-only track is never a candidate at all — not even as a last
/// resort when it is the only track in its language. Offered plain, it captions only the foreign-language parts,
/// which over an otherwise-single-language film reads as "barely any subtitles", not "some subtitles": exactly
/// the Fauda complaint this fix started from, just aimed at the release's own track instead of a downloaded one.
/// A forced track is only ever right where the dub it accompanies supplies the rest — `own_wins` still applies
/// `in_context` to rank it against den-subtitles' candidate once it IS one.
fn best_own(tracks: &[SubtitleTrack], lang: &str, audio_lang: Option<&str>) -> Option<usize> {
    let dub = audio_lang == Some(lang);
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.text
                && t.language.as_deref().map(crate::lang::canonical).as_deref() == Some(lang)
                && (dub || !t.forced)
        })
        .min_by_key(|(_, t)| in_context(own_tier(t), dub))
        .map(|(n, _)| n)
}

/// The index of the best text track that names no language at all, among tracks that name none — or `None`
/// where there is no such track, or every one of them is forced. There is no language here to call a dub of, so
/// unlike `best_own` there is no `dub` context to flip forced and full: a forced, language-less track stays out
/// unconditionally, the same as a forced track outside a dub context.
fn best_undetermined(tracks: &[SubtitleTrack]) -> Option<usize> {
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            t.text
                && t.language.as_deref().map(crate::lang::canonical).is_none_or(|l| l.is_empty())
                && !t.forced
        })
        .min_by_key(|(_, t)| own_tier(t))
        .map(|(n, _)| n)
}

/// The renditions of a session, most wanted first, filling `MAX_LANGUAGES` in this order: the viewer's own first
/// preferred language (whatever named it — the player's requested languages, this browser's, or the install's),
/// then English — guaranteed a place even when the viewer named none, so the cap is never the reason it is
/// missing — then the viewer's other preferred languages in their own order, then the session's audio language,
/// then the rest of the release's languages. Without this order, the cap used to fall back to the release's own
/// track order once the viewer's list ran out, which reads as alphabetical past the first couple of languages
/// and silently dropped a preference the release's own tracks just happened to sort late (den-remux#34).
///
/// A bitmap track is never offered. A track with no usable language tag is filed under `UNDETERMINED` instead of
/// dropped — never a preferred pick (nothing ever asks for "und"), but a candidate the fill below reaches at the
/// file position its untagged track sits at, the same as a tagged language would: a release that has nothing in
/// any language this viewer or the cap would otherwise reach still has cues, which beats offering none (Fauda
/// S1E3's own 17 GB release: no embedded English, but an untagged full track that almost certainly is one).
/// `audio_lang` is also the session's own selected audio language (`lang::canonical`) for the tier pick within
/// one language's own tracks — full, SDH, or forced/foreign-parts-only — which is the best one: see `in_context`.
/// The pick here is provisional where den-subtitles is also asked (`den: true`): whether it actually plays over
/// a downloaded file is `Session::subtitle_playlist`'s call, made with den-subtitles' own answer in hand.
///
/// `den_list` is den-subtitles' own list for the title (fetched before the session answers, at no OpenSubtitles
/// cost — see `fetch_list`), never the OpenSubtitles quota itself. A language with no own track is promised a
/// `den: true` rendition only when `den_list` already names a candidate for it: without this, a viewer's
/// preferred language with nothing servable anywhere still got a slot on the strength of "den-subtitles may
/// have it", which the web menu then offered and loaded empty (the Fauda S1E3 phantom-English report). A
/// language the release's own track already covers (`own: Some`) is unaffected either way — it has something to
/// show regardless of what the list says. An empty `den_list` (the fetch failed, timed out, or genuinely found
/// nothing) is read the same as "nothing": the safe default is to drop a den-only rendition, not to promise one
/// on a guess.
pub fn plan(
    requested: &[String],
    den: bool,
    tracks: &[SubtitleTrack],
    audio_lang: Option<&str>,
    den_list: &[Entry],
) -> Vec<Rendition> {
    let mut own: Vec<(String, usize)> = Vec::new();
    for t in tracks {
        let lang = t.language.as_deref().map(crate::lang::canonical).filter(|l| !l.is_empty());
        let key = lang.as_deref().unwrap_or(UNDETERMINED);
        if t.text && !own.iter().any(|(l, _)| l == key) {
            let best = match &lang {
                Some(l) => best_own(tracks, l, audio_lang),
                None => best_undetermined(tracks),
            };
            if let Some(n) = best {
                own.push((key.to_string(), n));
            }
        }
    }
    let found = |lang: &str| own.iter().find(|(l, _)| l == lang).map(|(_, n)| *n);

    // The viewer's own first choice leads; English sits right behind it (or leads itself, where that choice is
    // already English or there is none) so the cap never has to pick between the two.
    fn prefer<'a>(preferred: &mut Vec<&'a str>, lang: &'a str) {
        if !preferred.contains(&lang) {
            preferred.push(lang);
        }
    }
    let mut preferred: Vec<&str> = Vec::new();
    if let Some(first) = requested.first() {
        prefer(&mut preferred, first);
    }
    prefer(&mut preferred, "en");
    for lang in requested {
        prefer(&mut preferred, lang);
    }
    if let Some(lang) = audio_lang {
        prefer(&mut preferred, lang);
    }

    let mut out: Vec<Rendition> = preferred
        .iter()
        .filter_map(|l| {
            let own = found(l);
            // Own-backed languages keep `den` as asked — den-subtitles may still out-rank the own track
            // (`own_wins`). A den-only slot needs `den_list` to already name this language.
            let den =
                den && (own.is_some() || den_list.iter().any(|e| crate::lang::canonical(&e.lang) == *l));
            (den || own.is_some()).then(|| Rendition { lang: (*l).to_string(), den, own })
        })
        .collect();
    for (lang, n) in &own {
        if out.len() < MAX_LANGUAGES && !out.iter().any(|r| r.lang == *lang) {
            out.push(Rendition { lang: lang.clone(), den: false, own: Some(*n) });
        }
    }
    out.truncate(MAX_LANGUAGES);
    out
}

/// Why `plan()` offered or dropped the release's subtitle track `n` — never part of the decision
/// itself, only an account of it, so a report of "subtitles are missing/wrong" can be answered from
/// the next session's log instead of needing the file: whether this release even HAS an embedded
/// full track in the reported language, and if it does, why it wasn't the one served. Calls `plan()`'s
/// own `best_own` rather than re-deriving the same rule by eye, so the two can never drift apart.
fn explain_track(
    tracks: &[SubtitleTrack],
    n: usize,
    renditions: &[Rendition],
    audio_lang: Option<&str>,
) -> String {
    let t = &tracks[n];
    let lang = t.language.as_deref().map(crate::lang::canonical).filter(|l| !l.is_empty());
    let reason = match &lang {
        None if !t.text => "dropped (bitmap, can't become WebVTT)".to_string(),
        // No language to be a dub of either: a forced, language-less track stays out regardless of what else
        // the release carries, the same as a forced tagged track outside a dub context.
        None if t.forced => "dropped (forced, no usable language tag)".to_string(),
        None => match best_undetermined(tracks) {
            Some(best_n) if best_n != n => {
                format!("dropped (track {best_n} ranks higher with no usable language tag)")
            }
            Some(_) if renditions.iter().any(|r| r.own == Some(n)) => "offered as und".to_string(),
            Some(_) => "kept as a candidate with no usable language tag, not used this session".to_string(),
            None => "dropped (no usable language tag)".to_string(),
        },
        Some(_) if !t.text => "dropped (bitmap, can't become WebVTT)".to_string(),
        Some(l) => match best_own(tracks, l, audio_lang) {
            Some(best_n) if best_n != n => format!("dropped (track {best_n} ranks higher for {l})"),
            Some(_) if renditions.iter().any(|r| r.own == Some(n)) => format!("offered as {l}"),
            // Kept by `plan()`'s own-candidate pass but not turned into a Rendition: not among
            // the browser's requested languages, or `MAX_LANGUAGES` was already spent on others.
            Some(_) => format!("kept as a candidate for {l}, not used this session"),
            // `best_own` excludes every candidate: this track, forced, is the only one in `l`, and the
            // session's audio is not a dub in `l` — offered plain it would caption only the foreign-
            // language parts, near-empty over an otherwise-single-language film.
            None => format!("dropped (forced, and the session's audio is not a dub in {l})"),
        },
    };
    let flags: Vec<&str> =
        [t.forced.then_some("forced"), t.default.then_some("default"), t.hearing_impaired.then_some("sdh")]
            .into_iter()
            .flatten()
            .collect();
    format!(
        "{n} lang={} codec={} [{}] title={:?} -> {reason}",
        lang.as_deref().unwrap_or(t.language.as_deref().unwrap_or("und")),
        t.codec,
        flags.join(","),
        t.name.as_deref().unwrap_or(""),
    )
}

/// One line, logged once per session: every subtitle stream the release carries — index, language,
/// codec, disposition flags, title — and why `plan()` offered or dropped each. Never a URL.
pub fn describe_subtitles(
    tracks: &[SubtitleTrack],
    renditions: &[Rendition],
    audio_lang: Option<&str>,
) -> String {
    if tracks.is_empty() {
        return "no subtitle tracks".to_string();
    }
    (0..tracks.len()).map(|n| explain_track(tracks, n, renditions, audio_lang)).collect::<Vec<_>>().join("; ")
}

/// The subtitle side of a session.
pub struct Subs {
    /// den-subtitles' install URL, sealed config included — a secret.
    pub base: String,
    /// The first half of the release's OpenSubtitles hash, from the head read when it was opened.
    pub head_sum: Option<u64>,
    pub cache: tokio::sync::Mutex<Cache>,
}

#[derive(Default)]
pub struct Cache {
    /// den-subtitles' answer for the title, once asked.
    pub list: Option<Vec<Entry>>,
    /// Each rendition's document, once made.
    pub docs: Vec<Option<String>>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Entry {
    pub url: String,
    #[serde(default)]
    pub lang: String,
    /// OpenSubtitles' own flag: cues only over the OTHER language's dialogue, not a full transcript of this
    /// one — the muxed `forced` flag's downloaded equivalent.
    #[serde(default, rename = "foreignPartsOnly")]
    pub foreign_parts_only: bool,
    /// den-subtitles' own read of the release string: made for a DUBBED release, so its cues cover only what
    /// that dub leaves untranslated. Ranked the same as `foreign_parts_only` — both mean "not a full track".
    #[serde(default, rename = "looksDubbed")]
    pub looks_dubbed: bool,
}

impl Entry {
    /// This entry, read the same way a release's own track is: `FORCED_TIER` when it covers only part of the
    /// dialogue (either flag), else `FULL_TIER` — den-subtitles has no SDH signal to offer, so there is no
    /// middle tier for a downloaded file. `in_context` still applies: the tier flips in a dub context the same
    /// way an own forced track's does.
    pub(crate) fn tier(&self) -> u8 {
        if self.foreign_parts_only || self.looks_dubbed {
            FORCED_TIER
        } else {
            FULL_TIER
        }
    }
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    subtitles: Vec<Entry>,
}

/// The sum of one end's little-endian u64 words: half of the OpenSubtitles hash.
pub fn chunk_sum(b: &[u8]) -> u64 {
    b.as_chunks::<8>().0.iter().fold(0u64, |acc, w| acc.wrapping_add(u64::from_le_bytes(*w)))
}

/// OpenSubtitles' movie hash: the file size plus both ends' sums, as 16 hex digits.
pub fn movie_hash(size: u64, head_sum: u64, tail_sum: u64) -> String {
    format!("{:016x}", size.wrapping_add(head_sum).wrapping_add(tail_sum))
}

/// `<install>/subtitles/<type>/<id>/videoHash=…&videoSize=…&filename=….json`: Stremio's extras are a path
/// segment, not a query.
pub fn list_url(
    base: &str,
    id: &str,
    hash: Option<&str>,
    size: Option<u64>,
    filename: &str,
) -> Option<String> {
    let mut url = reqwest::Url::parse(base).ok()?;
    let mut extras = Vec::new();
    if let Some(h) = hash {
        extras.push(format!("videoHash={h}"));
    }
    if let Some(s) = size {
        extras.push(format!("videoSize={s}"));
    }
    // One field of one segment: these would split the extras or the path (the Apple TV blanks them too).
    let safe: String =
        filename.chars().map(|c| if matches!(c, '/' | '&' | '=' | '?' | '#') { ' ' } else { c }).collect();
    extras.push(format!("filename={safe}"));
    url.path_segments_mut()
        .ok()?
        .push("subtitles")
        .push(crate::scout::kind(id))
        .push(id)
        .push(&format!("{}.json", extras.join("&")));
    Some(url.into())
}

pub fn parse_list(body: &[u8]) -> Vec<Entry> {
    serde_json::from_slice::<List>(body).map(|l| l.subtitles).unwrap_or_default()
}

/// The release identity `fetch_list` hashes and names it by: scout's title id, where its bytes can be
/// read from, the first half of its OpenSubtitles hash (from the head read when it was opened), its size
/// and its filename.
pub struct Release<'a> {
    pub id: &'a str,
    pub input: &'a str,
    pub head_sum: Option<u64>,
    pub size: Option<u64>,
    pub filename: &'a str,
}

/// den-subtitles' list for a title, with the release's hash, size and filename as hints — one request,
/// never counted against OpenSubtitles' own quota. Shared by `session::create` (to decide, before the
/// session answers, which `den: true` renditions `plan()` may promise) and `Session::list_subtitles`
/// (lazily, once a rendition is actually opened) so the hash and the request can never drift between the
/// two call sites.
pub async fn fetch_list(
    http: &reqwest::Client,
    source_http: &reqwest::Client,
    base: &str,
    release: &Release<'_>,
) -> Result<Vec<Entry>, String> {
    let hash = match (release.size, release.head_sum) {
        (Some(size), Some(head_sum)) if size >= HASH_CHUNK => {
            crate::probe::read_range(source_http, release.input, size - HASH_CHUNK, HASH_CHUNK)
                .await
                .ok()
                .filter(|t| t.len() as u64 == HASH_CHUNK)
                .map(|tail| movie_hash(size, head_sum, chunk_sum(&tail)))
        }
        _ => None,
    };
    let Some(url) = list_url(base, release.id, hash.as_deref(), release.size, release.filename) else {
        return Ok(Vec::new());
    };
    let resp = http.get(&url).send().await.map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("den-subtitles answered {}", resp.status().as_u16()));
    }
    crate::probe::read_capped(resp, MAX_LIST).await.map(|b| parse_list(&b))
}

/// The same entry, asked for as the caller's own last resort: den-subtitles refuses a real but
/// too-sparse-for-its-span track (a dubbed release's captions, say) on a plain request, so a language
/// with nothing else offered would otherwise come back empty. `?sparse=1` is only ever tried after
/// every candidate for a language has already been fetched and refused (see `Session::subtitle`) —
/// den-subtitles still refuses anything that isn't a real, merely-incomplete track (a cue-less body,
/// an expired link, a dead upload) regardless of this flag.
pub fn with_sparse(url: &str) -> String {
    match url.contains('?') {
        true => format!("{url}&sparse=1"),
        false => format!("{url}?sparse=1"),
    }
}

/// den-subtitles serves every subtitle as `.srt` or, the same document, `.vtt`.
pub fn vtt_url(url: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (url, None),
    };
    let path = path.strip_suffix(".srt").map_or_else(|| path.to_string(), |p| format!("{p}.vtt"));
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

/// Is `url` on one of `origins`, with no credentials? A subtitle URL comes from den-subtitles' answer,
/// so it is checked like one the browser sent.
pub fn on_origin(url: &str, origins: &[String]) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.username().is_empty()
            && u.password().is_none()
            && origins.contains(&u.origin().ascii_serialization())
    })
}

/// The document as an HLS WebVTT segment, or `None` if it is not WebVTT. A timestamp map pins its cue
/// times to the media timeline, which starts at 0 (the jobs run `-copyts -start_at_zero`).
pub fn for_hls(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim_start_matches('\u{feff}');
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    if !first.trim_end().starts_with("WEBVTT") {
        return None;
    }
    if rest.contains("X-TIMESTAMP-MAP") {
        return Some(text.to_string());
    }
    Some(format!("{}\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n{rest}", first.trim_end()))
}

/// One cue of an own track: its times in milliseconds on the film's timeline and the block as ffmpeg wrote it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub block: String,
}

/// `HH:MM:SS.mmm` or `MM:SS.mmm`, in milliseconds.
fn timestamp(s: &str) -> Option<u64> {
    let (clock, ms) = s.split_once('.')?;
    let mut parts = clock.split(':').rev();
    let seconds: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = parts.next().map_or(Some(0), |h| h.parse().ok())?;
    let ms: u64 = format!("{:0<3}", ms.chars().take(3).collect::<String>()).parse().ok()?;
    Some(((hours * 60 + minutes) * 60 + seconds) * 1000 + ms)
}

/// The cues of a WebVTT file ffmpeg is writing. Its muxer puts the blank line before each cue and writes a cue in one
/// go, so a file that ends in the middle of a line has a cue being written, which the next look reads; a cue with no
/// text yet is the same. Notes, styles and regions are not cues.
pub fn parse_cues(text: &str) -> Vec<Cue> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let mut blocks: Vec<&str> = text.split("\n\n").collect();
    if !text.ends_with('\n') {
        blocks.pop();
    }
    blocks
        .into_iter()
        .filter_map(|block| {
            let block = block.trim_matches('\n');
            let mut lines = block.lines().skip_while(|l| !l.contains("-->"));
            let timing = lines.next()?;
            if lines.all(|l| l.trim().is_empty()) {
                return None;
            }
            if matches!(block.split_whitespace().next(), Some("WEBVTT" | "NOTE" | "STYLE" | "REGION")) {
                return None;
            }
            let (start, rest) = timing.split_once("-->")?;
            let (start, end) = (timestamp(start.trim())?, timestamp(rest.split_whitespace().next()?)?);
            Some(Cue { start, end, block: block.to_string() })
        })
        .collect()
}

/// What one own track has so far: the cues its runs wrote, and the stretches of the film those runs have read past.
#[derive(Default)]
struct TrackCues {
    cues: BTreeSet<Cue>,
    covered: Vec<(f64, f64)>,
}

/// The release's own tracks as its runs have written them, for the session's life. A seek starts another run, which
/// writes cues from where it starts; the cues of the runs before it stay, as does the record of what they covered.
#[derive(Default)]
pub struct OwnCues {
    tracks: HashMap<usize, TrackCues>,
    /// How much of each run's file was read last time, so an unchanged one is not parsed again.
    read: HashMap<(u32, usize), u64>,
}

impl OwnCues {
    /// Take what run `job` has written to `path` for track `track` and note that it has read the film from `from` to
    /// `upto`. Cues past `upto` are kept as well: the demuxer is a little ahead of the video it has finished.
    pub fn take(&mut self, job: u32, track: usize, path: &std::path::Path, from: f64, upto: f64) {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let entry = self.tracks.entry(track).or_default();
        if len > 0 && len <= MAX_OWN && self.read.get(&(job, track)) != Some(&len) {
            if let Ok(text) = std::fs::read_to_string(path) {
                entry.cues.extend(parse_cues(&text));
                self.read.insert((job, track), len);
            }
        }
        if upto > from {
            entry.covered.push((from, upto));
            entry.covered.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut merged: Vec<(f64, f64)> = Vec::with_capacity(entry.covered.len());
            for &(a, b) in &entry.covered {
                match merged.last_mut() {
                    Some(last) if a <= last.1 + SNAP => last.1 = last.1.max(b),
                    _ => merged.push((a, b)),
                }
            }
            entry.covered = merged;
        }
    }

    /// Have the runs read all of `from`..`to` for `track`?
    pub fn covers(&self, track: usize, from: f64, to: f64) -> bool {
        self.tracks
            .get(&track)
            .is_some_and(|t| t.covered.iter().any(|(a, b)| *a <= from + SNAP && *b >= to - SNAP))
    }

    /// The WebVTT segment for `from`..`to`: every cue showing at any moment of it, on the media timeline.
    pub fn window(&self, track: usize, from: f64, to: f64) -> String {
        let (from, to) = ((from * 1000.0) as u64, (to * 1000.0) as u64);
        let mut out = String::from(EMPTY);
        for cue in self.tracks.get(&track).into_iter().flat_map(|t| &t.cues) {
            if cue.start < to && cue.end.max(cue.start + 1) > from {
                out.push_str(&cue.block);
                out.push_str("\n\n");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_size_plus_both_ends() {
        let mut head = vec![0u8; HASH_CHUNK as usize];
        head[..8].copy_from_slice(&1u64.to_le_bytes());
        let tail = vec![0xFFu8; HASH_CHUNK as usize];
        // The tail's 8192 words of u64::MAX wrap to -8192.
        let tail_sum = chunk_sum(&tail);
        assert_eq!(tail_sum, 0u64.wrapping_sub(8192));
        assert_eq!(
            movie_hash(1 << 30, chunk_sum(&head), tail_sum),
            format!("{:016x}", (1u64 << 30) + 1 - 8192)
        );
    }

    #[test]
    fn the_list_url_carries_the_hints_as_one_segment() {
        let u = list_url("http://subs.lan:8093/c2Vj", "tt1:2:3", Some("00ff"), Some(42), "A/B&C=D Film.mkv")
            .unwrap();
        assert_eq!(
            u,
            "http://subs.lan:8093/c2Vj/subtitles/series/tt1:2:3/videoHash=00ff&videoSize=42&filename=A%20B%20C%20D%20Film.mkv.json"
        );
        let u = list_url("http://subs.lan:8093/c2Vj", "tt1", None, None, "f.mkv").unwrap();
        assert!(u.ends_with("/subtitles/movie/tt1/filename=f.mkv.json"), "{u}");
    }

    #[test]
    fn a_subtitle_url_is_fetched_as_vtt_and_only_from_an_allowed_origin() {
        assert_eq!(vtt_url("http://s/c/subtitle/9.srt?lang=fin"), "http://s/c/subtitle/9.vtt?lang=fin");
        assert_eq!(vtt_url("http://s/c/subtitle/9.srt"), "http://s/c/subtitle/9.vtt");
        let allowed = ["http://192.168.86.193:8093".to_string()];
        assert!(on_origin("http://192.168.86.193:8093/c/subtitle/9.vtt?x=1", &allowed));
        assert!(!on_origin("http://169.254.169.254/latest", &allowed));
        assert!(!on_origin("http://u@192.168.86.193:8093/c", &allowed));
    }

    /// The last-resort retry's query has to land correctly whether den-subtitles' entry already
    /// carries one (`?ref=`/`&lang=`, from a hash-matched or legacy-encoded sub) or not — and
    /// `vtt_url`'s own query-preserving split must still find `sparse=1` afterwards, since
    /// `fetch_subtitle` runs that AFTER this.
    #[test]
    fn with_sparse_appends_to_whichever_query_shape_the_entry_already_has() {
        assert_eq!(with_sparse("http://s/c/subtitle/9.srt"), "http://s/c/subtitle/9.srt?sparse=1");
        assert_eq!(
            with_sparse("http://s/c/subtitle/9.srt?ref=5&lang=en"),
            "http://s/c/subtitle/9.srt?ref=5&lang=en&sparse=1"
        );
        // `vtt_url` must still carry it through to the `.vtt` form fetch_subtitle actually requests.
        assert_eq!(
            vtt_url(&with_sparse("http://s/c/subtitle/9.srt?lang=fin")),
            "http://s/c/subtitle/9.vtt?lang=fin&sparse=1"
        );
    }

    fn track(codec: &str, language: Option<&str>, text: bool, forced: bool) -> SubtitleTrack {
        SubtitleTrack {
            codec: codec.into(),
            language: language.map(String::from),
            text,
            forced,
            default: true,
            hearing_impaired: false,
            name: None,
        }
    }

    #[test]
    fn a_language_the_release_carries_is_offered_and_one_it_lacks_is_not() {
        let tracks = [
            track("S_HDMV/PGS", Some("eng"), false, false),
            track("S_TEXT/UTF8", Some("swe"), true, true),
            track("S_TEXT/ASS", Some("fin"), true, false),
            track("S_TEXT/UTF8", Some("eng"), true, false),
            track("S_TEXT/UTF8", Some("eng"), true, false),
            track("S_TEXT/UTF8", None, true, false),
        ];
        let named = ["fi".to_string(), "de".to_string()];
        // No den-subtitles: only what the release has, the languages asked for first, then its others.
        // Bitmap makes no rendition, and a language with two full tracks takes the first. Swedish's only
        // own track is forced, and nothing here names the audio as a Swedish dub, so it is no rendition at
        // all — offered plain it would caption only the foreign-language parts, which over an otherwise-
        // single-language film reads as "barely any subtitles". The untagged track at 5 fills the one slot
        // left over, filed under `UNDETERMINED`.
        assert_eq!(
            plan(&named, false, &tracks, None, &[]),
            [
                Rendition { lang: "fi".into(), den: false, own: Some(2) },
                Rendition { lang: "en".into(), den: false, own: Some(3) },
                Rendition { lang: "und".into(), den: false, own: Some(5) },
            ]
        );
        // With it AND a den-subtitles list that actually names "de": every language asked for is a
        // rendition, and English, guaranteed a place right after the viewer's own first choice, outranks
        // "de", named second, which the release lacks. The untagged track still rides along after, same
        // as before.
        assert_eq!(
            plan(&named, true, &tracks, None, &[den_entry("de")]),
            [
                Rendition { lang: "fi".into(), den: true, own: Some(2) },
                Rendition { lang: "en".into(), den: true, own: Some(3) },
                Rendition { lang: "de".into(), den: true, own: None },
                Rendition { lang: "und".into(), den: false, own: Some(5) },
            ]
        );
        assert!(plan(&[], false, &[track("S_HDMV/PGS", Some("eng"), false, false)], None, &[]).is_empty());
    }

    /// den-remux#<TODO>: with den-subtitles connected, `plan()` used to promise a `den: true` rendition for
    /// every preferred language regardless of whether den-subtitles had anything to serve — "den-subtitles
    /// may have it" — so the web menu offered a language nothing could play (Fauda S1E3: Swedish/English
    /// preferences, a Polish-only release, den-subtitles listing Hebrew and Polish but no English or
    /// Swedish). Neither phantom language may appear; Polish, backed by the release's own track, still does.
    #[test]
    fn a_den_only_rendition_needs_a_candidate_in_den_subtitles_own_list() {
        let tracks = [track("S_TEXT/UTF8", Some("pl"), true, false)];
        let requested = ["sv".to_string(), "en".to_string()];
        let den_list = [den_entry("he"), den_entry("pl")];
        let renditions = plan(&requested, true, &tracks, None, &den_list);
        let langs: Vec<&str> = renditions.iter().map(|r| r.lang.as_str()).collect();
        assert!(!langs.contains(&"sv"), "{langs:?}");
        assert!(!langs.contains(&"en"), "{langs:?}");
        assert_eq!(renditions, [Rendition { lang: "pl".into(), den: false, own: Some(0) }], "{langs:?}");

        // An empty list (the fetch failed, timed out, or genuinely found nothing) is read the same way:
        // no den-only rendition is promised on a guess.
        let renditions = plan(&requested, true, &tracks, None, &[]);
        let langs: Vec<&str> = renditions.iter().map(|r| r.lang.as_str()).collect();
        assert!(!langs.contains(&"sv"), "{langs:?}");
        assert!(!langs.contains(&"en"), "{langs:?}");
    }

    #[test]
    fn the_releases_languages_stop_at_the_most_a_session_names() {
        let tracks: Vec<_> = ["eng", "fin", "swe", "deu", "fra", "spa", "ita", "por", "nor", "dan"]
            .iter()
            .map(|l| track("S_TEXT/UTF8", Some(l), true, false))
            .collect();
        assert_eq!(plan(&[], false, &tracks, None, &[]).len(), MAX_LANGUAGES);
    }

    /// The real bug this fix was filed against: a release with 30 embedded languages, in the muxer's own
    /// near-alphabetical order, where the cap offered "he, en, ar, cs, da, de, el, es" and left out "sv" and
    /// "fi" — the household's own languages — because they sorted past the eighth slot. The cap must fill from
    /// the viewer's own languages and English first, never from file order alone.
    #[test]
    fn the_cap_never_excludes_english_or_the_viewers_own_languages() {
        let codes = [
            "ar", "cs", "da", "de", "el", "es", "he", "hi", "hr", "hu", "id", "is", "it", "en", "ja", "ko",
            "lt", "lv", "ms", "nl", "no", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "th", "fi",
        ];
        let tracks: Vec<_> = codes.iter().map(|l| track("S_TEXT/UTF8", Some(l), true, false)).collect();

        // The viewer's own languages lead — English guaranteed right behind the first of them even though it
        // wasn't named — then the rest fills from the release in file order.
        let preferred = ["sv".to_string(), "fi".to_string()];
        let out = plan(&preferred, false, &tracks, None, &[]);
        let langs: Vec<&str> = out.iter().map(|r| r.lang.as_str()).collect();
        assert_eq!(langs, ["sv", "en", "fi", "ar", "cs", "da", "de", "el"], "{langs:?}");
        assert_eq!(out.len(), MAX_LANGUAGES);

        // A preference that already leads with English keeps it first rather than inserting a duplicate.
        let english_first = ["en".to_string(), "sv".to_string()];
        let out = plan(&english_first, false, &tracks, None, &[]);
        assert_eq!((out[0].lang.as_str(), out[1].lang.as_str()), ("en", "sv"));

        // No named preference at all: English still leads when the release carries it, ahead of the session's
        // own audio language, which in turn outranks the rest of the release's languages.
        let out = plan(&[], false, &tracks, Some("fi"), &[]);
        let langs: Vec<&str> = out.iter().map(|r| r.lang.as_str()).collect();
        assert_eq!(&langs[..2], ["en", "fi"], "{langs:?}");
    }

    /// `describe_subtitles`'s account of each track must agree with what `plan()` actually decided —
    /// the whole point of logging it is that a report can be answered from the log, not from
    /// re-deriving plan()'s rules by eye and hoping they still match what shipped.
    #[test]
    fn describe_subtitles_agrees_with_plan_s_own_decisions() {
        let tracks = [
            track("S_TEXT/UTF8", Some("heb"), true, true), // 0: forced Hebrew
            track("S_TEXT/UTF8", Some("heb"), true, false), // 1: full Hebrew
            track("S_HDMV/PGS", Some("eng"), false, false), // 2: bitmap English
            track("S_TEXT/UTF8", None, true, false),       // 3: untagged
            SubtitleTrack {
                name: Some("English [SDH]".into()),
                hearing_impaired: true,
                ..track("S_TEXT/UTF8", Some("eng"), true, false) // 4: English SDH, listed before...
            },
            track("S_TEXT/UTF8", Some("eng"), true, false), // 5: ...the full English track
        ];
        let requested = ["en".to_string()];
        // No audio language known: neither Hebrew nor English is a dub-in-its-own-language context, so the
        // plain full-over-SDH-over-forced order applies throughout.
        let renditions = plan(&requested, false, &tracks, None, &[]);
        // The full track at 5 now correctly beats the SDH track at 4 (`own_tier`/`best_own`), and the full
        // Hebrew track at 1 beats the forced one at 0 — track order no longer decides it. Hebrew rides
        // along too: `plan()` offers every own-track language, not only the requested one, when nothing
        // asked for den-subtitles. The untagged track at 3 rides along too, filed under `UNDETERMINED`
        // since there is room for it.
        assert_eq!(
            renditions,
            [
                Rendition { lang: "en".into(), den: false, own: Some(5) },
                Rendition { lang: "he".into(), den: false, own: Some(1) },
                Rendition { lang: "und".into(), den: false, own: Some(3) },
            ]
        );

        let desc = describe_subtitles(&tracks, &renditions, None);
        assert!(desc.contains("0 lang=he codec=S_TEXT/UTF8 [forced,default]"), "{desc}");
        assert!(desc.contains("dropped (track 1 ranks higher for he)"), "{desc}");
        assert!(desc.contains("1 lang=he") && desc.contains("-> offered as he"), "{desc}");
        assert!(desc.contains("2 lang=en") && desc.contains("bitmap, can't become WebVTT"), "{desc}");
        assert!(desc.contains("3 lang=und") && desc.contains("-> offered as und"), "{desc}");
        assert!(
            desc.contains("4 lang=en codec=S_TEXT/UTF8 [default,sdh] title=\"English [SDH]\"")
                && desc.contains("dropped (track 5 ranks higher for en)"),
            "the SDH track must lose to the full one, not shadow it by listing order: {desc}"
        );
        assert!(desc.contains("5 lang=en") && desc.contains("-> offered as en"), "{desc}");
    }

    /// den-remux#<TODO>: a release (Fauda S1E3, 17 GB WEB-DL) with no embedded English track at all, but an
    /// untagged pair at 5/6 — forced-and-default, then full — almost certainly English (the dub this release's
    /// audio is in is Hebrew, and a forced-only track over an otherwise single-language film is the shape of
    /// a "translate the foreign bits" track, not a whole show's worth of dialogue). Before this fix neither
    /// untagged track was ever offered; now the full one is, at its own file position, same as a tagged
    /// language would be — and it is still passed over by an actual tagged language that out-ranks it there.
    #[test]
    fn an_untagged_full_track_fills_in_where_nothing_tagged_fits() {
        let tagged = ["ar", "bg", "da", "de", "el"];
        let mut tracks: Vec<_> = tagged.iter().map(|l| track("S_TEXT/UTF8", Some(l), true, false)).collect();
        tracks.push(track("S_TEXT/UTF8", None, true, true)); // 5: forced, untagged
        tracks.push(track("S_TEXT/UTF8", None, true, false)); // 6: full, untagged — likely English
        for l in [
            "es", "fi", "fr", "he", "it", "ja", "ko", "no", "nl", "pl", "pt", "ro", "ru", "sv", "th", "tr",
            "vi", "zh",
        ] {
            tracks.push(track("S_TEXT/UTF8", Some(l), true, false));
        }
        let requested = ["en".to_string()];
        // The viewer asked for English; the release tags none, so the preferred pass finds nothing there — but
        // the untagged track at 6 fills a slot in the backfill, at the position its file order earns it:
        // ahead of "es" and everything after, same as a tagged language at 6 would have outranked them.
        let renditions = plan(&requested, false, &tracks, Some("he"), &[]);
        let langs: Vec<(&str, Option<usize>)> = renditions.iter().map(|r| (r.lang.as_str(), r.own)).collect();
        assert_eq!(
            langs,
            [
                ("he", Some(10)),
                ("ar", Some(0)),
                ("bg", Some(1)),
                ("da", Some(2)),
                ("de", Some(3)),
                ("el", Some(4)),
                ("und", Some(6)),
                ("es", Some(7)),
            ],
            "{langs:?}"
        );
        assert_eq!(renditions.len(), MAX_LANGUAGES);

        let desc = describe_subtitles(&tracks, &renditions, Some("he"));
        assert!(
            desc.contains("5 lang=und") && desc.contains("dropped (forced, no usable language tag)"),
            "{desc}"
        );
        assert!(desc.contains("6 lang=und") && desc.contains("-> offered as und"), "{desc}");
    }

    #[test]
    fn describe_subtitles_says_so_when_there_are_none() {
        assert_eq!(describe_subtitles(&[], &[], None), "no subtitle tracks");
    }

    /// Mirrors the Apple TV's `subtitleRank`: within one language's own tracks, a full track wins over an
    /// SDH one whatever order they are listed in the file — not "the first non-forced track wins", which let
    /// an SDH track hide a full one listed after it.
    #[test]
    fn own_tracks_rank_full_over_sdh_whatever_order_they_are_listed() {
        let tracks = [
            SubtitleTrack { hearing_impaired: true, ..track("S_TEXT/UTF8", Some("eng"), true, false) }, // 0: SDH, listed first
            track("S_TEXT/UTF8", Some("eng"), true, false), // 1: full, listed second
        ];
        let requested = ["en".to_string()];
        assert_eq!(
            plan(&requested, false, &tracks, None, &[]),
            [Rendition { lang: "en".into(), den: false, own: Some(1) }]
        );
    }

    /// Rule: when the session's audio is a dub in the subtitle's own language, a forced/foreign-parts own
    /// track is the right default instead of a full one — it captions only what the dub leaves
    /// untranslated, where a full track would duplicate the spoken dialogue.
    #[test]
    fn a_dub_in_the_subtitle_s_own_language_prefers_the_forced_track() {
        let tracks = [
            track("S_TEXT/UTF8", Some("eng"), true, false), // 0: full English
            track("S_TEXT/UTF8", Some("eng"), true, true),  // 1: forced English
        ];
        let requested = ["en".to_string()];
        // Without a dub context, full still wins, same as any other language.
        assert_eq!(
            plan(&requested, false, &tracks, None, &[]),
            [Rendition { lang: "en".into(), den: false, own: Some(0) }]
        );
        // The session's own audio is an English dub: forced is now the better own candidate.
        assert_eq!(
            plan(&requested, false, &tracks, Some("en"), &[]),
            [Rendition { lang: "en".into(), den: false, own: Some(1) }]
        );
    }

    /// A forced track is never a rendition's own candidate outside a dub context, even when it is the
    /// only track its language has at all — the regression a Docker e2e run caught: a release's forced
    /// Swedish track, with English audio, must stay invisible rather than surface as near-empty "Swedish"
    /// subtitles (captioning only the foreign-language parts of an otherwise all-English film). The same
    /// forced track, in a dub context for its own language, is the right default.
    #[test]
    fn a_sole_forced_track_is_offered_only_in_its_own_dub_context() {
        let swedish = [track("S_TEXT/UTF8", Some("swe"), true, true)];
        let requested = ["sv".to_string()];
        // English audio: not a Swedish dub. The forced Swedish track is no candidate at all.
        assert!(plan(&requested, false, &swedish, Some("en"), &[]).is_empty());
        assert_eq!(best_own(&swedish, "sv", Some("en")), None);
        // A Swedish dub: now it's the only, and therefore the default, candidate.
        assert_eq!(
            plan(&requested, false, &swedish, Some("sv"), &[]),
            [Rendition { lang: "sv".into(), den: false, own: Some(0) }]
        );
        assert_eq!(best_own(&swedish, "sv", Some("sv")), Some(0));
    }

    /// A plain, unflagged den-subtitles entry — not marked foreign-parts-only or dubbed.
    fn den_entry(lang: &str) -> Entry {
        Entry {
            url: "http://s/subtitle/1.srt".into(),
            lang: lang.into(),
            foreign_parts_only: false,
            looks_dubbed: false,
        }
    }

    /// `Session::subtitle_playlist`'s own-vs-downloaded call, as the five scenarios this fix targets (the Fauda
    /// report: a guest's English subtitles, downloaded for a dubbed release, were missing whole scenes).
    mod own_wins_tests {
        use super::*;

        /// An English-audio release with its own full English track plus a plain den-subtitles English file:
        /// the own track is the default — it costs no download and is already in sync, so it wins the tie.
        #[test]
        fn an_own_full_track_beats_a_plain_download() {
            let own = track("S_TEXT/UTF8", Some("eng"), true, false);
            assert!(own_wins(&own, Some(&den_entry("en")), true));
        }

        /// Hebrew audio with a full own English track and a den-subtitles file flagged foreign-parts-only: full
        /// is the default. The audio is not a dub in English, so the full track is what's needed — the
        /// foreign-parts file would miss whatever the Hebrew dialogue says.
        #[test]
        fn a_full_own_track_beats_a_foreign_parts_download_when_the_audio_is_not_its_dub() {
            let own = track("S_TEXT/UTF8", Some("eng"), true, false);
            let partial = Entry { foreign_parts_only: true, ..den_entry("en") };
            assert!(own_wins(&own, Some(&partial), false));
        }

        /// English dub audio with an own forced English track, against a plain (unflagged) den-subtitles
        /// download: forced is the default. The dub already speaks the dialogue; a full track, own or
        /// downloaded, would duplicate it, so the forced track — captioning only what the dub leaves
        /// untranslated — wins even over a download that out-tiers it in every other context.
        #[test]
        fn an_own_forced_track_beats_a_full_download_when_the_audio_is_its_dub() {
            let own = track("S_TEXT/UTF8", Some("eng"), true, true);
            assert!(own_wins(&own, Some(&den_entry("en")), true));
        }

        /// A release with no own English track at all, and den-subtitles' own list naming English as a
        /// candidate: den-subtitles is used — there is no own candidate for `own_wins` to call, and
        /// `plan()` never puts one in a `Rendition` to call it with.
        #[test]
        fn no_own_track_means_plan_names_none() {
            let tracks = [track("S_HDMV/PGS", Some("eng"), false, false)];
            let requested = ["en".to_string()];
            assert_eq!(
                plan(&requested, true, &tracks, None, &[den_entry("en")]),
                [Rendition { lang: "en".into(), den: true, own: None }]
            );
        }

        /// A den-subtitles file flagged foreign-parts-only or looks-dubbed never beats a full own track when the
        /// audio isn't a dub in that language, however the own track's tier would otherwise compare — the whole
        /// point of carrying the flags through from den-subtitles#15.
        #[test]
        fn a_flagged_download_never_beats_a_full_own_track_outside_a_dub_context() {
            let own = track("S_TEXT/UTF8", Some("eng"), true, false);
            for partial in [
                Entry { foreign_parts_only: true, ..den_entry("en") },
                Entry { looks_dubbed: true, ..den_entry("en") },
            ] {
                assert!(own_wins(&own, Some(&partial), false));
            }
        }
    }

    #[test]
    fn ffmpegs_webvtt_is_read_cue_by_cue_and_a_half_written_cue_waits() {
        // As the muxer writes it: the blank line before each cue, minutes and seconds until an hour.
        let file = "WEBVTT\n\n00:01.000 --> 00:03.000\nHello\n\n\
                    1:02:03.500 --> 1:02:04.000 line:90%\n<i>Later</i>\nlines\n";
        let cues = parse_cues(file);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!((cues[0].start, cues[0].end), (1000, 3000));
        assert_eq!(cues[0].block, "00:01.000 --> 00:03.000\nHello");
        assert_eq!(cues[1].start, (62 * 60 + 3) * 1000 + 500, "an hour, unpadded");
        assert!(cues[1].block.contains("<i>Later</i>\nlines"), "{}", cues[1].block);
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:10.000\n")).len(), 2, "no text yet");
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:1")).len(), 2, "cut mid-line");
        assert_eq!(parse_cues(&format!("{file}\n00:09.000 --> 00:10.000\nHalf\n")).len(), 3);
        assert!(parse_cues("WEBVTT\n\nNOTE a note --> here\n\n").is_empty());
        assert!(parse_cues("").is_empty());
    }

    #[test]
    fn a_window_holds_the_cues_showing_in_it_and_only_once_covered() {
        let dir = std::env::temp_dir().join(format!("den-remux-own-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t0.vtt");
        std::fs::write(
            &file,
            "WEBVTT\n\n00:01.000 --> 00:03.000\nfirst\n\n00:07.000 --> 00:09.000\nacross\n\n\
             00:20.000 --> 00:21.000\nlate\n",
        )
        .unwrap();
        let mut own = OwnCues::default();
        assert!(!own.covers(0, 0.0, 8.0), "nothing read yet");
        own.take(1, 0, &file, 0.0, 12.0);
        assert!(own.covers(0, 0.0, 8.0) && own.covers(0, 8.0, 12.0));
        assert!(!own.covers(0, 8.0, 13.0) && !own.covers(1, 0.0, 8.0));
        let first = own.window(0, 0.0, 8.0);
        assert!(first.starts_with(EMPTY), "the timestamp map, as den-subtitles' documents have: {first}");
        assert!(first.contains("first") && first.contains("across") && !first.contains("late"), "{first}");
        let second = own.window(0, 8.0, 13.0);
        assert!(
            second.contains("across") && !second.contains("first"),
            "a cue over the cut is in both: {second}"
        );
        // A second run that reads the same cues again, and further, adds no duplicate and joins the coverage.
        own.take(2, 0, &file, 10.0, 30.0);
        assert!(own.covers(0, 0.0, 30.0));
        assert_eq!(own.window(0, 0.0, 30.0).matches("across").count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_vtt_gets_its_timestamp_map_and_anything_else_is_refused() {
        let v = for_hls("\u{feff}WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nHi\n".as_bytes()).unwrap();
        assert!(v.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n00:00:01.000"), "{v}");
        assert!(for_hls(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n").is_none(), "SRT is not WebVTT");
        assert!(EMPTY.starts_with("WEBVTT\n"));
    }
}
