//! LRC lyrics: parsing (incl. enhanced word timestamps), timeline lookup,
//! local-file discovery and online fetch (Lrclib).
//!
//! The parsed data powers the `lyric` widget: current/next/previous lines and
//! per-line karaoke progress.

/// One lyric line with an optional word-level timeline (enhanced LRC `<mm:ss.xx>`).
/// `words` entries are `(start, end, text)` — start/end in seconds relative to the
/// line's start, text is the word's own characters (used for per-word karaoke colouring).
#[derive(Debug, Clone)]
pub struct LyricLine {
    /// Start time in seconds (after applying the global offset).
    pub time: f32,
    pub text: String,
    pub words: Vec<(f32, f32, String)>,
}

/// Parsed lyric document.
#[derive(Debug, Clone, Default)]
pub struct LyricData {
    /// Lines sorted by start time.
    pub lines: Vec<LyricLine>,
    /// Global `[offset:ms]` adjustment (negative = lyrics earlier).
    pub offset: f32,
}

/// Current playback state resolved against a `LyricData`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineState {
    /// Index into `LyricData::lines`.
    pub index: usize,
    /// Karaoke progress within the current line, 0..1.
    pub progress: f32,
    /// Index of the word currently being sung (0 when no word timeline).
    pub word: usize,
}

/// Parse an LRC document. Handles `[mm:ss.xx]` time tags (a line may carry several,
/// duplicating the text for each), metadata tags (`[ti:]`, `[ar:]`, `[al:]`, `[offset:]`,
/// `[by:]`) and enhanced word timestamps `<mm:ss.xx>`.
pub fn parse_lrc(text: &str) -> LyricData {
    let mut offset = 0.0f32;
    let mut raw: Vec<(f32, String, Vec<(f32, f32, String)>)> = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Collect leading [tag] segments.
        let mut times: Vec<f32> = Vec::new();
        let mut rest = line;
        loop {
            let trimmed = rest.trim_start();
            let Some(body) = trimmed.strip_prefix('[') else {
                break;
            };
            let Some((tag, tail)) = body.split_once(']') else {
                break;
            };
            match parse_time_tag(tag) {
                Some(t) => times.push(t),
                None => {
                    let lower = tag.to_ascii_lowercase();
                    if let Some(ms) = lower.strip_prefix("offset:") {
                        if let Ok(v) = ms.trim().parse::<f32>() {
                            offset = v / 1000.0;
                        }
                    }
                    // [ti:]/[ar:]/[al:]/[by:] are metadata — ignored for display.
                }
            }
            rest = tail;
        }
        let content = rest.trim();
        if times.is_empty() {
            continue;
        }
        // Drop noisy credit/attribution lines (QQ Music LRCs carry them as timed
        // lines: "词：周杰伦", "曲：…", "编曲：…", etc.) — they are metadata, not
        // singable lyrics.
        if is_credit_line(content) {
            continue;
        }
        // Enhanced LRC: inline <mm:ss.xx> word timestamps.
        let (plain, words) = parse_enhanced(content);
        // Skip empty timed lines (instrumental pauses, "[00:15.72] " etc.) — they
        // carry nothing to display and would make the prev/next rail drop lines.
        if plain.trim().is_empty() && words.is_empty() {
            continue;
        }
        for &t in &times {
            raw.push((t, plain.clone(), words.clone()));
        }
    }

/// True when a lyric line is a composer/credits attribution (not singable text).
fn is_credit_line(text: &str) -> bool {
    const CREDIT_PREFIXES: [&str; 24] = [
        "词：", "曲：", "词:", "曲:", "编曲", "制作人", "合声", "和声", "吉他", "贝斯",
        "鼓", "钢琴", "键盘", "弦乐", "管乐", "监制", "录音", "混音", "母带",
        "企划", "统筹", "发行", "出品", "配唱",
    ];
    let t = text.trim();
    // QQ credit lines are short and start with a credit keyword.
    t.chars().count() <= 40
        && CREDIT_PREFIXES
            .iter()
            .any(|p| t.starts_with(p) || t.contains(&format!("：{}", p.trim_end_matches('：'))))
}

    raw.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let lines = raw
        .into_iter()
        .map(|(t, text, words)| LyricLine {
            time: t - offset,
            text,
            words,
        })
        .collect();
    LyricData { lines, offset }
}

/// Parse a `[mm:ss(.xx)]` tag; None for metadata tags.
fn parse_time_tag(tag: &str) -> Option<f32> {
    let (mm, ss) = tag.split_once(':')?;
    let mm: f32 = mm.trim().parse().ok()?;
    let ss: f32 = ss.trim().parse().ok()?;
    Some(mm * 60.0 + ss)
}

/// Strip `<mm:ss.xx>` word markers, returning plain text and word (start, end, text)
/// triples relative to the line start.
fn parse_enhanced(text: &str) -> (String, Vec<(f32, f32, String)>) {
    let mut segs: Vec<(f32, String)> = Vec::new();
    let mut first: Option<f32> = None;
    let mut rest = text;
    while let Some(lt) = rest.find('<') {
        let after = &rest[lt + 1..];
        let Some(gt) = after.find('>') else { break };
        let Some(t) = parse_time_tag(&after[..gt]) else { break };
        let base = *first.get_or_insert(t);
        if segs.is_empty() {
            segs.push((0.0, String::new()));
        }
        if let Some(last) = segs.last_mut() {
            last.1.push_str(&rest[..lt]);
        }
        segs.push((t - base, String::new()));
        rest = &after[gt + 1..];
    }
    if segs.is_empty() {
        // No word timestamps at all: plain line, no per-word timeline.
        return (text.to_string(), Vec::new());
    }
    if let Some(last) = segs.last_mut() {
        last.1.push_str(rest);
    }
    let mut plain = String::new();
    let mut words: Vec<(f32, f32, String)> = Vec::new();
    for i in 0..segs.len() {
        plain.push_str(&segs[i].1);
        let start = segs[i].0;
        let end = segs
            .get(i + 1)
            .map(|s| s.0)
            .unwrap_or(start + 1.0)
            .max(start + 0.01);
        if !segs[i].1.is_empty() {
            words.push((start, end, segs[i].1.clone()));
        }
    }
    (plain, words)
}

/// Resolve playback time `t` (seconds) against the lyric data.
/// Returns the active line and karaoke progress, or None before the first line.
pub fn line_state(data: &LyricData, t: f32) -> Option<LineState> {
    if data.lines.is_empty() || t < data.lines[0].time {
        return None;
    }
    let mut idx = 0usize;
    for (i, l) in data.lines.iter().enumerate() {
        if l.time <= t {
            idx = i;
        } else {
            break;
        }
    }
    let line = &data.lines[idx];
    let start = line.time;
    let end = data
        .lines
        .get(idx + 1)
        .map(|n| n.time)
        .unwrap_or(start + 5.0)
        .max(start + 0.05);
    let progress = ((t - start) / (end - start)).clamp(0.0, 1.0);
    let word = if line.words.is_empty() {
        0
    } else {
        let rel = t - start;
        line.words
            .iter()
            .position(|(ws, we, _)| rel >= *ws && rel < *we)
            .unwrap_or(line.words.len().saturating_sub(1))
    };
    Some(LineState { index: idx, progress, word })
}

/// Sanitise a track key into a filesystem-safe name.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect()
}

/// Try to load a local `.lrc` for `(title, artist)` from `dir`.
/// Candidate names: `<title>.lrc`, `<artist> - <title>.lrc`, `<artist>-<title>.lrc`.
/// Both the raw title/artist and the filesystem-sanitized forms are tried, so
/// files named naturally (with spaces) and by the cache convention both match.
pub fn load_local(dir: &str, title: &str, artist: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() {
        return None;
    }
    let artist = artist.trim();
    let mut names = vec![format!("{title}.lrc")];
    for base in [title.to_string(), sanitize(title)] {
        for a in [artist.to_string(), sanitize(artist)] {
            if !a.is_empty() {
                names.push(format!("{a} - {base}.lrc"));
                names.push(format!("{a}-{base}.lrc"));
            }
        }
        names.push(format!("{base}.lrc"));
    }
    // De-duplicate while preserving order.
    let mut seen = std::collections::HashSet::new();
    for n in names {
        if !seen.insert(n.clone()) {
            continue;
        }
        let p = format!("{dir}/{n}");
        if let Ok(s) = std::fs::read_to_string(&p) {
            return Some(s);
        }
    }
    None
}

/// Fetch lyrics from Tencent QQ Music's public web API (no auth). Searches by
/// `title + artist`, validates the candidate against `duration_hint` (seconds) when
/// known — SPlayer-style guard against matching the wrong song — then downloads the
/// LRC. Blocking — call from a background thread.
pub fn fetch_qq(
    title: &str,
    artist: &str,
    duration_hint: Option<f32>,
    timeout: std::time::Duration,
) -> Option<String> {
    let keyword = format!("{} {}", title.trim(), artist.trim());
    if keyword.trim().is_empty() {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();
    let referer = "https://y.qq.com/";
    let search_url = format!(
        "https://c.y.qq.com/soso/fcgi-bin/client_search_cp?w={}&format=json&n=5",
        urlencode(&keyword)
    );
    let resp = agent.get(&search_url).set("Referer", referer).call().ok()?;
    let body = resp.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let list = v.pointer("/data/song/list")?.as_array()?;
    if list.is_empty() {
        return None;
    }
    // Pick the best candidate: duration match (if a hint is available) beats order,
    // so a wrong-length re-record from another album never wins over the right one.
    let title_l = title.trim().to_lowercase();
    let mut best: Option<(f32, usize)> = None; // (score, index); lower is better
    let mut any_dur_ok = false;
    for (i, s) in list.iter().enumerate() {
        let name = s.get("songname").and_then(|x| x.as_str()).unwrap_or("").to_lowercase();
        let interval = s.get("interval").and_then(|x| x.as_u64()).unwrap_or(0) as f32;
        let name_ok = name.contains(&title_l) || title_l.contains(&name.trim_end());
        let dur_ok = duration_hint
            .map(|hint| (interval - hint).abs() < 8.0)
            .unwrap_or(true);
        if dur_ok {
            any_dur_ok = true;
        }
        let score = if dur_ok && name_ok {
            0.0
        } else if dur_ok {
            1.0
        } else if name_ok {
            2.0
        } else {
            3.0
        };
        if best.map(|(bs, _)| score < bs).unwrap_or(true) {
            best = Some((score, i));
        }
    }
    // When the real duration is known, only accept a duration-plausible match — a
    // name-only hit can be a wrong version/cover, which lrclib would handle better.
    if duration_hint.is_some() && !any_dur_ok {
        return None;
    }
    let idx = best?.1;
    let song = &list[idx];
    let mid = song
        .get("songmid")
        .or_else(|| song.get("media_mid"))
        .and_then(|x| x.as_str())?;
    if mid.is_empty() {
        return None;
    }
    let lyric_url = format!(
        "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={mid}&format=json&nobase64=1"
    );
    let resp = agent.get(&lyric_url).set("Referer", referer).call().ok()?;
    let body = resp.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let lrc = v.get("lyric").and_then(|x| x.as_str())?;
    if lrc.trim().is_empty() {
        return None;
    }
    Some(lrc.to_string())
}

/// Fetch synced lyrics for `(title, artist)` from Lrclib (https://lrclib.net),
/// a free open lyrics API returning LRC files. Blocking — call from a background thread.
pub fn fetch_online(title: &str, artist: &str, timeout: std::time::Duration) -> Option<String> {
    let query = format!("{} {}", title.trim(), artist.trim());
    if query.trim().is_empty() {
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .build();
    let url = format!(
        "https://lrclib.net/api/search?q={}",
        urlencode(query.trim())
    );
    let resp = agent.get(&url).call().ok()?;
    let body = resp.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let arr = v.as_array()?;
    // Prefer the first result with a synced (timestamped) lyric.
    for entry in arr.iter().take(5) {
        if let Some(lrc) = entry.get("syncedLyrics").and_then(|s| s.as_str()) {
            if !lrc.trim().is_empty() {
                return Some(lrc.to_string());
            }
        }
    }
    None
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Full fetch pipeline: local `~/.config/pulse-ring/lyrics/` first, then cache, then
/// QQ Music, then Lrclib. The winner is cached to disk. Blocking — background thread.
pub fn fetch_lyrics(
    title: &str,
    artist: &str,
    cfg_dir: &str,
    cache_dir: &str,
    duration_hint: Option<f32>,
) -> Option<String> {
    if title.trim().is_empty() {
        return None;
    }
    if let Some(s) = fetch_local_or_cache(title, artist, cfg_dir, cache_dir) {
        return Some(s);
    }
    let timeout = std::time::Duration::from_secs(3);
    let fetched = fetch_qq(title, artist, duration_hint, timeout)
        .or_else(|| fetch_online(title, artist, timeout))?;
    let _ = std::fs::create_dir_all(cache_dir);
    let _ = std::fs::write(&cache_path(title, artist, cache_dir), &fetched);
    Some(fetched)
}

/// Cache file path for a track.
pub fn cache_path(title: &str, artist: &str, cache_dir: &str) -> String {
    format!("{cache_dir}/{}.lrc", sanitize(&format!("{artist}-{title}")))
}

/// Instant lookup: local lyrics dir + disk cache only — NO network. Safe to call on
/// the main thread on every track change so previously-heard songs show lyrics
/// immediately instead of waiting for a network round-trip.
pub fn fetch_local_or_cache(
    title: &str,
    artist: &str,
    cfg_dir: &str,
    cache_dir: &str,
) -> Option<String> {
    if let Some(s) = load_local(cfg_dir, title, artist) {
        return Some(s);
    }
    let cache_path = cache_path(title, artist, cache_dir);
    if let Ok(s) = std::fs::read_to_string(&cache_path) {
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[ti:测试歌曲]
[ar:测试歌手]
[offset:0]
[00:01.00]第一句歌词
[00:03.50][00:05.00]重复行
[00:07.20]最后一句
";

    #[test]
    fn parses_basic_lrc() {
        let d = parse_lrc(SAMPLE);
        // 3 unique texts, one duplicated -> 4 lines
        assert_eq!(d.lines.len(), 4, "lines: {:?}", d.lines);
        assert_eq!(d.lines[0].time, 1.0);
        assert_eq!(d.lines[0].text, "第一句歌词");
        // duplicated line appears twice with both timestamps
        assert_eq!(d.lines[1].time, 3.5);
        assert_eq!(d.lines[1].text, "重复行");
        assert_eq!(d.lines[2].time, 5.0);
        assert_eq!(d.lines[2].text, "重复行");
        assert!(d.lines.windows(2).all(|w| w[0].time <= w[1].time), "sorted");
    }

    #[test]
    fn parses_offset() {
        let d = parse_lrc("[offset:2000]\n[00:01.00]x\n");
        // time = 1.0 - 2.0 = -1.0
        assert!((d.lines[0].time + 1.0).abs() < 1e-4);
    }

    #[test]
    fn parses_enhanced_words() {
        let d = parse_lrc("[00:10.00]<00:10.00>你<00:11.00>好<00:12.50>世界\n");
        assert_eq!(d.lines.len(), 1);
        assert_eq!(d.lines[0].text, "你好世界");
        assert_eq!(d.lines[0].words.len(), 3);
        // first word starts at 0, second at 1.0, third at 2.5 — with their own text
        let w = &d.lines[0].words;
        assert!((w[0].0).abs() < 1e-4 && (w[0].1 - 1.0).abs() < 1e-4 && w[0].2 == "你");
        assert!((w[1].0 - 1.0).abs() < 1e-4 && (w[1].1 - 2.5).abs() < 1e-4 && w[1].2 == "好");
        assert!((w[2].0 - 2.5).abs() < 1e-4 && w[2].2 == "世界");
    }

    #[test]
    fn line_state_progress_and_word() {
        let d = parse_lrc("[00:10.00]<00:10.00>A<00:12.00>B<00:14.00>C\n[00:20.00]next\n");
        let s = line_state(&d, 10.5).unwrap();
        assert_eq!(s.index, 0);
        assert_eq!(s.word, 0); // word A spans 10..12s
        let s = line_state(&d, 13.0).unwrap();
        assert_eq!(s.index, 0);
        assert_eq!(s.word, 1); // word B spans 12..14s
        // progress = (13-10)/(20-10) = 0.3
        assert!((s.progress - 0.3).abs() < 1e-3);
        let s = line_state(&d, 20.5).unwrap();
        assert_eq!(s.index, 1);
        assert_eq!(s.word, 0);
        // last line: end = start + 5s -> progress = 0.5/5 = 0.1
        assert!((s.progress - 0.1).abs() < 1e-3);
    }

    #[test]
    fn line_state_before_first() {
        let d = parse_lrc("[00:10.00]x\n");
        assert!(line_state(&d, 5.0).is_none());
        assert!(line_state(&d, 10.0).is_some());
    }

    #[test]
    fn load_local_finds_files() {
        let dir = std::env::temp_dir().join("pulse-ring-lrc-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("周杰伦 - 晴天.lrc"), "[00:01.00]test\n").unwrap();
        let s = load_local(dir.to_str().unwrap(), "晴天", "周杰伦");
        assert!(s.is_some(), "should find artist - title file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "network"]
    fn online_fetch_returns_synced_lrc() {
        let s = fetch_online("晴天", "周杰伦", std::time::Duration::from_secs(10));
        let s = s.expect("lrclib should return lyrics");
        assert!(s.contains("["), "should be LRC text: {}", &s[..s.len().min(80)]);
    }

    #[test]
    #[ignore = "network"]
    fn qq_fetch_returns_lrc() {
        let lrc = fetch_qq("晴天", "周杰伦", Some(269.0), std::time::Duration::from_secs(10));
        let lrc = lrc.expect("QQ should return lyrics");
        assert!(lrc.contains("[00:"), "should be LRC: {}", &lrc[..lrc.len().min(80)]);
        // duration validation: wrong-length song should be rejected (falls through to lrclib)
        let wrong = fetch_qq("晴天", "周杰伦", Some(30.0), std::time::Duration::from_secs(10));
        assert!(wrong.is_none(), "wrong-duration match should be rejected");
    }

    #[test]
    fn credit_lines_are_filtered() {
        let d = parse_lrc("[00:01.00]词：周杰伦\n[00:02.00]曲：周杰伦\n[00:03.00]编曲：钟兴民\n[00:04.00]这是真的歌词内容\n");
        assert_eq!(d.lines.len(), 1, "credit lines should be dropped");
        assert_eq!(d.lines[0].text, "这是真的歌词内容");
    }

    #[test]
    fn empty_timed_lines_are_dropped() {
        let d = parse_lrc("[00:01.00]第一行\n[00:02.00]   \n[00:03.00] 第二行 \n[00:04.00]\n[00:05.00]第三行\n");
        assert_eq!(d.lines.len(), 3, "empty timed lines must be filtered");
        assert_eq!(d.lines[0].text, "第一行");
        assert_eq!(d.lines[1].text, "第二行");
        assert_eq!(d.lines[2].text, "第三行");
    }
}

