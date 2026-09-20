//! A parser for the slice of TOML this daemon's configuration actually uses.
//!
//! Supported: comments, `[table]`, `[[array-of-tables]]`, and `key = value`
//! where a value is a quoted string, an integer, a boolean, or an array of
//! strings (which may span lines). Not supported: nested keys, inline tables,
//! floats, dates, multi-line strings. A config file that needs any of those is
//! a config file that has outgrown this daemon.
//!
//! The point of hand-rolling it is the dependency graph: `serde` + `toml` is
//! around fifteen crates, and keeping the whole workspace at one dependency
//! (`libc`) is what makes a fully static musl build a non-event — which
//! matters on SteamOS, where the binary has to survive an OS update.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Array(Vec<String>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::Int(_) => "integer",
            Value::Bool(_) => "boolean",
            Value::Array(_) => "array",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Table {
    pub name: String,
    pub line: usize,
    pub entries: Vec<(String, Value, usize)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v)
    }

    pub fn line_of(&self, key: &str) -> usize {
        self.entries
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, _, l)| *l)
            .unwrap_or(self.line)
    }

    /// Keys that are not in `known`, so a typo does not silently do nothing.
    pub fn unknown_keys(&self, known: &[&str]) -> Vec<(String, usize)> {
        self.entries
            .iter()
            .filter(|(k, _, _)| !known.contains(&k.as_str()))
            .map(|(k, _, l)| (k.clone(), *l))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, Error> {
    Err(Error {
        line,
        message: message.into(),
    })
}

/// Parse into a flat, ordered list of tables. `[[server]]` appears once per
/// occurrence, in file order.
pub fn parse(input: &str) -> Result<Vec<Table>, Error> {
    let mut tables: Vec<Table> = Vec::new();
    // Keys before any header belong to an implicit root table.
    let mut current = Table {
        name: String::new(),
        line: 0,
        entries: Vec::new(),
    };

    let lines: Vec<&str> = input.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let lineno = i + 1;
        let raw = lines[i];
        i += 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("[[") {
            let name = rest.strip_suffix("]]").ok_or_else(|| Error {
                line: lineno,
                message: "unterminated [[table]] header".into(),
            })?;
            tables.push(std::mem::replace(
                &mut current,
                Table {
                    name: name.trim().to_owned(),
                    line: lineno,
                    entries: Vec::new(),
                },
            ));
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| Error {
                line: lineno,
                message: "unterminated [table] header".into(),
            })?;
            let name = name.trim().to_owned();
            // `current` has not been pushed yet, so it has to be checked too.
            if tables
                .iter()
                .chain(std::iter::once(&current))
                .any(|t| t.name == name)
            {
                return err(lineno, format!("[{name}] appears more than once"));
            }
            tables.push(std::mem::replace(
                &mut current,
                Table {
                    name,
                    line: lineno,
                    entries: Vec::new(),
                },
            ));
            continue;
        }

        let (key, mut value_text) = line.split_once('=').ok_or_else(|| Error {
            line: lineno,
            message: format!("expected `key = value`, found `{line}`"),
        })?;
        let key = key.trim().to_owned();
        if key.is_empty() {
            return err(lineno, "empty key");
        }
        if current.entries.iter().any(|(k, _, _)| *k == key) {
            return err(lineno, format!("`{key}` is set twice in the same table"));
        }

        // An array may run over several lines; keep pulling until the
        // brackets balance so the common multi-line style works.
        let mut owned;
        if value_text.trim_start().starts_with('[') && !brackets_balanced(value_text) {
            owned = value_text.to_owned();
            while i < lines.len() && !brackets_balanced(&owned) {
                owned.push(' ');
                owned.push_str(strip_comment(lines[i]));
                i += 1;
            }
            if !brackets_balanced(&owned) {
                return err(lineno, "unterminated array");
            }
            value_text = &owned;
        }

        let value = parse_value(value_text.trim(), lineno)?;
        current.entries.push((key, value, lineno));
    }

    tables.push(current);
    // Drop the root table unless something was written before the first header.
    tables.retain(|t| !t.name.is_empty() || !t.entries.is_empty());
    Ok(tables)
}

fn brackets_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut quote = '"';
    let mut prev_escape = false;
    for c in s.chars() {
        if in_str {
            if prev_escape {
                prev_escape = false;
            } else if c == '\\' && quote == '"' {
                prev_escape = true;
            } else if c == quote {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_str = true;
                quote = c;
            }
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    depth == 0 && !in_str
}

/// Remove a `#` comment, respecting quoted strings so a `#` inside a hostname
/// or pattern survives.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut quote = b'"';
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' && quote == b'"' {
                escape = true;
            } else if b == quote {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => {
                in_str = true;
                quote = b;
            }
            b'#' => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_value(text: &str, line: usize) -> Result<Value, Error> {
    match text {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "" => return err(line, "missing value"),
        _ => {}
    }
    if text.starts_with('[') {
        let inner = text
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| Error {
                line,
                message: "unterminated array".into(),
            })?;
        let mut out = Vec::new();
        for part in split_array(inner) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match parse_value(part, line)? {
                Value::Str(s) => out.push(s),
                other => {
                    return err(
                        line,
                        format!(
                            "arrays may only hold strings, found a {}",
                            other.type_name()
                        ),
                    )
                }
            }
        }
        return Ok(Value::Array(out));
    }
    if text.starts_with('"') || text.starts_with('\'') {
        return parse_string(text, line).map(Value::Str);
    }
    text.parse::<i64>().map(Value::Int).map_err(|_| Error {
        line,
        message: format!("`{text}` is not a string, integer or boolean"),
    })
}

fn split_array(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut quote = '"';
    let mut escape = false;
    for c in inner.chars() {
        if in_str {
            cur.push(c);
            if escape {
                escape = false;
            } else if c == '\\' && quote == '"' {
                escape = true;
            } else if c == quote {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                in_str = true;
                quote = c;
                cur.push(c);
            }
            ',' => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn parse_string(text: &str, line: usize) -> Result<String, Error> {
    let bytes: Vec<char> = text.chars().collect();
    let quote = bytes[0];
    if bytes.len() < 2 || bytes[bytes.len() - 1] != quote {
        return err(line, "unterminated string");
    }
    let body = &bytes[1..bytes.len() - 1];
    if quote == '\'' {
        // Literal string: no escapes at all, per TOML.
        return Ok(body.iter().collect());
    }
    let mut out = String::with_capacity(body.len());
    let mut it = body.iter();
    while let Some(&c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => return err(line, format!("unsupported escape `\\{other}`")),
            None => return err(line, "string ends in a backslash"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shipped_example_shape() {
        let src = r#"
# usbfwd.toml
[[server]]
name    = "steamdeck"
host    = "steamdeck.tailnet-name.ts.net"   # MagicDNS, or a raw IP
port    = 3240
devices = ["28de:1304", "28de:1305", "28de:*"]
auto_attach = true

[[server]]
name = "tablet"
host = "tablet.tailnet-name.ts.net"

[discovery]
mdns = false        # LAN convenience only; cannot work over Tailscale
"#;
        let t = parse(src).expect("parse");
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].name, "server");
        assert_eq!(t[0].get("name"), Some(&Value::Str("steamdeck".into())));
        assert_eq!(
            t[0].get("host"),
            Some(&Value::Str("steamdeck.tailnet-name.ts.net".into()))
        );
        assert_eq!(t[0].get("port"), Some(&Value::Int(3240)));
        assert_eq!(
            t[0].get("devices"),
            Some(&Value::Array(vec![
                "28de:1304".into(),
                "28de:1305".into(),
                "28de:*".into()
            ]))
        );
        assert_eq!(t[0].get("auto_attach"), Some(&Value::Bool(true)));
        assert_eq!(t[1].name, "server");
        assert_eq!(t[1].get("port"), None, "absent keys stay absent");
        assert_eq!(t[2].name, "discovery");
        assert_eq!(t[2].get("mdns"), Some(&Value::Bool(false)));
    }

    #[test]
    fn comments_inside_strings_survive() {
        let t = parse(r#"host = "a#b"  # trailing"#).unwrap();
        assert_eq!(t[0].get("host"), Some(&Value::Str("a#b".into())));
    }

    #[test]
    fn arrays_may_span_lines() {
        let t = parse(
            r#"
devices = [
  "28de:1304",   # puck
  "28de:*",
]
"#,
        )
        .unwrap();
        assert_eq!(
            t[0].get("devices"),
            Some(&Value::Array(vec!["28de:1304".into(), "28de:*".into()]))
        );
    }

    #[test]
    fn literal_and_escaped_strings() {
        let t = parse("a = 'c:\\path'\nb = \"tab\\there\"").unwrap();
        assert_eq!(t[0].get("a"), Some(&Value::Str("c:\\path".into())));
        assert_eq!(t[0].get("b"), Some(&Value::Str("tab\there".into())));
    }

    #[test]
    fn errors_carry_the_line_number() {
        let e = parse("[[server]]\nname = \"a\"\nthis is not toml\n").unwrap_err();
        assert_eq!(e.line, 3);
        let e = parse("[[server]]\nport = 1\nport = 2\n").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("twice"));
        let e = parse("[server\n").unwrap_err();
        assert_eq!(e.line, 1);
        assert!(parse("x = \"unterminated").is_err());
        assert!(
            parse("x = [1, 2]").is_err(),
            "non-string arrays are rejected"
        );
        assert!(parse("[a]\nx=1\n[a]\ny=2").is_err(), "duplicate tables");
    }

    #[test]
    fn unknown_keys_are_reportable() {
        let t = parse("[[server]]\nname = \"a\"\nhsot = \"typo\"").unwrap();
        assert_eq!(
            t[0].unknown_keys(&["name", "host"]),
            vec![("hsot".to_string(), 3)]
        );
    }

    #[test]
    fn an_empty_file_yields_no_tables() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("# just a comment\n\n").unwrap().is_empty());
    }
}
