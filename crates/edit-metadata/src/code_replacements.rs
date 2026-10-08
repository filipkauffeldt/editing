//! Photo Mechanic-compatible code replacements and hot codes.
//!
//! A code replacement file is plain UTF-8 tab-delimited text: column 1 is
//! a code, columns 2+ are replacement variants. Typing `=code=` in a
//! template field substitutes the first variant; `=code#2=` selects the
//! second. Lines that start with `//` are comments, and so is a trailing
//! column that starts with `//`. Codes are case-sensitive. A file may
//! contain variables (`{…}`) in a code (`={monthname}=` translates the
//! month through a lookup) and in a replacement (a macro row such as
//! `mydate` → `{day} {monthname} {year4}`).
//!
//! Hot codes extend the same file with declarations that earlier Photo
//! Mechanic versions ignore as comments (research `presets-metadata-ftp.md`
//! section 3):
//!
//! - `//==\t{serial}` — a code generator; several lines are tried in
//!   order ("try this OR that"). The evaluated text is the code to look up.
//! - `//##\tfull\tlast` — names for the replacement columns; a name is the
//!   hot code you invoke.
//! - `//??\t{cmnt}|Unknown\tOther` — per-column defaults, with `|`-chained
//!   fallbacks used when no generated code matches.
//! - `//$$\tns1` — starts a namespace; invoke as `[ns1.name]`. Definitions
//!   before any namespace line are the default `global` namespace.
//!
//! The classic delimiter is `=`; the invocation in the plan's core loop
//! and its demo is `\h1\`. Both hot-code spellings are accepted: `[name]`
//! (Photo Mechanic) and `\name\`. A hot-code name that has no column
//! definition falls back to a plain code of the same name, which makes
//! `\h1\` a shorthand for `=h1=` on a simple `code⇥replacement` file.
//!
//! Unknown codes are left verbatim (what Photo Mechanic does with a
//! not-yet-entered replacement, so late typing never destroys text), and
//! expansion is depth-capped so a self-referential row cannot loop.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::template::{self, TemplateVars};

/// Default code delimiter (Photo Mechanic's default character).
pub const DEFAULT_DELIMITER: char = '=';

/// Expansion nesting cap: a replacement that re-invokes a code stops
/// after this many levels and keeps the remaining text verbatim.
const MAX_DEPTH: u8 = 8;

/// The default hot-code namespace shared by plain `[name]` invocations.
const GLOBAL_NAMESPACE: &str = "global";

/// Hot-code declarations for one namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotNamespace {
    /// Code generators, tried in order until one yields a known code.
    pub generators: Vec<String>,
    /// Column names; the invocation name maps to a position here.
    pub names: Vec<String>,
    /// Per-column defaults when no generator matches (may contain `|`).
    pub defaults: Vec<String>,
}

fn default_delimiter() -> char {
    DEFAULT_DELIMITER
}

/// A parsed code replacement file: literal codes plus hot-code defs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeReplacements {
    #[serde(default = "default_delimiter")]
    delimiter: char,
    /// Literal code → replacement variants, in column order.
    #[serde(default)]
    codes: BTreeMap<String, Vec<String>>,
    /// Namespace name → hot-code declarations.
    #[serde(default)]
    namespaces: BTreeMap<String, HotNamespace>,
}

impl Default for CodeReplacements {
    fn default() -> Self {
        Self {
            delimiter: DEFAULT_DELIMITER,
            codes: BTreeMap::new(),
            namespaces: BTreeMap::new(),
        }
    }
}

impl CodeReplacements {
    /// Parse a replacement file. A UTF-8 BOM at the start is ignored.
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut namespace = GLOBAL_NAMESPACE.to_string();
        for raw in text.lines() {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = header(line, "//==") {
                let generator = rest.trim();
                if !generator.is_empty() {
                    out.namespace_mut(&namespace)
                        .generators
                        .push(generator.to_string());
                }
            } else if let Some(rest) = header(line, "//##") {
                let names = columns(rest);
                if !names.is_empty() {
                    out.namespace_mut(&namespace).names = names;
                }
            } else if let Some(rest) = header(line, "//??") {
                let defaults = columns(rest);
                if !defaults.is_empty() {
                    out.namespace_mut(&namespace).defaults = defaults;
                }
            } else if let Some(rest) = header(line, "//$$") {
                let name = rest.trim();
                if !name.is_empty() {
                    namespace = name.to_string();
                }
            } else if line.starts_with("//") {
                continue;
            } else {
                let cols = columns(line);
                // A trailing column that starts with `//` is a comment;
                // `header`-style trailing comments are dropped with it.
                let end = cols
                    .iter()
                    .position(|col| col.starts_with("//"))
                    .unwrap_or(cols.len());
                if end < 2 {
                    continue;
                }
                let code = &cols[0];
                if code.is_empty() {
                    continue;
                }
                out.codes.insert(code.clone(), cols[1..end].to_vec());
            }
        }
        out
    }

    /// Read and parse a replacement file.
    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    /// No codes and no hot-code declarations: expansion is a no-op.
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty() && self.namespaces.is_empty()
    }

    /// Number of literal codes in the file.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// The active delimiter (first character of a classic `=code=` token).
    pub fn delimiter(&self) -> char {
        self.delimiter
    }

    /// Replace the delimiter; kept off `parse` until a settings UI needs it.
    pub fn with_delimiter(mut self, delimiter: char) -> Self {
        self.delimiter = delimiter;
        self
    }

    fn namespace_mut(&mut self, namespace: &str) -> &mut HotNamespace {
        self.namespaces.entry(namespace.to_string()).or_default()
    }

    /// Expand code replacements, hot codes and `{…}` variables in one pass.
    pub fn expand(&self, text: &str, vars: &TemplateVars) -> String {
        self.expand_depth(text, vars, 0)
    }

    fn expand_depth(&self, text: &str, vars: &TemplateVars, depth: u8) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        loop {
            let mut next: Option<(usize, char)> = None;
            for marker in [self.delimiter, '[', '\\', '{'] {
                if let Some(index) = rest.find(marker)
                    && next.is_none_or(|(best, _)| index < best)
                {
                    next = Some((index, marker));
                }
            }
            let Some((index, marker)) = next else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..index]);
            let tail = &rest[index..];
            match marker {
                '{' => match tail.find('}') {
                    Some(close) => {
                        out.push_str(&template::expand(&tail[..close + 1], vars));
                        rest = &tail[close + 1..];
                    }
                    None => {
                        out.push_str(tail);
                        break;
                    }
                },
                '[' => match tail.find(']') {
                    Some(close) => {
                        let name = &tail[1..close];
                        match self.resolve_hot(name, vars, depth) {
                            Some(value) => out.push_str(&value),
                            None => out.push_str(&tail[..close + 1]),
                        }
                        rest = &tail[close + 1..];
                    }
                    None => {
                        out.push_str(tail);
                        break;
                    }
                },
                '\\' => match tail[1..].find('\\').map(|at| at + 1) {
                    Some(close) => {
                        let name = &tail[1..close];
                        match self.resolve_hot(name, vars, depth) {
                            Some(value) => out.push_str(&value),
                            None => out.push_str(&tail[..close + 1]),
                        }
                        rest = &tail[close + 1..];
                    }
                    None => {
                        out.push_str(tail);
                        break;
                    }
                },
                delimiter => match tail[1..].find(delimiter).map(|at| at + 1) {
                    Some(close) => {
                        let inner = &tail[1..close];
                        match self.resolve_code(inner, vars, depth) {
                            Some(value) => out.push_str(&value),
                            None => out.push_str(&tail[..close + 1]),
                        }
                        rest = &tail[close + 1..];
                    }
                    None => {
                        out.push_str(tail);
                        break;
                    }
                },
            }
        }
        out
    }

    /// Resolve `code` (possibly carrying a `#N` selector) to its variant.
    /// Variables in the code are evaluated first, so `={monthname}=` looks
    /// up the month's translated row. Unknown codes yield `None`.
    fn resolve_code(&self, inner: &str, vars: &TemplateVars, depth: u8) -> Option<String> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let (raw_code, selector) = split_selector(inner);
        let code = template::expand(raw_code, vars);
        let variants = self.codes.get(&code)?;
        let chosen = selector
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| variants.get(index))
            .or_else(|| variants.first())?;
        Some(self.expand_depth(chosen, vars, depth + 1))
    }

    /// Resolve a hot-code name (optionally `ns.name`) to its column value.
    fn resolve_hot(&self, name: &str, vars: &TemplateVars, depth: u8) -> Option<String> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let (namespace, name) = name.split_once('.').unwrap_or((GLOBAL_NAMESPACE, name));
        if let Some(defs) = self.namespaces.get(namespace)
            && let Some(index) = defs.names.iter().position(|n| n.as_str() == name)
        {
            for generator in &defs.generators {
                let code = template::expand(generator, vars);
                if code.is_empty() {
                    continue;
                }
                if let Some(row) = self.codes.get(&code) {
                    let value = row.get(index).cloned().unwrap_or_default();
                    return Some(self.expand_depth(&value, vars, depth + 1));
                }
            }
            let value = defs
                .defaults
                .get(index)
                .map(|spec| evaluate_default(spec, vars))
                .unwrap_or_default();
            return Some(self.expand_depth(&value, vars, depth + 1));
        }
        // Plain-code fallback: makes `\h1\` a shorthand for `=h1=` when the
        // file defines no hot-code column named `h1`.
        let value = self.codes.get(name)?.first()?.clone();
        Some(self.expand_depth(&value, vars, depth + 1))
    }
}

/// Split off a trailing `#N` variant selector; `None` means variant 1.
fn split_selector(inner: &str) -> (&str, Option<usize>) {
    if let Some((code, number)) = inner.rsplit_once('#')
        && let Ok(index) = number.trim().parse::<usize>()
    {
        return (code, Some(index));
    }
    (inner, None)
}

/// Split a header line into its columns after a `//xx` tag, accepting the
/// documented tab or its absence.
fn header<'a>(line: &'a str, tag: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(tag)?;
    Some(rest.strip_prefix('\t').unwrap_or(rest))
}

/// Tab-split a header payload, dropping a trailing empty field.
fn columns(text: &str) -> Vec<String> {
    let mut cols: Vec<String> = text.split('\t').map(str::to_string).collect();
    while cols.last().is_some_and(|col| col.is_empty()) {
        cols.pop();
    }
    cols
}

/// Evaluate one `//??` default: `|`-separated alternatives, first non-empty
/// expanded value wins (Photo Mechanic's chained fallback).
fn evaluate_default(spec: &str, vars: &TemplateVars) -> String {
    let mut last = String::new();
    for alternative in spec.split('|') {
        last = template::expand(alternative, vars);
        if !last.trim().is_empty() {
            return last;
        }
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            sequence: 7,
            filename: "AHQ_0701.JPG".into(),
            base_name: "AHQ_0701".into(),
            ext: "JPG".into(),
            session: "2026-10-07 run".into(),
            taken_at: Some(1_791_374_400), // 2026-10-07 12:00:00 UTC
            camera: Some("EOS R6".into()),
            lens: Some("RF24-105mm".into()),
            iso: Some(6400),
            aperture: Some(2.8),
            shutter: Some("1/250".into()),
            focal: Some(24),
        }
    }

    #[test]
    fn parses_codes_with_variants_and_comments() {
        let file = "// a full-line comment\n\
                    wc8\tAlex Ovechkin\tLeft wing\n\
                    ff\tstaff\tJane Smith\t// trailing comment\n\
                    \n\
                    //==\t{base}\n\
                    //##\tfull\tlast\n\
                    //??\tUnknown\tOther\n\
                    AHQ_0701\tDennis Walker\tWalker\n";
        let repl = CodeReplacements::parse(file);
        assert_eq!(repl.len(), 3);
        assert_eq!(repl.delimiter(), '=');
        let v = vars();
        assert_eq!(repl.expand("=wc8=", &v), "Alex Ovechkin");
        assert_eq!(repl.expand("=wc8#2=", &v), "Left wing");
        assert_eq!(repl.expand("=ff=", &v), "staff", "trailing comment dropped");
        assert_eq!(repl.expand("=ff#2=", &v), "Jane Smith");
        // The `//==`/`//##`/`//??` lines are declarations, not codes.
        assert_eq!(repl.len(), 3);
    }

    #[test]
    fn unknown_codes_stay_verbatim() {
        let repl = CodeReplacements::parse("known\tvalue\n");
        let v = vars();
        assert_eq!(repl.expand("x =missing= y", &v), "x =missing= y");
        assert_eq!(repl.expand("=known=", &v), "value");
        assert_eq!(
            repl.expand("unterminated =known", &v),
            "unterminated =known"
        );
        assert_eq!(repl.expand("plain text", &v), "plain text");
    }

    #[test]
    fn variables_expand_in_codes_replacements_and_plain_text() {
        let month_file = "January\tEnero\nJuly\tJulio\nmydate\t{day}/{monthname}/{year4}\n";
        let repl = CodeReplacements::parse(month_file);
        let v = vars(); // October, so no July row: stays verbatim
        assert_eq!(repl.expand("={monthname}=", &v), "={monthname}=");
        assert_eq!(
            repl.expand("={monthname}= {yyyy}", &v),
            "={monthname}= 2026"
        );
        assert_eq!(repl.expand("=mydate=", &v), "07/Oct/2026");
    }

    #[test]
    fn hot_codes_resolve_by_generator() {
        let file = "//==\t{base}\n\
                    //==\t{model}\n\
                    //##\tfull\tlast\ttitle\n\
                    //??\t{camera}|Unknown\tOther\t\n\
                    AHQ_0701\tDennis Walker\tWalker\tPhoto Nerd\n";
        let repl = CodeReplacements::parse(file);
        let v = vars();
        assert_eq!(repl.expand("[full] [last]", &v), "Dennis Walker Walker");
        assert_eq!(repl.expand("[title]", &v), "Photo Nerd");
        // `\name\` is the plan's alternate spelling.
        assert_eq!(repl.expand("\\full\\", &v), "Dennis Walker");
        // No generator code matches a default-only name.
        let v2 = TemplateVars {
            base_name: "OTHER".into(),
            camera: None,
            ..vars()
        };
        assert_eq!(repl.expand("[full]/[last]", &v2), "Unknown/Other");
    }

    #[test]
    fn hot_code_defaults_chain_on_empty_variables() {
        let file = "//==\t{base}\n\
                    //##\tfull\n\
                    //??\t{camera}|Unknown\n\
                    AHQ_0701\tDennis Walker\n";
        let repl = CodeReplacements::parse(file);
        let v = TemplateVars {
            base_name: "OTHER".into(),
            camera: None,
            ..vars()
        };
        assert_eq!(repl.expand("[full]", &v), "Unknown");
    }

    #[test]
    fn hot_code_namespaces_isolate_generators() {
        let file = "//$$\tns1\n\
                    //==\tiso{iso}\n\
                    //##\tspeed\n\
                    //$$\tns2\n\
                    //==\tyear{yyyy}\n\
                    //##\tdecade\n\
                    iso6400\ttwo thousand ISO\n\
                    year2026\tthe 2020s\n";
        let repl = CodeReplacements::parse(file);
        let v = vars();
        assert_eq!(repl.expand("[ns1.speed]", &v), "two thousand ISO");
        assert_eq!(repl.expand("[ns2.decade]", &v), "the 2020s");
        // An unknown namespace stays verbatim.
        assert_eq!(repl.expand("[ns3.speed]", &v), "[ns3.speed]");
    }

    #[test]
    fn plain_code_hot_fallback_supports_backslash_demo() {
        let repl = CodeReplacements::parse("h1\tSHL\nd1\tDay one caption\n");
        let v = vars();
        assert_eq!(repl.expand("\\h1\\", &v), "SHL");
        assert_eq!(repl.expand("Byline: \\h1\\", &v), "Byline: SHL");
        assert_eq!(repl.expand("=h1=", &v), "SHL");
        assert_eq!(repl.expand("\\missing\\", &v), "\\missing\\");
    }

    #[test]
    fn self_referential_code_terminates() {
        let repl = CodeReplacements::parse("loop\t=loop=\n");
        let v = vars();
        let out = repl.expand("=loop=", &v);
        assert!(out.contains("loop"), "no hang, bounded output: {out}");
    }

    #[test]
    fn empty_and_whitespace_files_are_no_ops() {
        let repl = CodeReplacements::parse("\n\n   \n");
        assert!(repl.is_empty());
        assert_eq!(
            repl.expand("=anything= {date}", &vars()),
            "=anything= 2026-10-07"
        );
    }

    #[test]
    fn round_trips_through_serde() {
        let repl =
            CodeReplacements::parse("//$$\tns\n//==\t{base}\n//##\tfull\n//??\tUnknown\nh1\tSHL\n");
        let json = serde_json::to_string(&repl).unwrap();
        let back: CodeReplacements = serde_json::from_str(&json).unwrap();
        assert_eq!(back, repl);
    }
}
