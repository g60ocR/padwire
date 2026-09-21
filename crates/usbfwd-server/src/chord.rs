//! Recognising a button chord in Valve's input reports.
//!
//! The toggle needs to notice a deliberate button combination in two quite
//! different places — in the reports flowing through a live session, and in
//! the reports read off `hidraw` when there is no session at all — so the
//! decoding and the hold timing live here, away from both.
//!
//! ## The report
//!
//! A Steam Deck's own controls send 64-byte reports with a four-byte header:
//!
//! ```text
//! 0  0x01        header
//! 1  0x00
//! 2  type        0x09 for the Deck's state report
//! 3  length      payload bytes that follow
//! 4  packet number, le32
//! 8  buttons,    le64   <- the only field this module cares about
//! ```
//!
//! Anything that is not a state report — a keyboard report from lizard mode, a
//! feature-report reply, a different device's input entirely — fails the
//! header check and is ignored rather than guessed at. That is what makes it
//! safe to feed *every* interrupt IN completion through here.
//!
//! ## Naming bits
//!
//! [`BUTTONS`] maps names to bit positions following the layout SDL uses for
//! the Deck. It is a convenience, not the source of truth: run
//! `usbfwd-server --chord-probe`, hold the buttons, and read the bit numbers
//! straight off the device. A chord may always be written as bit numbers
//! (`b41+b42`) or as a raw mask (`0x60000000000`), which is what makes the
//! feature usable on a controller whose layout differs from the table.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, Instant};

/// `bReportType` of the Deck's controller-state report.
pub const DECK_STATE: u8 = 0x09;

/// Offset of the 64-bit button field, and therefore the shortest report that
/// can carry one.
const BUTTONS_AT: usize = 8;
const MIN_REPORT: usize = BUTTONS_AT + 8;

/// Bit positions in the button field, as SDL's Deck layout has them.
///
/// Verify with `--chord-probe` before trusting a name: this table is the one
/// part of the feature that cannot be checked without the hardware in hand,
/// and a wrong name is a chord that never fires.
pub const BUTTONS: &[(&str, u8)] = &[
    ("r2", 0),
    ("l2", 1),
    ("r1", 2),
    ("l1", 3),
    ("y", 4),
    ("b", 5),
    ("x", 6),
    ("a", 7),
    ("up", 8),
    ("right", 9),
    ("left", 10),
    ("down", 11),
    ("view", 12),
    ("steam", 13),
    ("menu", 14),
    ("l5", 15),
    ("r5", 16),
    ("lpad", 17),
    ("rpad", 18),
    ("lpadtouch", 19),
    ("rpadtouch", 20),
    ("l3", 22),
    ("r3", 26),
    ("l4", 41),
    ("r4", 42),
    ("qam", 50),
];

/// The button field of one state report, or `None` if this is not one.
pub fn buttons(report: &[u8]) -> Option<u64> {
    if report.len() < MIN_REPORT || report[0] != 0x01 || report[1] != 0x00 {
        return None;
    }
    if report[2] != DECK_STATE {
        return None;
    }
    // The header's own length field has to cover the button word, or this is a
    // shorter variant of the report and offset 8 means something else.
    if (report[3] as usize) < MIN_REPORT - 4 {
        return None;
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&report[BUTTONS_AT..BUTTONS_AT + 8]);
    Some(u64::from_le_bytes(b))
}

/// The buttons that have to be held together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    mask: u64,
}

impl Chord {
    pub fn mask(&self) -> u64 {
        self.mask
    }

    /// Subset match: the chord fires when its buttons are all down, whatever
    /// else is. Requiring nothing else to be pressed would make the chord
    /// depend on how still the sticks are being held.
    pub fn held_in(&self, buttons: u64) -> bool {
        buttons & self.mask == self.mask
    }
}

impl FromStr for Chord {
    type Err = String;

    /// `L4+R4`, `steam,l5`, `b41+b42` or `0x60000000000`, case-insensitive.
    fn from_str(s: &str) -> Result<Chord, String> {
        let mut mask = 0u64;
        let mut any = false;
        for tok in s.split(['+', ',']).map(str::trim).filter(|t| !t.is_empty()) {
            any = true;
            mask |= parse_token(&tok.to_ascii_lowercase())?;
        }
        if !any {
            return Err("a chord needs at least one button".into());
        }
        Ok(Chord { mask })
    }
}

fn parse_token(tok: &str) -> Result<u64, String> {
    if let Some(hex) = tok.strip_prefix("0x") {
        return u64::from_str_radix(hex, 16)
            .map_err(|_| format!("`{tok}` is not a 64-bit hex mask"));
    }
    let bit = tok.strip_prefix("bit").or_else(|| tok.strip_prefix('b'));
    if let Some(n) = bit.filter(|n| n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty()) {
        let n: u32 = n.parse().map_err(|_| format!("`{tok}` is not a bit"))?;
        if n > 63 {
            return Err(format!("bit {n} is outside the 64-bit button field"));
        }
        return Ok(1u64 << n);
    }
    BUTTONS
        .iter()
        .find(|(name, _)| *name == tok)
        .map(|(_, bit)| 1u64 << bit)
        .ok_or_else(|| {
            format!(
                "`{tok}` is not a known button. Known: {}. \
                 Bit numbers (`b41`) and raw masks (`0x...`) also work — \
                 `--chord-probe` prints what the device actually sends.",
                BUTTONS
                    .iter()
                    .map(|(n, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", describe(self.mask))
    }
}

/// Render a button mask the way `--chord-probe` prints it: names where the
/// table has one, bit numbers where it does not.
pub fn describe(mask: u64) -> String {
    if mask == 0 {
        return "none".into();
    }
    let mut parts = Vec::new();
    for bit in 0..64u8 {
        if mask & (1u64 << bit) == 0 {
            continue;
        }
        match BUTTONS.iter().find(|(_, b)| *b == bit) {
            Some((name, _)) => parts.push(name.to_string()),
            None => parts.push(format!("b{bit}")),
        }
    }
    parts.join("+")
}

/// What the caller should do with the report it just fed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verdict {
    /// The chord is down in this report, so it should not reach the importer.
    /// A held chord is an instruction to this program, not input for a game.
    pub swallow: bool,
    /// The chord has now been held long enough. Fires once per press.
    pub fired: bool,
}

/// Hold-to-fire, so a chord that happens to occur during play does nothing.
///
/// A fresh detector is **disarmed**: it will not fire until it has seen the
/// chord released at least once. That is not politeness, it is what stops the
/// toggle oscillating. Firing hands the device from one watcher to the other —
/// a session ending, or a session starting — and the new watcher's first
/// reports arrive while the buttons are, of course, still down. Without the
/// rule, letting go a fraction too slowly would toggle straight back.
pub struct Detector {
    chord: Chord,
    hold: Duration,
    /// When the current uninterrupted hold started.
    since: Option<Instant>,
    /// Cleared by firing, set by seeing the chord released, so one press is
    /// one toggle however long the buttons stay down.
    armed: bool,
}

impl Detector {
    pub fn new(chord: Chord, hold: Duration) -> Detector {
        Detector {
            chord,
            hold,
            since: None,
            armed: false,
        }
    }

    pub fn feed(&mut self, report: &[u8], now: Instant) -> Verdict {
        let Some(buttons) = buttons(report) else {
            // Not a state report: no evidence either way, so the hold is
            // neither advanced nor broken.
            return Verdict::default();
        };
        if !self.chord.held_in(buttons) {
            self.since = None;
            self.armed = true;
            return Verdict::default();
        }
        // Held, so it is an instruction rather than input either way — but it
        // only counts once the buttons have been seen up.
        let started = *self.since.get_or_insert(now);
        if !self.armed || now.duration_since(started) < self.hold {
            return Verdict {
                swallow: true,
                fired: false,
            };
        }
        self.armed = false;
        Verdict {
            swallow: true,
            fired: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-byte Deck state report with these buttons down.
    fn report(buttons: u64) -> Vec<u8> {
        let mut r = vec![0u8; 64];
        r[0] = 0x01;
        r[1] = 0x00;
        r[2] = DECK_STATE;
        r[3] = 60;
        r[4..8].copy_from_slice(&7u32.to_le_bytes());
        r[8..16].copy_from_slice(&buttons.to_le_bytes());
        r
    }

    #[test]
    fn button_names_bit_numbers_and_raw_masks_agree() {
        let by_name: Chord = "L4+R4".parse().unwrap();
        let by_bit: Chord = "b41,b42".parse().unwrap();
        let raw: Chord = "0x60000000000".parse().unwrap();
        assert_eq!(by_name, by_bit);
        assert_eq!(by_name, raw);
        assert_eq!(by_name.to_string(), "l4+r4");
    }

    #[test]
    fn a_misspelt_button_says_what_is_known() {
        let e = "l4+nonsense".parse::<Chord>().unwrap_err();
        assert!(e.contains("steam"), "unhelpful: {e}");
        assert!(e.contains("--chord-probe"), "unhelpful: {e}");
        assert!("".parse::<Chord>().is_err());
        assert!("b64".parse::<Chord>().is_err(), "past the end of the field");
        assert!("0xzz".parse::<Chord>().is_err());
    }

    #[test]
    fn only_deck_state_reports_decode() {
        assert_eq!(buttons(&report(0x1234)), Some(0x1234));
        // Right shape, wrong report type: a keyboard report in lizard mode.
        let mut kbd = report(0xffff);
        kbd[2] = 0x01;
        assert_eq!(buttons(&kbd), None);
        // Truncated, and a header that is not Valve's at all.
        assert_eq!(buttons(&report(0)[..12]), None);
        assert_eq!(buttons(&[0u8; 64]), None);
        // A state report whose length field cannot cover the button word.
        let mut short = report(0xff);
        short[3] = 8;
        assert_eq!(buttons(&short), None);
    }

    #[test]
    fn a_chord_fires_once_after_the_hold_and_not_before() {
        let chord: Chord = "l4+r4".parse().unwrap();
        let mut d = Detector::new(chord, Duration::from_millis(500));
        let t0 = Instant::now();

        // Other buttons alone do nothing.
        assert_eq!(d.feed(&report(0xff), t0), Verdict::default());

        // The chord goes down: swallowed immediately, but not yet fired.
        let held = chord.mask() | 0x80;
        let v = d.feed(&report(held), t0);
        assert!(v.swallow && !v.fired);
        let v = d.feed(&report(held), t0 + Duration::from_millis(499));
        assert!(v.swallow && !v.fired);

        // Long enough.
        let v = d.feed(&report(held), t0 + Duration::from_millis(500));
        assert!(v.swallow && v.fired);

        // Still held: swallowed, but it must not fire again.
        let v = d.feed(&report(held), t0 + Duration::from_secs(5));
        assert!(v.swallow && !v.fired, "one press, one toggle");
    }

    #[test]
    fn releasing_the_chord_restarts_the_hold() {
        let chord: Chord = "steam+l5".parse().unwrap();
        let mut d = Detector::new(chord, Duration::from_millis(500));
        let t0 = Instant::now();
        d.feed(&report(0), t0); // arm it
        assert!(d.feed(&report(chord.mask()), t0).swallow);
        // Let go before the hold elapses.
        assert_eq!(
            d.feed(&report(0), t0 + Duration::from_millis(300)),
            Verdict::default()
        );
        // Press again: the clock starts over, so the earlier 300 ms is gone.
        let t1 = t0 + Duration::from_millis(400);
        let v = d.feed(&report(chord.mask()), t1 + Duration::from_millis(499));
        assert!(!v.fired, "a released chord must not carry credit forward");
        assert!(
            d.feed(&report(chord.mask()), t1 + Duration::from_secs(1))
                .fired
        );
    }

    #[test]
    fn foreign_reports_neither_fire_nor_break_a_hold() {
        let chord: Chord = "l4+r4".parse().unwrap();
        let mut d = Detector::new(chord, Duration::from_millis(500));
        let t0 = Instant::now();
        d.feed(&report(0), t0); // arm it
        assert!(d.feed(&report(chord.mask()), t0).swallow);
        // A feature-report reply arriving mid-hold is not evidence of release.
        let v = d.feed(&[0x00, 0x01, 0x02], t0 + Duration::from_millis(100));
        assert_eq!(v, Verdict::default());
        assert!(
            d.feed(&report(chord.mask()), t0 + Duration::from_secs(1))
                .fired
        );
    }

    /// The oscillation guard: whichever side of the toggle a detector is
    /// created on, the chord that caused the handover is still down.
    #[test]
    fn a_chord_already_held_when_watching_starts_must_be_released_first() {
        let chord: Chord = "l4+r4".parse().unwrap();
        let mut d = Detector::new(chord, Duration::from_millis(500));
        let t0 = Instant::now();

        // Held from the first report and never let go: swallowed, never fired,
        // however long it is kept down.
        for ms in [0, 500, 5_000, 60_000] {
            let v = d.feed(&report(chord.mask()), t0 + Duration::from_millis(ms));
            assert!(v.swallow && !v.fired, "must not fire at {ms} ms");
        }

        // Released, then pressed again: an ordinary press, which does fire.
        d.feed(&report(0), t0 + Duration::from_millis(60_001));
        let t1 = t0 + Duration::from_millis(60_002);
        assert!(!d.feed(&report(chord.mask()), t1).fired);
        assert!(
            d.feed(&report(chord.mask()), t1 + Duration::from_secs(1))
                .fired
        );
    }

    #[test]
    fn unknown_bits_describe_themselves_by_number() {
        assert_eq!(describe(0), "none");
        assert_eq!(describe(1 << 60), "b60");
        assert_eq!(describe((1 << 13) | (1 << 60)), "steam+b60");
    }
}
