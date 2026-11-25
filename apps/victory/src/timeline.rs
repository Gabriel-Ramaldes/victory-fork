//! `victory timeline <capture folder> [log]`: a page of what a run put on
//! the screen, frame after frame.
//!
//! A diagnostics build run with `VICTORY_CAPTURE=<folder>` (and, for the
//! world, `VICTORY_CAPTURE_WORLD=1`) saves frames as PNG files and writes a
//! line of `timeline.jsonl` per saved frame (`victory-gpu`'s `capture`):
//! when it was presented, the draws the host ran and those it dropped since
//! the frame before it (by why), and what the shader cache and the pipeline
//! library had done so far. This puts the pictures, those numbers and the
//! run's own log lines between them on one page, `timeline.html` in the
//! folder, and prints a summary.
//!
//! Dropped draws are grouped by cause, which tells cache problems from
//! translator gaps: a pipeline still being made (its shaders or pipeline
//! were not in the caches yet: a cold or mismatched cache, a queue
//! backlog); a shader that does not translate (a translator gap: the stage
//! and hash of each shader); left out, refused by the host, the ray-tracing
//! structure builder off.
//!
//! Reads the folder and the log; writes only `timeline.html`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

/// One saved frame, as `timeline.jsonl` has it.
#[derive(Debug, Default, PartialEq)]
struct Frame {
    frame: u64,
    t_ms: u64,
    utc_ms: u64,
    png: String,
    world_frame: Option<u64>,
    drawn: u64,
    dropped: u64,
    drops: Vec<(String, u64)>,
    shaders_cached: u64,
    shaders_translated: u64,
    pipelines_loaded: u64,
    pipelines_made: u64,
    pipelines_queued: u64,
}

/// What a drop's why says it was, with its colour on the page.
fn cause(why: &str) -> (&'static str, &'static str) {
    const CAUSES: [(&str, &str, &str); 6] = [
        ("being made", "pipeline still being made", "#d08a1c"),
        ("left out", "left out", "#8a63d2"),
        ("refused", "refused by the host", "#c2413b"),
        ("ray-tracing structure builder", "ray-tracing builder off", "#2f7fb8"),
        ("skipped shader", "skipped shader", "#6b7785"),
        ("no pipeline set", "no pipeline set", "#6b7785"),
    ];
    for (needle, label, colour) in CAUSES {
        if why.contains(needle) {
            return (label, colour);
        }
    }
    // `make::missing`: "<stage> <hash>" for each shader that did not translate.
    let stage = why.split(' ').next().unwrap_or("");
    let hash = why.split(' ').nth(1).unwrap_or("");
    if ["vertex", "pixel", "mesh", "compute", "geometry", "hull", "domain"].contains(&stage)
        && hash.len() == 16
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return ("shader does not translate", "#2b9c6a");
    }
    ("other", "#9aa3ad")
}

pub fn run(folder: &Path, log: Option<&Path>) -> Result<()> {
    let jsonl = folder.join("timeline.jsonl");
    let text = std::fs::read_to_string(&jsonl).with_context(|| format!("reading {}", jsonl.display()))?;
    let frames: Vec<Frame> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| parse_frame(l).with_context(|| format!("{} line {}", jsonl.display(), i + 1)))
        .collect::<Result<_>>()?;
    anyhow::ensure!(!frames.is_empty(), "no frames in {}", jsonl.display());
    let logs = match log {
        Some(path) => log_lines(path)?,
        None => Vec::new(),
    };
    let (page, summary) = render(&frames, &logs, |png| folder.join(png).exists());
    let out = folder.join("timeline.html");
    std::fs::write(&out, page).with_context(|| format!("writing {}", out.display()))?;
    for line in summary {
        println!("{line}");
    }
    println!("page         {}", out.display());
    Ok(())
}

/// The page and the summary's lines.
fn render(frames: &[Frame], logs: &[(u64, String)], has_png: impl Fn(&str) -> bool) -> (String, Vec<String>) {
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    let mut cards = String::new();
    let (mut previous, mut li) = (0u64, 0usize);
    for f in frames {
        let mut by: BTreeMap<&str, (u64, &str)> = BTreeMap::new();
        for (why, n) in &f.drops {
            let (label, colour) = cause(why);
            by.entry(label).or_insert((0, colour)).0 += n;
            *totals.entry(label).or_default() += n;
        }
        let mut lines = Vec::new();
        while li < logs.len() && logs[li].0 <= f.utc_ms {
            if logs[li].0 > previous {
                lines.push(logs[li].1.as_str());
            }
            li += 1;
        }
        previous = f.utc_ms;
        let mut by: Vec<_> = by.into_iter().collect();
        by.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        let bar: String = by
            .iter()
            .map(|(label, (n, colour))| {
                format!(r#"<span style="flex:{n};background:{colour}" title="{}: {n}"></span>"#, esc(label))
            })
            .collect();
        let top: String = f
            .drops
            .iter()
            .take(6)
            .map(|(why, n)| format!("<li><b>{n}</b> {}</li>", esc(&why.chars().take(160).collect::<String>())))
            .collect();
        let picture = if has_png(&f.png) {
            format!(r#"<a href="{0}"><img loading="lazy" src="{0}"></a>"#, esc(&f.png))
        } else {
            r#"<div class="missing">no picture</div>"#.to_owned()
        };
        let world = f.world_frame.map(|w| format!(" · world frame {w}")).unwrap_or_default();
        let log_html: String = lines
            .iter()
            .rev()
            .take(8)
            .rev()
            .map(|l| format!("<div>{}</div>", esc(&l.chars().take(300).collect::<String>())))
            .collect();
        let _ = write!(
            cards,
            r#"<section class="card"><div class="pic">{picture}</div><div class="facts">
<h2>frame {}<small> · {:.1} s{world}</small></h2>
<p>drawn since the last saved frame <b>{}</b>, dropped <b>{}</b>; shaders {} from the cache, {} translated; pipelines {} loaded, {} made, {} queued</p>
<div class="bar">{bar}</div><ul>{top}</ul><div class="log">{log_html}</div></div></section>
"#,
            f.frame,
            f.t_ms as f64 / 1000.0,
            f.drawn,
            f.dropped,
            f.shaders_cached,
            f.shaders_translated,
            f.pipelines_loaded,
            f.pipelines_made,
            f.pipelines_queued,
        );
    }
    let in_world: Vec<&Frame> = frames.iter().filter(|f| f.world_frame.is_some()).collect();
    let mut summary = vec![format!("frames       {} saved, {} in the world", frames.len(), in_world.len())];
    if !in_world.is_empty() {
        let median = |v: Vec<u64>| {
            let mut v = v;
            v.sort_unstable();
            v[v.len() / 2]
        };
        summary.push(format!(
            "world        per saved frame: median {} drawn, {} dropped",
            median(in_world.iter().map(|f| f.drawn).collect()),
            median(in_world.iter().map(|f| f.dropped).collect())
        ));
    }
    let mut totals: Vec<_> = totals.into_iter().collect();
    totals.sort_by(|a, b| b.1.cmp(&a.1));
    for (label, n) in totals {
        summary.push(format!("dropped      {n} {label}"));
    }
    let last = frames.last().expect("frames");
    summary.push(format!(
        "caches       shaders {} from the cache, {} translated; pipelines {} loaded, {} made",
        last.shaders_cached, last.shaders_translated, last.pipelines_loaded, last.pipelines_made
    ));
    let legend: String = [
        ("pipeline still being made", "#d08a1c"),
        ("shader does not translate", "#2b9c6a"),
        ("left out", "#8a63d2"),
        ("refused by the host", "#c2413b"),
        ("ray-tracing builder off", "#2f7fb8"),
        ("other", "#9aa3ad"),
    ]
    .iter()
    .map(|(l, c)| format!(r#"<span class="key"><i style="background:{c}"></i>{l}</span>"#))
    .collect();
    let items: String = summary.iter().map(|s| format!("<li>{}</li>", esc(s))).collect();
    let page = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Render timeline</title>
<style>
:root {{ --bg:#f6f7f9; --fg:#1d232a; --card:#fff; --line:#dde2e8; --dim:#5d6875; }}
@media (prefers-color-scheme: dark) {{ :root {{ --bg:#14181d; --fg:#e4e8ed; --card:#1c2128; --line:#2c333c; --dim:#9aa5b1; }} }}
body {{ background:var(--bg); color:var(--fg); font:14px/1.45 system-ui,sans-serif; margin:0 auto; padding:16px; max-width:1200px; }}
h1 {{ font-size:20px; margin:0 0 8px; }}
.legend {{ display:flex; flex-wrap:wrap; gap:12px; margin:10px 0 18px; color:var(--dim); }}
.key i {{ display:inline-block; width:10px; height:10px; border-radius:2px; margin-right:5px; vertical-align:middle; }}
.card {{ display:grid; grid-template-columns:minmax(0,360px) minmax(0,1fr); gap:16px; background:var(--card); border:1px solid var(--line); border-radius:8px; padding:12px; margin-bottom:12px; }}
.card img {{ width:100%; border-radius:4px; display:block; }}
.missing {{ color:var(--dim); padding:40px 0; text-align:center; }}
h2 {{ font-size:16px; margin:0 0 4px; }} h2 small {{ color:var(--dim); font-weight:normal; }}
.facts p {{ margin:0 0 8px; color:var(--dim); }}
.bar {{ display:flex; height:10px; border-radius:3px; overflow:hidden; background:var(--line); margin-bottom:8px; }}
.facts ul {{ margin:0 0 8px; padding-left:18px; font-size:13px; }}
.log {{ font:12px/1.4 ui-monospace,monospace; color:var(--dim); overflow-wrap:anywhere; }}
@media (max-width:700px) {{ .card {{ grid-template-columns:1fr; }} }}
</style></head><body>
<h1>Render timeline</h1>
<ul>{items}</ul>
<div class="legend">{legend}</div>
{cards}</body></html>
"#
    );
    (page, summary)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// (unix ms, text) of the log lines worth showing between frames: the frame
/// lines (fps), warnings, errors, device and crash notes.
fn log_lines(path: &Path) -> Result<Vec<(u64, String)>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let Some((t, rest)) = log_time(&line) else { continue };
        let lower = rest.to_ascii_lowercase();
        if ["fps,", " warn ", " error ", "device", "removed", "left out", "crash"]
            .iter()
            .any(|w| lower.contains(w))
        {
            out.push((t, rest.trim().to_owned()));
        }
    }
    Ok(out)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A log line's leading `YYYY-MM-DDTHH:MM:SS.ffffffZ` as unix ms, and the rest.
fn log_time(line: &str) -> Option<(u64, &str)> {
    let end = line.find('Z')?;
    let stamp = &line[..end];
    let (date, time) = stamp.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, s) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days from 1970-01-01 (the civil-from-days inverse, Howard Hinnant's).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let unix = ((days * 24 + h) * 60 + mi) * 60 + s;
    Some((u64::try_from(unix * 1000 + ms).ok()?, &line[end + 1..]))
}

/// One line of `timeline.jsonl` (the subset of JSON `capture` writes).
fn parse_frame(line: &str) -> Result<Frame> {
    let value = Json::parse(line).context("not JSON")?;
    let Json::Object(fields) = value else { anyhow::bail!("not an object") };
    let mut f = Frame::default();
    for (key, v) in fields {
        let n = || v.number().with_context(|| format!("{key}: not a number"));
        match key.as_str() {
            "frame" => f.frame = n()?,
            "t_ms" => f.t_ms = n()?,
            "utc_ms" => f.utc_ms = n()?,
            "png" => f.png = v.string().context("png: not a string")?.to_owned(),
            "world_frame" => f.world_frame = v.number(),
            "drawn" => f.drawn = n()?,
            "dropped" => f.dropped = n()?,
            "shaders_cached" => f.shaders_cached = n()?,
            "shaders_translated" => f.shaders_translated = n()?,
            "pipelines_loaded" => f.pipelines_loaded = n()?,
            "pipelines_made" => f.pipelines_made = n()?,
            "pipelines_queued" => f.pipelines_queued = n()?,
            "drops" => {
                let Json::Array(items) = v else { anyhow::bail!("drops: not an array") };
                for item in items {
                    let Json::Array(pair) = item else { anyhow::bail!("drops: not pairs") };
                    match pair.as_slice() {
                        [why, n] => f.drops.push((
                            why.string().context("drops: why")?.to_owned(),
                            n.number().context("drops: count")?,
                        )),
                        _ => anyhow::bail!("drops: not pairs"),
                    }
                }
            }
            _ => {}
        }
    }
    Ok(f)
}

#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
    Bool(bool),
}

impl Json {
    fn number(&self) -> Option<u64> {
        match self {
            Json::Number(n) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }
    fn string(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    fn parse(text: &str) -> Option<Json> {
        let mut p = Parser { s: text.as_bytes(), at: 0 };
        let v = p.value()?;
        p.ws();
        (p.at == p.s.len()).then_some(v)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let ok = self.s.get(self.at) == Some(&c);
        self.at += usize::from(ok);
        ok
    }
    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.at)? {
            b'{' => {
                self.at += 1;
                let mut fields = Vec::new();
                if self.eat(b'}') {
                    return Some(Json::Object(fields));
                }
                loop {
                    self.ws();
                    let Json::String(key) = self.value()? else { return None };
                    if !self.eat(b':') {
                        return None;
                    }
                    fields.push((key, self.value()?));
                    if self.eat(b'}') {
                        return Some(Json::Object(fields));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                if self.eat(b']') {
                    return Some(Json::Array(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(b']') {
                        return Some(Json::Array(items));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'"' => {
                self.at += 1;
                let mut out = String::new();
                loop {
                    let c = *self.s.get(self.at)?;
                    self.at += 1;
                    match c {
                        b'"' => return Some(Json::String(out)),
                        b'\\' => {
                            let e = *self.s.get(self.at)?;
                            self.at += 1;
                            match e {
                                b'"' => out.push('"'),
                                b'\\' => out.push('\\'),
                                b'/' => out.push('/'),
                                b'n' => out.push('\n'),
                                b't' => out.push('\t'),
                                b'r' => out.push('\r'),
                                b'u' => {
                                    let hex = std::str::from_utf8(self.s.get(self.at..self.at + 4)?).ok()?;
                                    self.at += 4;
                                    out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                                }
                                _ => return None,
                            }
                        }
                        _ => {
                            // Whole UTF-8 sequences: copy up to the next quote or escape.
                            let start = self.at - 1;
                            while self.s.get(self.at).is_some_and(|b| *b != b'"' && *b != b'\\') {
                                self.at += 1;
                            }
                            out.push_str(std::str::from_utf8(&self.s[start..self.at]).ok()?);
                        }
                    }
                }
            }
            b'n' => self.word(b"null", Json::Null),
            b't' => self.word(b"true", Json::Bool(true)),
            b'f' => self.word(b"false", Json::Bool(false)),
            _ => {
                let start = self.at;
                while self.s.get(self.at).is_some_and(|b| b"+-.eE0123456789".contains(b)) {
                    self.at += 1;
                }
                std::str::from_utf8(&self.s[start..self.at]).ok()?.parse().ok().map(Json::Number)
            }
        }
    }
    fn word(&mut self, w: &[u8], v: Json) -> Option<Json> {
        let ok = self.s.get(self.at..self.at + w.len()) == Some(w);
        self.at += if ok { w.len() } else { 0 };
        ok.then_some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"frame":1890,"t_ms":381000,"utc_ms":1795567281000,"png":"frame-1890.png","world_frame":8,"drawn":9000,"dropped":1200,"drops":[["being made [vertex 0123456789abcdef]",700],["compute 89abcdef01234567",300],["say \"hi\" é",1]],"shaders_cached":1900,"shaders_translated":3,"pipelines_loaded":1500,"pipelines_made":40,"pipelines_queued":12}"#;

    #[test]
    fn a_timeline_line_reads_back() {
        let f = parse_frame(LINE).unwrap();
        assert_eq!(f.frame, 1890);
        assert_eq!(f.world_frame, Some(8));
        assert_eq!(f.drops[0], ("being made [vertex 0123456789abcdef]".to_owned(), 700));
        assert_eq!(f.drops[2].0, "say \"hi\" é");
        let before = parse_frame(&LINE.replace(r#""world_frame":8"#, r#""world_frame":null"#)).unwrap();
        assert_eq!(before.world_frame, None);
        assert!(parse_frame("{\"frame\":").is_err());
    }

    #[test]
    fn drops_are_grouped_by_cause() {
        assert_eq!(cause("being made [vertex 0123456789abcdef]").0, "pipeline still being made");
        assert_eq!(cause("compute 89abcdef01234567").0, "shader does not translate");
        assert_eq!(cause("vertex 0123456789abcdef + pixel fedcba9876543210").0, "shader does not translate");
        assert_eq!(cause("crashed the host's compiler in a pre-build, left out").0, "left out");
        assert_eq!(cause("a stage of the ray-tracing structure builder, which is off").0, "ray-tracing builder off");
        assert_eq!(cause("refused [compute 0123456789abcdef]").0, "refused by the host");
    }

    #[test]
    fn log_lines_fall_between_their_frames() {
        assert_eq!(log_time("2026-11-25T00:01:20.5Z  INFO x"), Some((1_795_564_880_500, "  INFO x")));
        assert_eq!(log_time("1970-01-01T00:00:01.000000Z"), Some((1000, "")));
        let f = parse_frame(LINE).unwrap();
        let logs = vec![(f.utc_ms - 1, "2.1 fps, 470 ms/frame".to_owned()), (f.utc_ms + 1, "later".to_owned())];
        let (page, summary) = render(&[f], &logs, |_| false);
        assert!(page.contains("2.1 fps") && !page.contains("later"));
        assert!(summary.iter().any(|s| s.contains("700 pipeline still being made")));
    }
}
