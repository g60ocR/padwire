//! `vid:pid` allow-listing.
//!
//! The exporter defaults to `28de:*` rather than "everything". Port 3240
//! carries plaintext USB traffic and a permissive default would make a
//! misconfigured bind address far more expensive than it needs to be.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pattern {
    /// `None` means `*`.
    pub vendor: Option<u16>,
    pub product: Option<u16>,
}

impl Pattern {
    pub fn matches(&self, vid: u16, pid: u16) -> bool {
        self.vendor.map_or(true, |v| v == vid) && self.product.map_or(true, |p| p == pid)
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.vendor {
            Some(v) => write!(f, "{v:04x}:")?,
            None => write!(f, "*:")?,
        }
        match self.product {
            Some(p) => write!(f, "{p:04x}"),
            None => write!(f, "*"),
        }
    }
}

impl FromStr for Pattern {
    type Err = String;

    fn from_str(s: &str) -> Result<Pattern, String> {
        let s = s.trim();
        let (v, p) = s
            .split_once(':')
            .ok_or_else(|| format!("`{s}` is not a vid:pid pattern"))?;
        Ok(Pattern {
            vendor: parse_part(v, s)?,
            product: parse_part(p, s)?,
        })
    }
}

fn parse_part(part: &str, whole: &str) -> Result<Option<u16>, String> {
    let part = part.trim();
    if part == "*" || part.is_empty() {
        return Ok(None);
    }
    u16::from_str_radix(part, 16)
        .map(Some)
        .map_err(|_| format!("`{part}` in `{whole}` is not a 16-bit hex value"))
}

/// Valve's vendor id. The 2015 controller, the Deck, the Proteus puck, the
/// Nereid receiver and the 2026 wired controller are all under it, which is
/// why a single default covers every device this project cares about.
pub const VALVE_VID: u16 = 0x28de;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFilter {
    patterns: Vec<Pattern>,
}

impl Default for DeviceFilter {
    fn default() -> Self {
        DeviceFilter {
            patterns: vec![Pattern {
                vendor: Some(VALVE_VID),
                product: None,
            }],
        }
    }
}

impl DeviceFilter {
    pub fn new(patterns: Vec<Pattern>) -> DeviceFilter {
        DeviceFilter { patterns }
    }

    /// An empty filter matches nothing, so `--allow ''` cannot accidentally
    /// widen the export list to the whole bus.
    pub fn matches(&self, vid: u16, pid: u16) -> bool {
        self.patterns.iter().any(|p| p.matches(vid, pid))
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }
}

impl FromStr for DeviceFilter {
    type Err = String;

    /// Comma-separated: `28de:1304,28de:1305` or `28de:*`.
    fn from_str(s: &str) -> Result<DeviceFilter, String> {
        let patterns = s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(Pattern::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DeviceFilter { patterns })
    }
}

impl fmt::Display for DeviceFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: Vec<String> = self.patterns.iter().map(|p| p.to_string()).collect();
        write!(f, "{}", s.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_valve_only() {
        let f = DeviceFilter::default();
        assert!(f.matches(0x28de, 0x1304), "Proteus puck");
        assert!(f.matches(0x28de, 0x1305), "Nereid receiver");
        assert!(
            f.matches(0x28de, 0x9999),
            "an unknown wired PID still matches"
        );
        assert!(!f.matches(0x046d, 0xc52b), "an unrelated dongle does not");
    }

    #[test]
    fn exact_patterns() {
        let f: DeviceFilter = "28de:1304,28de:1305".parse().unwrap();
        assert!(f.matches(0x28de, 0x1304));
        assert!(
            !f.matches(0x28de, 0x1205),
            "the Deck's own controls are excluded"
        );
    }

    #[test]
    fn wildcards_on_either_side() {
        let f: DeviceFilter = "*:1304".parse().unwrap();
        assert!(f.matches(0x1234, 0x1304));
        assert!(!f.matches(0x1234, 0x1305));
        let f: DeviceFilter = "*:*".parse().unwrap();
        assert!(f.matches(0, 0));
    }

    #[test]
    fn an_empty_filter_matches_nothing() {
        let f: DeviceFilter = "".parse().unwrap();
        assert!(f.is_empty());
        assert!(!f.matches(0x28de, 0x1304));
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        assert!("28de".parse::<DeviceFilter>().is_err());
        assert!("zzzz:1304".parse::<DeviceFilter>().is_err());
        assert!("28de:123456".parse::<DeviceFilter>().is_err());
    }

    #[test]
    fn display_roundtrips() {
        let f: DeviceFilter = "28de:1304,*:*".parse().unwrap();
        assert_eq!(f.to_string(), "28de:1304,*:*");
        assert_eq!(f.to_string().parse::<DeviceFilter>().unwrap(), f);
    }
}
