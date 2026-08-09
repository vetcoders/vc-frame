// for more info, please see: https://sw.kovidgoyal.net/kitty/keyboard-protocol
use zellij_utils::data::KeyWithModifier;

#[derive(Debug)]
enum KittyKeysParsingState {
    Ground,
    ReceivedEscapeCharacter,
    ParsingNumber,
    ParsingModifiers,
    DoneParsingWithU,
    DoneParsingWithTilde,
}

/// Outcome of `KittyKeyboardParser::feed()`. One chunk can carry any
/// number of complete sequences (SSH and PTY buffering coalesce fast
/// keypresses into a single read) plus at most one unresolved tail.
#[derive(Debug)]
pub struct KittyFeedResult {
    /// Every complete sequence resolved from this chunk, in order, each
    /// paired with its own raw bytes (including any stashed prefix from
    /// earlier chunks).
    pub keys: Vec<(KeyWithModifier, Vec<u8>)>,
    /// How many bytes of *this* chunk belong to the completed keys
    /// above. The caller's fallback parser should only see the bytes
    /// from this index on — never the ones already resolved here.
    pub consumed_up_to: usize,
    /// What the trailing bytes (if any) turned out to be.
    pub rest: KittyFeedRest,
}

#[derive(Debug)]
pub enum KittyFeedRest {
    /// Everything resolved; parser is back at Ground.
    Consumed,
    /// Trailing bytes are a valid prefix; parser keeps state so the
    /// next chunk's continuation completes the sequence.
    Incomplete,
    /// Trailing bytes are not a Kitty sequence; parser reset to Ground.
    /// The fallback parser should handle the unconsumed tail.
    Passthrough,
}

#[derive(Debug)]
pub struct KittyKeyboardParser {
    state: KittyKeysParsingState,
    number_bytes: Vec<u8>,
    modifier_bytes: Vec<u8>,
    /// Raw bytes of the sequence currently being parsed, retained
    /// across chunks so a completed key can report its exact bytes.
    seq_bytes: Vec<u8>,
}

/// CSI final-byte range (0x40..=0x7E), minus `u` and `~` which trigger
/// the explicit `DoneParsingWith{U,Tilde}` states inside the parser.
/// A trailing letter in this range while still in
/// `ParsingNumber`/`ParsingModifiers` indicates a complete
/// letter-terminated sequence (e.g. `\x1b[A`, `\x1b[1;2A`).
fn is_csi_final_letter(b: u8) -> bool {
    (0x40..=0x7E).contains(&b) && b != b'u' && b != b'~'
}

impl KittyKeyboardParser {
    pub fn new() -> Self {
        KittyKeyboardParser {
            state: KittyKeysParsingState::Ground,
            number_bytes: vec![],
            modifier_bytes: vec![],
            seq_bytes: vec![],
        }
    }

    fn reset(&mut self) {
        self.state = KittyKeysParsingState::Ground;
        self.number_bytes.clear();
        self.modifier_bytes.clear();
        self.seq_bytes.clear();
    }

    /// Stateful, cross-chunk-aware entry point.
    /// * Sequences are resolved per byte, not once per chunk — SSH and
    ///   PTY buffering coalesce fast keypresses into a single read, so
    ///   one chunk routinely carries several complete sequences. Each
    ///   resolved key is collected; the parser resets and keeps going.
    /// * A trailing valid prefix preserves state (`Incomplete`), so a
    ///   sequence split across chunks resolves on a follow-up call.
    /// * A byte that breaks the state machine resets to Ground and
    ///   reports `Passthrough` — the keys already resolved survive, and
    ///   the caller hands the unconsumed tail to the fallback parser.
    pub fn feed(&mut self, bytes: &[u8]) -> KittyFeedResult {
        let mut keys = vec![];
        let mut consumed_up_to = 0;
        for (i, &byte) in bytes.iter().enumerate() {
            self.seq_bytes.push(byte);
            // A CSI final letter only terminates a sequence when it was
            // consumed as data *within* ParsingNumber/ParsingModifiers —
            // not when it caused the transition into that state (the
            // '[' of the CSI introducer is itself in the final-letter
            // byte range).
            let was_parsing_number = matches!(self.state, KittyKeysParsingState::ParsingNumber);
            let was_parsing_modifiers =
                matches!(self.state, KittyKeysParsingState::ParsingModifiers);
            if !self.advance(byte) {
                self.reset();
                return KittyFeedResult {
                    keys,
                    consumed_up_to,
                    rest: KittyFeedRest::Passthrough,
                };
            }
            // A sequence can complete on any byte, not just the last of
            // the chunk. `u`/`~` terminate via their Done states; a CSI
            // final letter terminates `\x1b[A`- and `\x1b[1;2A`-style
            // sequences in place.
            let completed = match self.state {
                KittyKeysParsingState::DoneParsingWithU => Some(
                    KeyWithModifier::from_bytes_with_u(&self.number_bytes, &self.modifier_bytes),
                ),
                KittyKeysParsingState::DoneParsingWithTilde => {
                    Some(KeyWithModifier::from_bytes_with_tilde(
                        &self.number_bytes,
                        &self.modifier_bytes,
                    ))
                },
                KittyKeysParsingState::ParsingNumber
                    if was_parsing_number && is_csi_final_letter(byte) =>
                {
                    Some(KeyWithModifier::from_bytes_with_no_ending_byte(
                        &self.number_bytes,
                        &self.modifier_bytes,
                    ))
                },
                KittyKeysParsingState::ParsingModifiers
                    if was_parsing_modifiers && is_csi_final_letter(byte) =>
                {
                    let last_modifier = self.modifier_bytes.pop().unwrap();
                    Some(KeyWithModifier::from_bytes_with_no_ending_byte(
                        &[last_modifier],
                        &self.modifier_bytes,
                    ))
                },
                _ => None,
            };
            if let Some(result) = completed {
                let seq = std::mem::take(&mut self.seq_bytes);
                self.reset();
                match result {
                    Some(k) => {
                        keys.push((k, seq));
                        consumed_up_to = i + 1;
                    },
                    None => {
                        // Structurally valid but unknown sequence — let
                        // the fallback parser see the unconsumed tail.
                        return KittyFeedResult {
                            keys,
                            consumed_up_to,
                            rest: KittyFeedRest::Passthrough,
                        };
                    },
                }
            }
        }
        let rest = match self.state {
            KittyKeysParsingState::Ground => KittyFeedRest::Consumed,
            _ => KittyFeedRest::Incomplete,
        };
        KittyFeedResult {
            keys,
            consumed_up_to,
            rest,
        }
    }

    pub fn advance(&mut self, byte: u8) -> bool {
        // returns false if we failed parsing
        match (&self.state, byte) {
            // Only ESC opens a sequence. A bare '[' (0x5b) is a
            // printable character — treating it as a prefix strands the
            // parser mid-state, where it swallows the next keypress.
            (KittyKeysParsingState::Ground, 0x1b) => {
                self.state = KittyKeysParsingState::ReceivedEscapeCharacter;
            },
            (KittyKeysParsingState::ReceivedEscapeCharacter, 91) => {
                self.state = KittyKeysParsingState::ParsingNumber;
            },
            (KittyKeysParsingState::ParsingNumber, 59) => {
                // semicolon
                if self.number_bytes == [49] {
                    self.number_bytes.clear();
                }
                self.state = KittyKeysParsingState::ParsingModifiers;
            },
            (
                KittyKeysParsingState::ParsingNumber | KittyKeysParsingState::ParsingModifiers,
                117,
            ) => {
                // u
                self.state = KittyKeysParsingState::DoneParsingWithU;
            },
            (
                KittyKeysParsingState::ParsingNumber | KittyKeysParsingState::ParsingModifiers,
                126,
            ) => {
                // ~
                self.state = KittyKeysParsingState::DoneParsingWithTilde;
            },
            (KittyKeysParsingState::ParsingNumber, _) => {
                self.number_bytes.push(byte);
            },
            (KittyKeysParsingState::ParsingModifiers, _) => {
                self.modifier_bytes.push(byte);
            },
            (_, _) => {
                return false;
            },
        }
        true
    }
}

/// Test helper. Drives the production `feed()` entry point on a single
/// chunk and projects its outcome onto an `Option` so the existing
/// assertion shape (`Some(KeyWithModifier { … })`) stays readable. The
/// full-byte tests in this file expect the input to be exactly one
/// complete sequence; anything else flattens to `None`.
#[cfg(test)]
fn parse_for_test(bytes: &[u8]) -> Option<KeyWithModifier> {
    let mut result = KittyKeyboardParser::new().feed(bytes);
    match (result.keys.len(), &result.rest) {
        (1, KittyFeedRest::Consumed) => Some(result.keys.remove(0).0),
        _ => None,
    }
}

#[test]
pub fn can_parse_bare_keys() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('a'))),
        "Can parse a bare 'a' keypress"
    );
    let key = "\u{1b}[49u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('1'))),
        "Can parse a bare '1' keypress"
    );
    let key = "\u{1b}[27u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Esc)),
        "Can parse a bare 'ESC' keypress"
    );
    let key = "\u{1b}[13u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Enter)),
        "Can parse a bare 'ENTER' keypress"
    );
    let key = "\u{1b}[9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Tab)),
        "Can parse a bare 'Tab' keypress"
    );
    let key = "\u{1b}[127u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Backspace)),
        "Can parse a bare 'Backspace' keypress"
    );
    let key = "\u{1b}[57358u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::CapsLock)),
        "Can parse a bare 'CapsLock' keypress"
    );
    let key = "\u{1b}[57359u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::ScrollLock)),
        "Can parse a bare 'ScrollLock' keypress"
    );
    let key = "\u{1b}[57360u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::NumLock)),
        "Can parse a bare 'NumLock' keypress"
    );
    let key = "\u{1b}[57361u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PrintScreen)),
        "Can parse a bare 'PrintScreen' keypress"
    );
    let key = "\u{1b}[57362u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Pause)),
        "Can parse a bare 'Pause' keypress"
    );
    let key = "\u{1b}[57363u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Menu)),
        "Can parse a bare 'Menu' keypress"
    );

    let key = "\u{1b}[2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Insert)),
        "Can parse a bare 'Insert' keypress"
    );
    let key = "\u{1b}[3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Delete)),
        "Can parse a bare 'Delete' keypress"
    );
    let key = "\u{1b}[5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageUp)),
        "Can parse a bare 'PageUp' keypress"
    );
    let key = "\u{1b}[6~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageDown)),
        "Can parse a bare 'PageDown' keypress"
    );
    let key = "\u{1b}[7~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home)),
        "Can parse a bare 'Home' keypress"
    );
    let key = "\u{1b}[8~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End)),
        "Can parse a bare 'End' keypress"
    );
    let key = "\u{1b}[11~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1))),
        "Can parse a bare 'F1' keypress"
    );
    let key = "\u{1b}[12~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2))),
        "Can parse a bare 'F2' keypress"
    );
    let key = "\u{1b}[13~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(3))),
        "Can parse a bare 'F3' keypress"
    );
    let key = "\u{1b}[14~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4))),
        "Can parse a bare 'F4' keypress"
    );
    let key = "\u{1b}[15~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(5))),
        "Can parse a bare 'F5' keypress"
    );
    let key = "\u{1b}[17~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(6))),
        "Can parse a bare 'F6' keypress"
    );
    let key = "\u{1b}[18~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(7))),
        "Can parse a bare 'F7' keypress"
    );
    let key = "\u{1b}[19~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(8))),
        "Can parse a bare 'F8' keypress"
    );
    let key = "\u{1b}[20~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(9))),
        "Can parse a bare 'F9' keypress"
    );
    let key = "\u{1b}[21~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(10))),
        "Can parse a bare 'F10' keypress"
    );
    let key = "\u{1b}[23~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(11))),
        "Can parse a bare 'F11' keypress"
    );
    let key = "\u{1b}[24~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(12))),
        "Can parse a bare 'F12' keypress"
    );
    let key = "\u{1b}[D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Left)),
        "Can parse a bare 'Left' keypress"
    );
    let key = "\u{1b}[C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Right)),
        "Can parse a bare 'Right' keypress"
    );
    let key = "\u{1b}[A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Up)),
        "Can parse a bare 'Up' keypress"
    );
    let key = "\u{1b}[B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Down)),
        "Can parse a bare 'Down' keypress"
    );
    let key = "\u{1b}[H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home)),
        "Can parse a bare 'Home' keypress"
    );
    let key = "\u{1b}[F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End)),
        "Can parse a bare 'End' keypress"
    );
    let key = "\u{1b}[P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1))),
        "Can parse a bare 'F1 (alternate)' keypress"
    );
    let key = "\u{1b}[Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2))),
        "Can parse a bare 'F2 (alternate)' keypress"
    );
    let key = "\u{1b}[S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4))),
        "Can parse a bare 'F4 (alternate)' keypress"
    );
    let key = "\u{1b}[1087u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('п'))),
        "Can parse a bare 'п' keypress"
    );
    let key = "\u{1b}[1255u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ӧ'))),
        "Can parse a bare 'ӧ' keypress"
    );
    let key = "\u{1b}[1098u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ъ'))),
        "Can parse a bare 'ъ' keypress"
    );
}

#[test]
pub fn can_parse_keys_with_shift_modifier() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('a')).with_shift_modifier()),
        "Can parse a bare 'a' keypress with shift"
    );
    let key = "\u{1b}[49;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('1')).with_shift_modifier()),
        "Can parse a bare '1' keypress with shift"
    );
    let key = "\u{1b}[27;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Esc).with_shift_modifier()),
        "Can parse a bare 'ESC' keypress with shift"
    );
    let key = "\u{1b}[13;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Enter).with_shift_modifier()),
        "Can parse a bare 'ENTER' keypress with shift"
    );
    let key = "\u{1b}[9;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Tab).with_shift_modifier()),
        "Can parse a bare 'Tab' keypress with shift"
    );
    let key = "\u{1b}[127;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Backspace).with_shift_modifier()),
        "Can parse a bare 'Backspace' keypress with shift"
    );
    let key = "\u{1b}[57358;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::CapsLock).with_shift_modifier()),
        "Can parse a bare 'CapsLock' keypress with shift"
    );
    let key = "\u{1b}[57359;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::ScrollLock).with_shift_modifier()),
        "Can parse a bare 'ScrollLock' keypress with shift"
    );
    let key = "\u{1b}[57360;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::NumLock).with_shift_modifier()),
        "Can parse a bare 'NumLock' keypress with shift"
    );
    let key = "\u{1b}[57361;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PrintScreen).with_shift_modifier()),
        "Can parse a bare 'PrintScreen' keypress with shift"
    );
    let key = "\u{1b}[57362;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Pause).with_shift_modifier()),
        "Can parse a bare 'Pause' keypress with shift"
    );
    let key = "\u{1b}[57363;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Menu).with_shift_modifier()),
        "Can parse a bare 'Menu' keypress with shift"
    );

    let key = "\u{1b}[2;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Insert).with_shift_modifier()),
        "Can parse a bare 'Insert' keypress with shift"
    );
    let key = "\u{1b}[3;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Delete).with_shift_modifier()),
        "Can parse a bare 'Delete' keypress with shift"
    );
    let key = "\u{1b}[5;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageUp).with_shift_modifier()),
        "Can parse a bare 'PageUp' keypress with shift"
    );
    let key = "\u{1b}[6;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageDown).with_shift_modifier()),
        "Can parse a bare 'PageDown' keypress with shift"
    );
    let key = "\u{1b}[7;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_shift_modifier()),
        "Can parse a bare 'Home' keypress with shift"
    );
    let key = "\u{1b}[8;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_shift_modifier()),
        "Can parse a bare 'End' keypress with shift"
    );
    let key = "\u{1b}[11;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_shift_modifier()),
        "Can parse a bare 'F1' keypress with shift"
    );
    let key = "\u{1b}[12;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_shift_modifier()),
        "Can parse a bare 'F2' keypress with shift"
    );
    let key = "\u{1b}[13;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(3)).with_shift_modifier()),
        "Can parse a bare 'F3' keypress with shift"
    );
    let key = "\u{1b}[14;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_shift_modifier()),
        "Can parse a bare 'F4' keypress with shift"
    );
    let key = "\u{1b}[15;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(5)).with_shift_modifier()),
        "Can parse a bare 'F5' keypress with shift"
    );
    let key = "\u{1b}[17;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(6)).with_shift_modifier()),
        "Can parse a bare 'F6' keypress with shift"
    );
    let key = "\u{1b}[18;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(7)).with_shift_modifier()),
        "Can parse a bare 'F7' keypress with shift"
    );
    let key = "\u{1b}[19;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(8)).with_shift_modifier()),
        "Can parse a bare 'F8' keypress with shift"
    );
    let key = "\u{1b}[20;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(9)).with_shift_modifier()),
        "Can parse a bare 'F9' keypress with shift"
    );
    let key = "\u{1b}[21;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(10)).with_shift_modifier()),
        "Can parse a bare 'F10' keypress with shift"
    );
    let key = "\u{1b}[23;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(11)).with_shift_modifier()),
        "Can parse a bare 'F11' keypress with shift"
    );
    let key = "\u{1b}[24;2~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(12)).with_shift_modifier()),
        "Can parse a bare 'F12' keypress with shift"
    );
    let key = "\u{1b}[1;2D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Left).with_shift_modifier()),
        "Can parse a bare 'Left' keypress with shift"
    );
    let key = "\u{1b}[1;2C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Right).with_shift_modifier()),
        "Can parse a bare 'Right' keypress with shift"
    );
    let key = "\u{1b}[1;2A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Up).with_shift_modifier()),
        "Can parse a bare 'Up' keypress with shift"
    );
    let key = "\u{1b}[1;2B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Down).with_shift_modifier()),
        "Can parse a bare 'Down' keypress with shift"
    );
    let key = "\u{1b}[1;2H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_shift_modifier()),
        "Can parse a bare 'Home' keypress with shift"
    );
    let key = "\u{1b}[1;2F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_shift_modifier()),
        "Can parse a bare 'End' keypress with shift"
    );
    let key = "\u{1b}[1;2P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_shift_modifier()),
        "Can parse a bare 'F1 (alternate)' keypress with shift"
    );
    let key = "\u{1b}[1;2Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_shift_modifier()),
        "Can parse a bare 'F2 (alternate)' keypress with shift"
    );
    let key = "\u{1b}[1;2S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_shift_modifier()),
        "Can parse a bare 'F4 (alternate)' keypress with shift"
    );
    let key = "\u{1b}[1087;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('п')).with_shift_modifier()),
        "Can parse a bare 'п' keypress with shift"
    );
    let key = "\u{1b}[1255;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ӧ')).with_shift_modifier()),
        "Can parse a bare 'ӧ' keypress with shift"
    );
    let key = "\u{1b}[1098;2u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ъ')).with_shift_modifier()),
        "Can parse a bare 'ъ' keypress with shift"
    );
}

#[test]
pub fn can_parse_keys_with_alt_modifier() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('a')).with_alt_modifier()),
        "Can parse a bare 'a' keypress with alt"
    );
    let key = "\u{1b}[49;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('1')).with_alt_modifier()),
        "Can parse a bare '1' keypress with alt"
    );
    let key = "\u{1b}[27;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Esc).with_alt_modifier()),
        "Can parse a bare 'ESC' keypress with alt"
    );
    let key = "\u{1b}[13;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Enter).with_alt_modifier()),
        "Can parse a bare 'ENTER' keypress with alt"
    );
    let key = "\u{1b}[9;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Tab).with_alt_modifier()),
        "Can parse a bare 'Tab' keypress with alt"
    );
    let key = "\u{1b}[127;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Backspace).with_alt_modifier()),
        "Can parse a bare 'Backspace' keypress with alt"
    );
    let key = "\u{1b}[57358;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::CapsLock).with_alt_modifier()),
        "Can parse a bare 'CapsLock' keypress with alt"
    );
    let key = "\u{1b}[57359;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::ScrollLock).with_alt_modifier()),
        "Can parse a bare 'ScrollLock' keypress with alt"
    );
    let key = "\u{1b}[57360;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::NumLock).with_alt_modifier()),
        "Can parse a bare 'NumLock' keypress with alt"
    );
    let key = "\u{1b}[57361;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PrintScreen).with_alt_modifier()),
        "Can parse a bare 'PrintScreen' keypress with alt"
    );
    let key = "\u{1b}[57362;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Pause).with_alt_modifier()),
        "Can parse a bare 'Pause' keypress with alt"
    );
    let key = "\u{1b}[57363;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Menu).with_alt_modifier()),
        "Can parse a bare 'Menu' keypress with alt"
    );

    let key = "\u{1b}[2;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Insert).with_alt_modifier()),
        "Can parse a bare 'Insert' keypress with alt"
    );
    let key = "\u{1b}[3;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Delete).with_alt_modifier()),
        "Can parse a bare 'Delete' keypress with alt"
    );
    let key = "\u{1b}[5;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageUp).with_alt_modifier()),
        "Can parse a bare 'PageUp' keypress with alt"
    );
    let key = "\u{1b}[6;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageDown).with_alt_modifier()),
        "Can parse a bare 'PageDown' keypress with alt"
    );
    let key = "\u{1b}[7;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_alt_modifier()),
        "Can parse a bare 'Home' keypress with alt"
    );
    let key = "\u{1b}[8;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_alt_modifier()),
        "Can parse a bare 'End' keypress with alt"
    );
    let key = "\u{1b}[11;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_alt_modifier()),
        "Can parse a bare 'F1' keypress with alt"
    );
    let key = "\u{1b}[12;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_alt_modifier()),
        "Can parse a bare 'F2' keypress with alt"
    );
    let key = "\u{1b}[13;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(3)).with_alt_modifier()),
        "Can parse a bare 'F3' keypress with alt"
    );
    let key = "\u{1b}[14;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_alt_modifier()),
        "Can parse a bare 'F4' keypress with alt"
    );
    let key = "\u{1b}[15;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(5)).with_alt_modifier()),
        "Can parse a bare 'F5' keypress with alt"
    );
    let key = "\u{1b}[17;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(6)).with_alt_modifier()),
        "Can parse a bare 'F6' keypress with alt"
    );
    let key = "\u{1b}[18;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(7)).with_alt_modifier()),
        "Can parse a bare 'F7' keypress with alt"
    );
    let key = "\u{1b}[19;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(8)).with_alt_modifier()),
        "Can parse a bare 'F8' keypress with alt"
    );
    let key = "\u{1b}[20;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(9)).with_alt_modifier()),
        "Can parse a bare 'F9' keypress with alt"
    );
    let key = "\u{1b}[21;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(10)).with_alt_modifier()),
        "Can parse a bare 'F10' keypress with alt"
    );
    let key = "\u{1b}[23;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(11)).with_alt_modifier()),
        "Can parse a bare 'F11' keypress with alt"
    );
    let key = "\u{1b}[24;3~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(12)).with_alt_modifier()),
        "Can parse a bare 'F12' keypress with alt"
    );
    let key = "\u{1b}[1;3D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Left).with_alt_modifier()),
        "Can parse a bare 'Left' keypress with alt"
    );
    let key = "\u{1b}[1;3C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Right).with_alt_modifier()),
        "Can parse a bare 'Right' keypress with alt"
    );
    let key = "\u{1b}[1;3A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Up).with_alt_modifier()),
        "Can parse a bare 'Up' keypress with alt"
    );
    let key = "\u{1b}[1;3B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Down).with_alt_modifier()),
        "Can parse a bare 'Down' keypress with alt"
    );
    let key = "\u{1b}[1;3H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_alt_modifier()),
        "Can parse a bare 'Home' keypress with alt"
    );
    let key = "\u{1b}[1;3F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_alt_modifier()),
        "Can parse a bare 'End' keypress with alt"
    );
    let key = "\u{1b}[1;3P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_alt_modifier()),
        "Can parse a bare 'F1 (alternate)' keypress with alt"
    );
    let key = "\u{1b}[1;3Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_alt_modifier()),
        "Can parse a bare 'F2 (alternate)' keypress with alt"
    );
    let key = "\u{1b}[1;3S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_alt_modifier()),
        "Can parse a bare 'F4 (alternate)' keypress with alt"
    );
    let key = "\u{1b}[1087;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('п')).with_alt_modifier()),
        "Can parse a bare 'п' keypress with alt"
    );
    let key = "\u{1b}[1255;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ӧ')).with_alt_modifier()),
        "Can parse a bare 'ӧ' keypress with alt"
    );
    let key = "\u{1b}[1098;3u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ъ')).with_alt_modifier()),
        "Can parse a bare 'ъ' keypress with alt"
    );
}

#[test]
pub fn can_parse_keys_with_ctrl_modifier() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('a')).with_ctrl_modifier()),
        "Can parse a bare 'a' keypress with ctrl"
    );
    let key = "\u{1b}[49;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('1')).with_ctrl_modifier()),
        "Can parse a bare '1' keypress with ctrl"
    );
    let key = "\u{1b}[27;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Esc).with_ctrl_modifier()),
        "Can parse a bare 'ESC' keypress with ctrl"
    );
    let key = "\u{1b}[13;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Enter).with_ctrl_modifier()),
        "Can parse a bare 'ENTER' keypress with ctrl"
    );
    let key = "\u{1b}[9;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Tab).with_ctrl_modifier()),
        "Can parse a bare 'Tab' keypress with ctrl"
    );
    let key = "\u{1b}[127;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Backspace).with_ctrl_modifier()),
        "Can parse a bare 'Backspace' keypress with ctrl"
    );
    let key = "\u{1b}[57358;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::CapsLock).with_ctrl_modifier()),
        "Can parse a bare 'CapsLock' keypress with ctrl"
    );
    let key = "\u{1b}[57359;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::ScrollLock).with_ctrl_modifier()),
        "Can parse a bare 'ScrollLock' keypress with ctrl"
    );
    let key = "\u{1b}[57360;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::NumLock).with_ctrl_modifier()),
        "Can parse a bare 'NumLock' keypress with ctrl"
    );
    let key = "\u{1b}[57361;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PrintScreen).with_ctrl_modifier()),
        "Can parse a bare 'PrintScreen' keypress with ctrl"
    );
    let key = "\u{1b}[57362;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Pause).with_ctrl_modifier()),
        "Can parse a bare 'Pause' keypress with ctrl"
    );
    let key = "\u{1b}[57363;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Menu).with_ctrl_modifier()),
        "Can parse a bare 'Menu' keypress with ctrl"
    );

    let key = "\u{1b}[2;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Insert).with_ctrl_modifier()),
        "Can parse a bare 'Insert' keypress with ctrl"
    );
    let key = "\u{1b}[3;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Delete).with_ctrl_modifier()),
        "Can parse a bare 'Delete' keypress with ctrl"
    );
    let key = "\u{1b}[5;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageUp).with_ctrl_modifier()),
        "Can parse a bare 'PageUp' keypress with ctrl"
    );
    let key = "\u{1b}[6;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageDown).with_ctrl_modifier()),
        "Can parse a bare 'PageDown' keypress with ctrl"
    );
    let key = "\u{1b}[7;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_ctrl_modifier()),
        "Can parse a bare 'Home' keypress with ctrl"
    );
    let key = "\u{1b}[8;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_ctrl_modifier()),
        "Can parse a bare 'End' keypress with ctrl"
    );
    let key = "\u{1b}[11;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_ctrl_modifier()),
        "Can parse a bare 'F1' keypress with ctrl"
    );
    let key = "\u{1b}[12;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_ctrl_modifier()),
        "Can parse a bare 'F2' keypress with ctrl"
    );
    let key = "\u{1b}[13;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(3)).with_ctrl_modifier()),
        "Can parse a bare 'F3' keypress with ctrl"
    );
    let key = "\u{1b}[14;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_ctrl_modifier()),
        "Can parse a bare 'F4' keypress with ctrl"
    );
    let key = "\u{1b}[15;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(5)).with_ctrl_modifier()),
        "Can parse a bare 'F5' keypress with ctrl"
    );
    let key = "\u{1b}[17;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(6)).with_ctrl_modifier()),
        "Can parse a bare 'F6' keypress with ctrl"
    );
    let key = "\u{1b}[18;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(7)).with_ctrl_modifier()),
        "Can parse a bare 'F7' keypress with ctrl"
    );
    let key = "\u{1b}[19;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(8)).with_ctrl_modifier()),
        "Can parse a bare 'F8' keypress with ctrl"
    );
    let key = "\u{1b}[20;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(9)).with_ctrl_modifier()),
        "Can parse a bare 'F9' keypress with ctrl"
    );
    let key = "\u{1b}[21;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(10)).with_ctrl_modifier()),
        "Can parse a bare 'F10' keypress with ctrl"
    );
    let key = "\u{1b}[23;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(11)).with_ctrl_modifier()),
        "Can parse a bare 'F11' keypress with ctrl"
    );
    let key = "\u{1b}[24;5~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(12)).with_ctrl_modifier()),
        "Can parse a bare 'F12' keypress with ctrl"
    );
    let key = "\u{1b}[1;5D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Left).with_ctrl_modifier()),
        "Can parse a bare 'Left' keypress with ctrl"
    );
    let key = "\u{1b}[1;5C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Right).with_ctrl_modifier()),
        "Can parse a bare 'Right' keypress with ctrl"
    );
    let key = "\u{1b}[1;5A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Up).with_ctrl_modifier()),
        "Can parse a bare 'Up' keypress with ctrl"
    );
    let key = "\u{1b}[1;5B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Down).with_ctrl_modifier()),
        "Can parse a bare 'Down' keypress with ctrl"
    );
    let key = "\u{1b}[1;5H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_ctrl_modifier()),
        "Can parse a bare 'Home' keypress with ctrl"
    );
    let key = "\u{1b}[1;5F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_ctrl_modifier()),
        "Can parse a bare 'End' keypress with ctrl"
    );
    let key = "\u{1b}[1;5P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_ctrl_modifier()),
        "Can parse a bare 'F1 (ctrlernate)' keypress with ctrl"
    );
    let key = "\u{1b}[1;5Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_ctrl_modifier()),
        "Can parse a bare 'F2 (ctrlernate)' keypress with ctrl"
    );
    let key = "\u{1b}[1;5S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_ctrl_modifier()),
        "Can parse a bare 'F4 (ctrlernate)' keypress with ctrl"
    );
    let key = "\u{1b}[1087;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('п')).with_ctrl_modifier()),
        "Can parse a bare 'п' keypress with ctrl"
    );
    let key = "\u{1b}[1255;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ӧ')).with_ctrl_modifier()),
        "Can parse a bare 'ӧ' keypress with ctrl"
    );
    let key = "\u{1b}[1098;5u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ъ')).with_ctrl_modifier()),
        "Can parse a bare 'ъ' keypress with ctrl"
    );
}

#[test]
pub fn can_parse_arrows_and_composer_key_with_super_modifier() {
    // The key-contract v3 wire format: the Alacritty preset translates
    // Cmd+arrows / Cmd+E into these exact sequences (kitty CSI-u, super
    // bit = 8 → modifier param 9). If this test breaks, the Cmd switcher
    // dies host-wide.
    use zellij_utils::data::BareKey;
    for (seq, bare, label) in [
        ("\u{1b}[1;9D", BareKey::Left, "Super+Left"),
        ("\u{1b}[1;9C", BareKey::Right, "Super+Right"),
        ("\u{1b}[1;9A", BareKey::Up, "Super+Up"),
        ("\u{1b}[1;9B", BareKey::Down, "Super+Down"),
        ("\u{1b}[101;9u", BareKey::Char('e'), "Super+e (Composer)"),
    ] {
        assert_eq!(
            parse_for_test(seq.as_bytes()),
            Some(KeyWithModifier::new(bare).with_super_modifier()),
            "Can parse {label} from the preset wire sequence"
        );
    }
}

#[test]
pub fn can_parse_keys_with_super_modifier() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('a')).with_super_modifier()),
        "Can parse a bare 'a' keypress with super"
    );
    let key = "\u{1b}[49;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('1')).with_super_modifier()),
        "Can parse a bare '1' keypress with super"
    );
    let key = "\u{1b}[27;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Esc).with_super_modifier()),
        "Can parse a bare 'ESC' keypress with super"
    );
    let key = "\u{1b}[13;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Enter).with_super_modifier()),
        "Can parse a bare 'ENTER' keypress with super"
    );
    let key = "\u{1b}[9;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Tab).with_super_modifier()),
        "Can parse a bare 'Tab' keypress with super"
    );
    let key = "\u{1b}[127;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Backspace).with_super_modifier()),
        "Can parse a bare 'Backspace' keypress with super"
    );
    let key = "\u{1b}[57358;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::CapsLock).with_super_modifier()),
        "Can parse a bare 'CapsLock' keypress with super"
    );
    let key = "\u{1b}[57359;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::ScrollLock).with_super_modifier()),
        "Can parse a bare 'ScrollLock' keypress with super"
    );
    let key = "\u{1b}[57360;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::NumLock).with_super_modifier()),
        "Can parse a bare 'NumLock' keypress with super"
    );
    let key = "\u{1b}[57361;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PrintScreen).with_super_modifier()),
        "Can parse a bare 'PrintScreen' keypress with super"
    );
    let key = "\u{1b}[57362;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Pause).with_super_modifier()),
        "Can parse a bare 'Pause' keypress with super"
    );
    let key = "\u{1b}[57363;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Menu).with_super_modifier()),
        "Can parse a bare 'Menu' keypress with super"
    );

    let key = "\u{1b}[2;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Insert).with_super_modifier()),
        "Can parse a bare 'Insert' keypress with super"
    );
    let key = "\u{1b}[3;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Delete).with_super_modifier()),
        "Can parse a bare 'Delete' keypress with super"
    );
    let key = "\u{1b}[5;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageUp).with_super_modifier()),
        "Can parse a bare 'PageUp' keypress with super"
    );
    let key = "\u{1b}[6;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::PageDown).with_super_modifier()),
        "Can parse a bare 'PageDown' keypress with super"
    );
    let key = "\u{1b}[7;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_super_modifier()),
        "Can parse a bare 'Home' keypress with super"
    );
    let key = "\u{1b}[8;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_super_modifier()),
        "Can parse a bare 'End' keypress with super"
    );
    let key = "\u{1b}[11;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_super_modifier()),
        "Can parse a bare 'F1' keypress with super"
    );
    let key = "\u{1b}[12;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_super_modifier()),
        "Can parse a bare 'F2' keypress with super"
    );
    let key = "\u{1b}[13;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(3)).with_super_modifier()),
        "Can parse a bare 'F3' keypress with super"
    );
    let key = "\u{1b}[14;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_super_modifier()),
        "Can parse a bare 'F4' keypress with super"
    );
    let key = "\u{1b}[15;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(5)).with_super_modifier()),
        "Can parse a bare 'F5' keypress with super"
    );
    let key = "\u{1b}[17;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(6)).with_super_modifier()),
        "Can parse a bare 'F6' keypress with super"
    );
    let key = "\u{1b}[18;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(7)).with_super_modifier()),
        "Can parse a bare 'F7' keypress with super"
    );
    let key = "\u{1b}[19;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(8)).with_super_modifier()),
        "Can parse a bare 'F8' keypress with super"
    );
    let key = "\u{1b}[20;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(9)).with_super_modifier()),
        "Can parse a bare 'F9' keypress with super"
    );
    let key = "\u{1b}[21;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(10)).with_super_modifier()),
        "Can parse a bare 'F10' keypress with super"
    );
    let key = "\u{1b}[23;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(11)).with_super_modifier()),
        "Can parse a bare 'F11' keypress with super"
    );
    let key = "\u{1b}[24;9~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(12)).with_super_modifier()),
        "Can parse a bare 'F12' keypress with super"
    );
    let key = "\u{1b}[1;9D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Left).with_super_modifier()),
        "Can parse a bare 'Left' keypress with super"
    );
    let key = "\u{1b}[1;9C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Right).with_super_modifier()),
        "Can parse a bare 'Right' keypress with super"
    );
    let key = "\u{1b}[1;9A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Up).with_super_modifier()),
        "Can parse a bare 'Up' keypress with super"
    );
    let key = "\u{1b}[1;9B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Down).with_super_modifier()),
        "Can parse a bare 'Down' keypress with super"
    );
    let key = "\u{1b}[1;9H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Home).with_super_modifier()),
        "Can parse a bare 'Home' keypress with super"
    );
    let key = "\u{1b}[1;9F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::End).with_super_modifier()),
        "Can parse a bare 'End' keypress with super"
    );
    let key = "\u{1b}[1;9P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(1)).with_super_modifier()),
        "Can parse a bare 'F1 (alternate)' keypress with super"
    );
    let key = "\u{1b}[1;9Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(2)).with_super_modifier()),
        "Can parse a bare 'F2 (alternate)' keypress with super"
    );
    let key = "\u{1b}[1;9S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::F(4)).with_super_modifier()),
        "Can parse a bare 'F4 (alternate)' keypress with super"
    );
    let key = "\u{1b}[1087;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('п')).with_super_modifier()),
        "Can parse a bare 'п' keypress with super"
    );
    let key = "\u{1b}[1255;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ӧ')).with_super_modifier()),
        "Can parse a bare 'ӧ' keypress with super"
    );
    let key = "\u{1b}[1098;9u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(KeyWithModifier::new(BareKey::Char('ъ')).with_super_modifier()),
        "Can parse a bare 'ъ' keypress with super"
    );
}

#[test]
pub fn can_parse_keys_with_multiple_modifiers() {
    use zellij_utils::data::BareKey;
    let key = "\u{1b}[97;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Char('a'))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'a' keypress with all modifiers"
    );
    let key = "\u{1b}[49;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Char('1'))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare '1' keypress with all modifiers"
    );
    let key = "\u{1b}[27;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Esc)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'ESC' keypress with all modifiers"
    );
    let key = "\u{1b}[13;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Enter)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'ENTER' keypress with all modifiers"
    );
    let key = "\u{1b}[9;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Tab)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Tab' keypress with all modifiers"
    );
    let key = "\u{1b}[127;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Backspace)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Backspace' keypress with all modifiers"
    );
    let key = "\u{1b}[57358;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::CapsLock)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'CapsLock' keypress with all modifiers"
    );
    let key = "\u{1b}[57359;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::ScrollLock)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'ScrollLock' keypress with all modifiers"
    );
    let key = "\u{1b}[57360;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::NumLock)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'NumLock' keypress with all modifiers"
    );
    let key = "\u{1b}[57361;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::PrintScreen)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'PrintScreen' keypress with all modifiers"
    );
    let key = "\u{1b}[57362;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Pause)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Pause' keypress with all modifiers"
    );
    let key = "\u{1b}[57363;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Menu)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Menu' keypress with all modifiers"
    );

    let key = "\u{1b}[2;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Insert)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Insert' keypress with all modifiers"
    );
    let key = "\u{1b}[3;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Delete)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Delete' keypress with all modifiers"
    );
    let key = "\u{1b}[5;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::PageUp)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'PageUp' keypress with all modifiers"
    );
    let key = "\u{1b}[6;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::PageDown)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'PageDown' keypress with all modifiers"
    );
    let key = "\u{1b}[7;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Home)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Home' keypress with all modifiers"
    );
    let key = "\u{1b}[8;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::End)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'End' keypress with all modifiers"
    );
    let key = "\u{1b}[11;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(1))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F1' keypress with all modifiers"
    );
    let key = "\u{1b}[12;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(2))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F2' keypress with all modifiers"
    );
    let key = "\u{1b}[13;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(3))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F3' keypress with all modifiers"
    );
    let key = "\u{1b}[14;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(4))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F4' keypress with all modifiers"
    );
    let key = "\u{1b}[15;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(5))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F5' keypress with all modifiers"
    );
    let key = "\u{1b}[17;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(6))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F6' keypress with all modifiers"
    );
    let key = "\u{1b}[18;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(7))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F7' keypress with all modifiers"
    );
    let key = "\u{1b}[19;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(8))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F8' keypress with all modifiers"
    );
    let key = "\u{1b}[20;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(9))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F9' keypress with all modifiers"
    );
    let key = "\u{1b}[21;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(10))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F10' keypress with all modifiers"
    );
    let key = "\u{1b}[23;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(11))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F11' keypress with all modifiers"
    );
    let key = "\u{1b}[24;16~";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(12))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F12' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16D";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Left)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Left' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16C";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Right)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Right' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16A";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Up)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Up' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16B";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Down)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Down' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16H";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Home)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'Home' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16F";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::End)
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'End' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16P";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(1))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F1 (superernate)' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16Q";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(2))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F2 (superernate)' keypress with all modifiers"
    );
    let key = "\u{1b}[1;16S";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::F(4))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'F4 (superernate)' keypress with all modifiers"
    );
    let key = "\u{1b}[1087;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Char('п'))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'п' keypress with all modifiers"
    );
    let key = "\u{1b}[1255;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Char('ӧ'))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'ӧ' keypress with all modifiers"
    );
    let key = "\u{1b}[1098;16u";
    assert_eq!(
        parse_for_test(key.as_bytes()),
        Some(
            KeyWithModifier::new(BareKey::Char('ъ'))
                .with_super_modifier()
                .with_ctrl_modifier()
                .with_alt_modifier()
                .with_shift_modifier()
        ),
        "Can parse a bare 'ъ' keypress with all modifiers"
    );
}

// =====================================================================
// Cross-chunk fragmentation tests for the long-lived feed() entry
// point. Under SSH or any kernel-boundary-fragmented stdin read, a
// single Kitty CSI sequence routinely arrives split across multiple
// chunks; feed() must keep state across calls so the sequence still
// resolves on a follow-up chunk instead of degrading to legacy CSI
// form (and losing modifier metadata).
// =====================================================================

#[test]
fn fragmented_kitty_csi_u_emits_one_event() {
    use zellij_utils::data::BareKey;
    let mut p = KittyKeyboardParser::new();
    let r1 = p.feed(b"\x1b[97;");
    assert!(r1.keys.is_empty());
    assert!(matches!(r1.rest, KittyFeedRest::Incomplete));
    let r2 = p.feed(b"2u");
    assert_eq!(r2.keys.len(), 1);
    assert_eq!(
        r2.keys[0].0,
        KeyWithModifier::new(BareKey::Char('a')).with_shift_modifier()
    );
    // The key's raw bytes span both chunks.
    assert_eq!(r2.keys[0].1, b"\x1b[97;2u");
    assert!(matches!(r2.rest, KittyFeedRest::Consumed));
}

#[test]
fn fragmented_kitty_byte_by_byte() {
    use zellij_utils::data::BareKey;
    let full = b"\x1b[97;5u"; // ctrl+a
    let mut p = KittyKeyboardParser::new();
    for &b in &full[..full.len() - 1] {
        let r = p.feed(&[b]);
        assert!(
            r.keys.is_empty() && matches!(r.rest, KittyFeedRest::Incomplete),
            "byte 0x{:02x} should be Incomplete",
            b
        );
    }
    let r = p.feed(&[full[full.len() - 1]]);
    assert_eq!(r.keys.len(), 1);
    assert_eq!(
        r.keys[0].0,
        KeyWithModifier::new(BareKey::Char('a')).with_ctrl_modifier()
    );
}

#[test]
fn non_kitty_bytes_yield_nomatch_and_reset() {
    // Plain printable bytes don't form a Kitty sequence — must report
    // Passthrough (not Incomplete) so the caller falls through to
    // termwiz immediately rather than buffering forever.
    let mut p = KittyKeyboardParser::new();
    let r = p.feed(b"hello");
    assert!(r.keys.is_empty());
    assert_eq!(r.consumed_up_to, 0);
    assert!(matches!(r.rest, KittyFeedRest::Passthrough));
}

// =====================================================================
// The mirror image of fragmentation: coalescence. The same SSH and PTY
// buffering that splits one sequence across chunks also *merges* several
// keypresses into a single read when typing outruns the link. feed()
// must surface the keys it already resolved instead of discarding the
// whole chunk.
// =====================================================================

#[test]
fn coalesced_kitty_sequences_in_one_chunk() {
    use zellij_utils::data::BareKey;
    // Backspace then Space, typed fast enough that both land in one read.
    let mut p = KittyKeyboardParser::new();
    let r = p.feed(b"\x1b[127u\x1b[32u");
    assert_eq!(r.keys.len(), 2, "both keypresses must survive");
    assert_eq!(r.keys[0].0, KeyWithModifier::new(BareKey::Backspace));
    assert_eq!(r.keys[0].1, b"\x1b[127u");
    assert_eq!(r.keys[1].0, KeyWithModifier::new(BareKey::Char(' ')));
    assert_eq!(r.keys[1].1, b"\x1b[32u");
    assert!(matches!(r.rest, KittyFeedRest::Consumed));
}

#[test]
fn coalesced_arrow_sequences_in_one_chunk() {
    use zellij_utils::data::BareKey;
    // Arrow keys use letter-terminated CSI, the other completion path.
    let mut p = KittyKeyboardParser::new();
    let r = p.feed(b"\x1b[A\x1b[B");
    assert_eq!(r.keys.len(), 2);
    assert_eq!(r.keys[0].0, KeyWithModifier::new(BareKey::Up));
    assert_eq!(r.keys[1].0, KeyWithModifier::new(BareKey::Down));
    assert!(matches!(r.rest, KittyFeedRest::Consumed));
}

#[test]
fn kitty_sequence_followed_by_plain_byte() {
    use zellij_utils::data::BareKey;
    // A resolved sequence must not be voided by whatever trails it; the
    // trailing byte is left for the fallback parser.
    let mut p = KittyKeyboardParser::new();
    let r = p.feed(b"\x1b[127ux");
    assert_eq!(r.keys.len(), 1);
    assert_eq!(r.keys[0].0, KeyWithModifier::new(BareKey::Backspace));
    assert_eq!(r.consumed_up_to, 6, "only the sequence bytes are consumed");
    assert!(matches!(r.rest, KittyFeedRest::Passthrough));
}

#[test]
fn lone_bracket_is_not_a_kitty_prefix() {
    // A bare '[' is a printable character, not the start of a CSI
    // sequence — only ESC is. Treating it as a prefix strands the parser
    // in a non-Ground state, where it swallows the *next* keypress.
    let mut p = KittyKeyboardParser::new();
    let r = p.feed(b"[");
    assert!(
        r.keys.is_empty() && matches!(r.rest, KittyFeedRest::Passthrough),
        "a lone '[' must not be treated as a Kitty prefix"
    );
}

#[test]
fn stranded_state_does_not_swallow_the_next_key() {
    use zellij_utils::data::BareKey;
    // Whatever the previous chunk was, a complete Backspace sequence in
    // the next chunk must still resolve to Backspace.
    let mut p = KittyKeyboardParser::new();
    let _ = p.feed(b"[");
    let r = p.feed(b"\x1b[127u");
    assert_eq!(r.keys.len(), 1);
    assert_eq!(r.keys[0].0, KeyWithModifier::new(BareKey::Backspace));
}
