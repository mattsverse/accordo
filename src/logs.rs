//! Incremental, bounded decoding of child output. Escape sequences may cross reads.

pub const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Default)]
enum Escape {
    #[default]
    None,
    Start,
    Csi,
    String,
    StringTerminator,
    Intermediate,
}

#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
    escape: Escape,
    previous_cr: bool,
}

impl Decoder {
    pub fn push(&mut self, input: &[u8]) -> Vec<String> {
        let mut records = Vec::new();
        for &byte in input {
            if !self.accept(byte) {
                continue;
            }
            if byte == b'\n' && self.previous_cr {
                self.previous_cr = false;
                continue;
            }
            self.previous_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                records.push(self.take_record());
            } else if byte == b'\t' || byte >= 0x20 && byte != 0x7f {
                self.bytes.push(byte);
                if self.bytes.len() >= MAX_RECORD_BYTES {
                    // Keep an incomplete UTF-8 suffix for the next record.
                    let split = match std::str::from_utf8(&self.bytes) {
                        Err(error) if error.error_len().is_none() => error.valid_up_to(),
                        _ => self.bytes.len(),
                    };
                    let suffix = self.bytes.split_off(split);
                    records.push(self.take_record());
                    self.bytes = suffix;
                }
            }
        }
        records
    }

    pub fn finish(&mut self) -> Option<String> {
        (!self.bytes.is_empty()).then(|| self.take_record())
    }

    fn take_record(&mut self) -> String {
        // Unicode C1 controls also must not reach the parent terminal.
        let text = String::from_utf8_lossy(&self.bytes)
            .chars()
            .filter(|character| !character.is_control() || *character == '\t')
            .collect();
        self.bytes.clear();
        text
    }

    fn accept(&mut self, byte: u8) -> bool {
        match self.escape {
            Escape::None => {
                if byte == 0x1b {
                    self.escape = Escape::Start;
                    false
                } else {
                    true
                }
            }
            Escape::Start => {
                self.escape = match byte {
                    b'[' => Escape::Csi,
                    b']' | b'P' | b'^' | b'_' | b'X' => Escape::String,
                    0x20..=0x2f => Escape::Intermediate,
                    _ => Escape::None,
                };
                false
            }
            Escape::Csi => {
                if (0x40..=0x7e).contains(&byte) || byte == b'\n' {
                    self.escape = Escape::None;
                }
                false
            }
            Escape::Intermediate => {
                if (0x30..=0x7e).contains(&byte) || byte == b'\n' {
                    self.escape = Escape::None;
                }
                false
            }
            Escape::String => {
                if byte == 0x07 {
                    self.escape = Escape::None;
                } else if byte == 0x1b {
                    self.escape = Escape::StringTerminator;
                }
                false
            }
            Escape::StringTerminator => {
                self.escape = if byte == b'\\' || byte == 0x07 {
                    Escape::None
                } else {
                    Escape::String
                };
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_chunks_controls_and_final_fragment() {
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"\x1b[3").is_empty());
        assert_eq!(decoder.push(b"1mhello\x1b[0m\r\nnext\r"), ["hello", "next"]);
        assert!(decoder.push(b"\x1b]0;title\x1b").is_empty());
        assert!(decoder.push(b"\\\xff\xe2").is_empty());
        assert!(decoder.push(b"\x82\xac").is_empty());
        assert_eq!(decoder.finish().unwrap(), "�€");
        assert!(decoder.finish().is_none());
    }

    #[test]
    fn caps_unterminated_records_without_splitting_utf8() {
        let mut decoder = Decoder::default();
        let input = format!("{}€tail", "a".repeat(MAX_RECORD_BYTES - 1));
        let records = decoder.push(input.as_bytes());
        assert_eq!(records[0].len(), MAX_RECORD_BYTES - 1);
        assert_eq!(decoder.finish().unwrap(), "€tail");
    }

    #[test]
    fn strips_character_set_selection_and_hyperlinks() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder.push(b"\x1b(0text\x1b(B \x1b]8;;https://example.com\x07link\x1b]8;;\x07\n"),
            ["text link"]
        );
    }
}
