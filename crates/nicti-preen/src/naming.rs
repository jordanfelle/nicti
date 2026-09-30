//! Filename / subfolder templates and Windows-safe path components (#57).
//!
//! ```text
//! template := (literal | "{{" | "}}" | token)*        token := "{" NAME (":" ARG)? "}"
//! NAME (case-insensitive): Filename | Sequence | Date | Rating | Make | Model | Folder
//! Sequence ARG: zero-pad width 1..=9         {Sequence:4} -> 0007
//! Date ARG: YYYY YY MM DD hh mm ss, plus literal - _ . and space      (default YYYYMMDD)
//! ```
//!
//! A filename template may not contain a path separator; the *subfolder* template
//! ([`Template::parse_subfolder`]) may (`/` or `\`) and is split into components at render time.
//! Every rendered component passes through [`sanitize_component`], applied on every OS so a
//! preset behaves the same wherever the files end up.
//!
//! Rendering is a pure function of ([`AssetFacts`], sequence number), so the result never depends
//! on export parallelism or scheduling: sequence numbers are assigned at plan time.

use std::fmt::Write as _;

/// Longest single path component we emit, in UTF-16 code units (NTFS's per-component limit).
pub const MAX_COMPONENT_UTF16: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unclosed `{{` in template")]
    Unclosed,
    #[error("stray `}}` in template (write `}}}}` for a literal brace)")]
    StrayClose,
    #[error("unknown token `{{{0}}}`")]
    UnknownToken(String),
    #[error("bad argument `{arg}` for token `{token}`")]
    BadArg { token: &'static str, arg: String },
    #[error("template is empty")]
    Empty,
    #[error("a filename template can't contain a path separator (use the subfolder setting)")]
    PathSeparator,
}

/// The photo-derived values a template can reference. Plain data so this crate needs neither the
/// catalog nor the filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssetFacts {
    pub asset_id: i64,
    /// Source file stem, without extension.
    pub stem: String,
    /// Name of the folder the source file lives in.
    pub folder: String,
    /// Capture time, when known (catalog `captured_at`).
    pub captured: Option<DateParts>,
    /// File mtime (unix seconds, UTC): the fallback when there is no capture time.
    pub mtime_unix: i64,
    /// `None` = unrated, `Some(-1)` = rejected, `Some(1..=5)` stars.
    pub rating: Option<i32>,
    pub make: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateParts {
    /// Parses the catalog's `YYYY-MM-DD HH:MM:SS` (dashed) text; tolerant of the EXIF colon form
    /// (`YYYY:MM:DD HH:MM:SS`) and a bare date.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (date, time) = match text.split_once([' ', 'T']) {
            Some((d, t)) => (d, t),
            None => (text, "00:00:00"),
        };
        let mut d = date.split(['-', ':']);
        let year: i32 = d.next()?.parse().ok()?;
        let month: u32 = d.next()?.parse().ok()?;
        let day: u32 = d.next()?.parse().ok()?;
        let mut t = time.split(':');
        let hour: u32 = t.next()?.parse().ok()?;
        let minute: u32 = t.next().unwrap_or("0").parse().ok()?;
        let second: u32 = t
            .next()
            .unwrap_or("0")
            .split(['.', '+', '-', 'Z'])
            .next()?
            .parse()
            .ok()?;
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 60
        {
            return None;
        }
        Some(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    /// Civil UTC date-time for a unix timestamp (Howard Hinnant's `civil_from_days`).
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (if m <= 2 { y + 1 } else { y }) as i32;
        Self {
            year,
            month: m,
            day: d,
            hour: (rem / 3_600) as u32,
            minute: (rem % 3_600 / 60) as u32,
            second: (rem % 60) as u32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DateTok {
    Year4,
    Year2,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Lit(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Filename,
    Sequence(u8),
    Date(Vec<DateTok>),
    Rating,
    Make,
    Model,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

fn parse_date_arg(arg: &str) -> Option<Vec<DateTok>> {
    let mut toks = Vec::new();
    let mut rest = arg;
    while !rest.is_empty() {
        let (tok, n) = if rest.starts_with("YYYY") {
            (DateTok::Year4, 4)
        } else if rest.starts_with("YY") {
            (DateTok::Year2, 2)
        } else if rest.starts_with("MM") {
            (DateTok::Month, 2)
        } else if rest.starts_with("DD") {
            (DateTok::Day, 2)
        } else if rest.starts_with("hh") {
            (DateTok::Hour, 2)
        } else if rest.starts_with("mm") {
            (DateTok::Minute, 2)
        } else if rest.starts_with("ss") {
            (DateTok::Second, 2)
        } else {
            let c = rest.chars().next()?;
            if !matches!(c, '-' | '_' | '.' | ' ') {
                return None;
            }
            (DateTok::Lit(c), c.len_utf8())
        };
        toks.push(tok);
        rest = &rest[n..];
    }
    (!toks.is_empty()).then_some(toks)
}

impl Template {
    /// A filename template: no path separators.
    pub fn parse(s: &str) -> Result<Self, TemplateError> {
        Self::parse_inner(s, false)
    }

    /// A subfolder template: `/` and `\` separate components.
    pub fn parse_subfolder(s: &str) -> Result<Self, TemplateError> {
        Self::parse_inner(s, true)
    }

    fn parse_inner(s: &str, allow_separators: bool) -> Result<Self, TemplateError> {
        if s.is_empty() {
            return Err(TemplateError::Empty);
        }
        let mut parts: Vec<Part> = Vec::new();
        let mut lit = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    lit.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    lit.push('}');
                }
                '}' => return Err(TemplateError::StrayClose),
                '{' => {
                    let mut body = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(ch) => body.push(ch),
                            None => return Err(TemplateError::Unclosed),
                        }
                    }
                    if !lit.is_empty() {
                        parts.push(Part::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(Self::parse_token(&body)?);
                }
                '/' | '\\' if !allow_separators => return Err(TemplateError::PathSeparator),
                c => lit.push(c),
            }
        }
        if !lit.is_empty() {
            parts.push(Part::Lit(lit));
        }
        if parts.is_empty() {
            return Err(TemplateError::Empty);
        }
        Ok(Template { parts })
    }

    fn parse_token(body: &str) -> Result<Part, TemplateError> {
        let (name, arg) = match body.split_once(':') {
            Some((n, a)) => (n, Some(a)),
            None => (body, None),
        };
        let bad = |token: &'static str, arg: &str| TemplateError::BadArg {
            token,
            arg: arg.to_string(),
        };
        let no_arg = |token: &'static str, part: Part| match arg {
            None => Ok(part),
            Some(a) => Err(bad(token, a)),
        };
        match name.to_ascii_lowercase().as_str() {
            "filename" => no_arg("Filename", Part::Filename),
            "rating" => no_arg("Rating", Part::Rating),
            "make" => no_arg("Make", Part::Make),
            "model" => no_arg("Model", Part::Model),
            "folder" => no_arg("Folder", Part::Folder),
            "sequence" => match arg {
                None => Ok(Part::Sequence(1)),
                Some(a) => match a.parse::<u8>() {
                    Ok(w) if (1..=9).contains(&w) => Ok(Part::Sequence(w)),
                    _ => Err(bad("Sequence", a)),
                },
            },
            "date" => match arg {
                None => Ok(Part::Date(parse_date_arg("YYYYMMDD").expect("valid"))),
                Some(a) => parse_date_arg(a)
                    .map(Part::Date)
                    .ok_or_else(|| bad("Date", a)),
            },
            _ => Err(TemplateError::UnknownToken(body.to_string())),
        }
    }

    /// Renders to raw (unsanitized) text.
    pub fn render(&self, facts: &AssetFacts, sequence: u32) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Filename => out.push_str(&facts.stem),
                Part::Folder => out.push_str(&facts.folder),
                Part::Make => out.push_str(facts.make.as_deref().unwrap_or("")),
                Part::Model => out.push_str(facts.model.as_deref().unwrap_or("")),
                Part::Rating => out.push_str(match facts.rating {
                    None => "0",
                    Some(r) if r < 0 => "X",
                    Some(0) => "0",
                    Some(1) => "1",
                    Some(2) => "2",
                    Some(3) => "3",
                    Some(4) => "4",
                    Some(_) => "5",
                }),
                Part::Sequence(width) => {
                    let _ = write!(out, "{:0width$}", sequence, width = *width as usize);
                }
                Part::Date(toks) => {
                    let d = facts
                        .captured
                        .unwrap_or_else(|| DateParts::from_unix(facts.mtime_unix));
                    for t in toks {
                        let _ = match t {
                            DateTok::Year4 => write!(out, "{:04}", d.year),
                            DateTok::Year2 => write!(out, "{:02}", d.year.rem_euclid(100)),
                            DateTok::Month => write!(out, "{:02}", d.month),
                            DateTok::Day => write!(out, "{:02}", d.day),
                            DateTok::Hour => write!(out, "{:02}", d.hour),
                            DateTok::Minute => write!(out, "{:02}", d.minute),
                            DateTok::Second => write!(out, "{:02}", d.second),
                            DateTok::Lit(c) => write!(out, "{c}"),
                        };
                    }
                }
            }
        }
        out
    }

    /// Renders and sanitizes as one filename stem (never empty: falls back to `untitled`).
    pub fn render_filename(&self, facts: &AssetFacts, sequence: u32) -> String {
        let s = sanitize_component(&self.render(facts, sequence));
        if s.is_empty() {
            "untitled".to_string()
        } else {
            s
        }
    }

    /// Renders and splits a subfolder template into sanitized components; empty components
    /// (and `.`/`..`, which sanitize to nothing) are dropped, so a template can never climb out
    /// of the destination.
    pub fn render_subfolder(&self, facts: &AssetFacts, sequence: u32) -> Vec<String> {
        self.render(facts, sequence)
            .split(['/', '\\'])
            .map(sanitize_component)
            .filter(|c| !c.is_empty())
            .collect()
    }
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7",
];

fn is_reserved_device_name(component: &str) -> bool {
    // The device name is what precedes the first dot ("nul.jpg" is still NUL).
    let base = component.split('.').next().unwrap_or("");
    let base = base.trim_end_matches(' ').to_ascii_uppercase();
    if RESERVED.contains(&base.as_str()) || matches!(base.as_str(), "LPT8" | "LPT9") {
        return true;
    }
    // Superscript-digit variants and the console pseudo-files are also reserved on Windows.
    matches!(
        base.as_str(),
        "COM\u{b9}"
            | "COM\u{b2}"
            | "COM\u{b3}"
            | "LPT\u{b9}"
            | "LPT\u{b2}"
            | "LPT\u{b3}"
            | "CONIN$"
            | "CONOUT$"
    )
}

/// Makes one path component safe on Windows (and harmless elsewhere): invalid characters and
/// control characters become `_`, trailing dots/spaces are trimmed (Windows strips them silently,
/// which can alias two names), a reserved device name gets a `_` prefix, and the result is capped
/// at [`MAX_COMPONENT_UTF16`] UTF-16 units on a character boundary. May return an empty string
/// (callers decide the fallback).
pub fn sanitize_component(input: &str) -> String {
    let mapped: String = input
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = mapped.trim_start().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = if is_reserved_device_name(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    out = truncate_utf16(&out, MAX_COMPONENT_UTF16);
    // Truncation can expose a new trailing dot/space.
    out.trim_end_matches(['.', ' ']).to_string()
}

/// Truncates to at most `max` UTF-16 code units, never splitting a character.
pub fn truncate_utf16(s: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> AssetFacts {
        AssetFacts {
            asset_id: 1,
            stem: "DSC_0042".into(),
            folder: "Con 2026".into(),
            captured: DateParts::parse("2026-09-27 14:03:09"),
            mtime_unix: 0,
            rating: Some(4),
            make: Some("NIKON CORPORATION".into()),
            model: Some("NIKON Z 8".into()),
        }
    }

    fn render(t: &str, seq: u32) -> String {
        Template::parse(t).unwrap().render_filename(&facts(), seq)
    }

    #[test]
    fn tokens_render_and_are_case_insensitive() {
        assert_eq!(render("{Filename}", 1), "DSC_0042");
        assert_eq!(render("{filename}_{SEQUENCE:4}", 7), "DSC_0042_0007");
        assert_eq!(render("{Sequence}", 12), "12");
        assert_eq!(render("{Date}", 1), "20260927");
        assert_eq!(
            render("{Date:YYYY-MM-DD_hh.mm.ss}", 1),
            "2026-09-27_14.03.09"
        );
        assert_eq!(render("{Date:YY}", 1), "26");
        assert_eq!(render("{Rating}", 1), "4");
        assert_eq!(render("{Make} {Model}", 1), "NIKON CORPORATION NIKON Z 8");
        assert_eq!(render("{Folder}", 1), "Con 2026");
        assert_eq!(render("a{{b}}c", 1), "a{b}c");
    }

    #[test]
    fn rating_and_missing_values_fall_back() {
        let mut f = facts();
        f.rating = None;
        f.make = None;
        f.captured = None;
        f.mtime_unix = 86_400 * 365; // 1971-01-01
        let t = Template::parse("{Rating}-{Make}-{Date}").unwrap();
        assert_eq!(t.render(&f, 1), "0--19710101");
        f.rating = Some(-1);
        assert_eq!(Template::parse("{Rating}").unwrap().render(&f, 1), "X");
    }

    #[test]
    fn an_all_empty_result_becomes_untitled() {
        let mut f = facts();
        f.make = None;
        assert_eq!(
            Template::parse("{Make}").unwrap().render_filename(&f, 1),
            "untitled"
        );
        assert_eq!(
            Template::parse("...").unwrap().render_filename(&f, 1),
            "untitled"
        );
    }

    #[test]
    fn every_parse_error_is_reported() {
        assert_eq!(Template::parse(""), Err(TemplateError::Empty));
        assert_eq!(Template::parse("{Filename"), Err(TemplateError::Unclosed));
        assert_eq!(Template::parse("a}b"), Err(TemplateError::StrayClose));
        assert!(matches!(
            Template::parse("{Nope}"),
            Err(TemplateError::UnknownToken(_))
        ));
        assert!(matches!(
            Template::parse("{Sequence:0}"),
            Err(TemplateError::BadArg { .. })
        ));
        assert!(matches!(
            Template::parse("{Sequence:10}"),
            Err(TemplateError::BadArg { .. })
        ));
        assert!(matches!(
            Template::parse("{Sequence:x}"),
            Err(TemplateError::BadArg { .. })
        ));
        assert!(matches!(
            Template::parse("{Date:YYYY/MM}"),
            Err(TemplateError::BadArg { .. })
        ));
        assert!(matches!(
            Template::parse("{Filename:3}"),
            Err(TemplateError::BadArg { .. })
        ));
        assert_eq!(Template::parse("a/b"), Err(TemplateError::PathSeparator));
        assert_eq!(Template::parse("a\\b"), Err(TemplateError::PathSeparator));
    }

    #[test]
    fn subfolder_templates_split_and_cannot_climb_out() {
        let t = Template::parse_subfolder("{Date:YYYY}/{Folder}/../x\\y").unwrap();
        assert_eq!(
            t.render_subfolder(&facts(), 1),
            vec!["2026", "Con 2026", "x", "y"]
        );
        let t = Template::parse_subfolder("/abs//path/").unwrap();
        assert_eq!(t.render_subfolder(&facts(), 1), vec!["abs", "path"]);
    }

    #[test]
    fn sanitize_replaces_invalid_characters_and_trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_component("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_component("bad\u{7}name\n"), "bad_name_");
        assert_eq!(sanitize_component("name. . "), "name");
        assert_eq!(sanitize_component("  lead"), "lead");
        assert_eq!(sanitize_component("."), "");
        assert_eq!(sanitize_component(".."), "");
        assert_eq!(sanitize_component("keep.dots.inside"), "keep.dots.inside");
    }

    #[test]
    fn sanitize_prefixes_reserved_device_names_case_insensitively() {
        for name in [
            "CON",
            "con",
            "NUL",
            "nul.jpg",
            "Com1",
            "LPT9.txt",
            "aux",
            "PRN",
            "COM\u{b9}",
        ] {
            assert_eq!(sanitize_component(name), format!("_{name}"), "{name}");
        }
        // Not reserved: only the exact device name (before the first dot) matches.
        assert_eq!(sanitize_component("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize_component("COM10"), "COM10");
        assert_eq!(sanitize_component("xNUL"), "xNUL");
    }

    #[test]
    fn sanitize_caps_length_on_a_character_boundary() {
        let long = "é".repeat(400);
        let out = sanitize_component(&long);
        assert_eq!(out.encode_utf16().count(), 255);
        // Astral characters are two UTF-16 units: 255 is odd, so the last one must be dropped whole.
        let astral = "😀".repeat(200);
        let out = sanitize_component(&astral);
        assert_eq!(out.encode_utf16().count(), 254);
        assert!(out.chars().all(|c| c == '😀'));
    }

    #[test]
    fn date_parts_parse_and_unix_conversion() {
        let d = DateParts::parse("2026:09:27 14:03:09").unwrap();
        assert_eq!((d.year, d.month, d.day, d.hour), (2026, 9, 27, 14));
        assert_eq!(DateParts::parse("2026-09-27").unwrap().hour, 0);
        assert!(DateParts::parse("garbage").is_none());
        assert!(DateParts::parse("2026-13-01 00:00:00").is_none());
        let epoch = DateParts::from_unix(0);
        assert_eq!((epoch.year, epoch.month, epoch.day), (1970, 1, 1));
        let leap = DateParts::from_unix(951_782_400); // 2000-02-29
        assert_eq!((leap.year, leap.month, leap.day), (2000, 2, 29));
        let before = DateParts::from_unix(-1);
        assert_eq!(
            (before.year, before.month, before.day, before.second),
            (1969, 12, 31, 59)
        );
    }
}
