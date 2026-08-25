//! Terminal-control normalization for safely rendering untrusted child output.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum TerminalState {
    #[default]
    Text,
    Utf8C2,
    Escape,
    ControlSequence,
    ControlString,
    ControlStringEscape,
    ControlStringC2,
}

#[derive(Debug, Default)]
pub(crate) struct TerminalOutputNormalizer {
    state: TerminalState,
    pending_carriage_return: bool,
    pending_utf8: Vec<u8>,
}

impl TerminalOutputNormalizer {
    pub(crate) fn push(&mut self, input: &[u8], output: &mut Vec<u8>) {
        for &byte in input {
            if self.state == TerminalState::Text && !self.pending_utf8.is_empty() {
                self.push_utf8(byte, output);
                continue;
            }
            if self.state == TerminalState::Text && self.pending_carriage_return {
                self.pending_carriage_return = false;
                output.push(b'\n');
                if byte == b'\n' {
                    continue;
                }
            }
            match self.state {
                TerminalState::Text => self.push_text(byte, output),
                TerminalState::Utf8C2 => {
                    if (0x80..=0x9f).contains(&byte) {
                        self.push_c1(byte, output);
                    } else {
                        output.push(0xc2);
                        self.state = TerminalState::Text;
                        self.push_text(byte, output);
                    }
                }
                TerminalState::Escape => match byte {
                    b'[' => self.state = TerminalState::ControlSequence,
                    b']' | b'P' | b'X' | b'^' | b'_' => {
                        self.state = TerminalState::ControlString;
                    }
                    b'\n' => {
                        self.state = TerminalState::Text;
                        output.push(b'\n');
                    }
                    b'\r' => {
                        self.state = TerminalState::Text;
                        self.pending_carriage_return = true;
                    }
                    0x30..=0x7e => self.state = TerminalState::Text,
                    _ => {}
                },
                TerminalState::ControlSequence => match byte {
                    0x1b => self.state = TerminalState::Escape,
                    b'\n' => {
                        self.state = TerminalState::Text;
                        output.push(b'\n');
                    }
                    b'\r' => {
                        self.state = TerminalState::Text;
                        self.pending_carriage_return = true;
                    }
                    0x40..=0x7e => self.state = TerminalState::Text,
                    _ => {}
                },
                TerminalState::ControlString => match byte {
                    0x07 | 0x9c => self.state = TerminalState::Text,
                    0x1b => self.state = TerminalState::ControlStringEscape,
                    0xc2 => self.state = TerminalState::ControlStringC2,
                    _ => {}
                },
                TerminalState::ControlStringEscape => {
                    self.state = if byte == b'\\' {
                        TerminalState::Text
                    } else if byte == 0x1b {
                        TerminalState::ControlStringEscape
                    } else {
                        TerminalState::ControlString
                    };
                }
                TerminalState::ControlStringC2 => {
                    self.state = if byte == 0x9c {
                        TerminalState::Text
                    } else {
                        TerminalState::ControlString
                    };
                }
            }
        }
    }

    fn push_text(&mut self, byte: u8, output: &mut Vec<u8>) {
        match byte {
            b'\r' => self.pending_carriage_return = true,
            b'\n' | b'\t' => output.push(byte),
            0x1b => self.state = TerminalState::Escape,
            0xc2 => self.state = TerminalState::Utf8C2,
            0xc3..=0xf4 => self.pending_utf8.push(byte),
            0x00..=0x1f | 0x7f => {}
            0x80..=0x9f => self.push_c1(byte, output),
            _ => output.push(byte),
        }
    }

    fn push_utf8(&mut self, byte: u8, output: &mut Vec<u8>) {
        self.pending_utf8.push(byte);
        let width = utf8_character_width(self.pending_utf8[0]);
        let complete = self.pending_utf8.len() == width;
        if !complete && (0x80..=0xbf).contains(&byte) {
            return;
        }

        let pending = std::mem::take(&mut self.pending_utf8);
        if complete && std::str::from_utf8(&pending).is_ok() {
            output.extend_from_slice(&pending);
            return;
        }

        output.push(pending[0]);
        self.push(&pending[1..], output);
    }

    fn push_c1(&mut self, byte: u8, output: &mut Vec<u8>) {
        match byte {
            0x85 => {
                self.state = TerminalState::Text;
                output.push(b'\n');
            }
            0x90 | 0x98 | 0x9d..=0x9f => self.state = TerminalState::ControlString,
            0x9b => self.state = TerminalState::ControlSequence,
            _ => self.state = TerminalState::Text,
        }
    }

    pub(crate) fn finish(&mut self, output: &mut Vec<u8>) {
        while !self.pending_utf8.is_empty() {
            let pending = std::mem::take(&mut self.pending_utf8);
            output.push(pending[0]);
            self.push(&pending[1..], output);
        }
        if self.pending_carriage_return {
            output.push(b'\n');
        } else if self.state == TerminalState::Utf8C2 {
            output.push(0xc2);
        }
        self.pending_carriage_return = false;
        self.state = TerminalState::Text;
    }
}

#[derive(Debug, Default)]
pub(crate) struct UnicodeFormatFilter {
    pending: Vec<u8>,
}

impl UnicodeFormatFilter {
    pub(crate) fn push(&mut self, input: &[u8], output: &mut Vec<u8>) {
        self.pending.extend_from_slice(input);
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let width = utf8_character_width(self.pending[consumed]);
            if self.pending.len() - consumed < width {
                break;
            }
            let end = consumed + width;
            let character = std::str::from_utf8(&self.pending[consumed..end])
                .ok()
                .and_then(|value| value.chars().next());
            if character.is_none_or(is_visible_output_character) {
                output.extend_from_slice(&self.pending[consumed..end]);
            }
            consumed = end;
        }
        self.pending.drain(..consumed);
    }

    pub(crate) fn finish(&mut self, output: &mut Vec<u8>) {
        output.append(&mut self.pending);
    }
}

const fn utf8_character_width(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

fn is_visible_output_character(character: char) -> bool {
    !matches!(
        character,
        '\u{00ad}'
            | '\u{034f}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{08e2}'
            | '\u{180e}'
            | '\u{3164}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{110bd}'
            | '\u{110cd}'
            | '\u{0600}'..='\u{0605}'
            | '\u{0890}'..='\u{0891}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{fff0}'..='\u{fffb}'
            | '\u{13430}'..='\u{1345f}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// Removes ANSI/C1 terminal controls and invisible Unicode format characters from raw output.
///
/// Carriage returns and terminal newline controls normalize to `\n`; printable UTF-8 and invalid
/// bytes are retained so subsequent byte-oriented redaction can still match the original stream.
#[must_use]
pub fn normalize_terminal_output(input: &[u8]) -> Vec<u8> {
    let mut terminal_normalizer = TerminalOutputNormalizer::default();
    let mut terminal_safe = Vec::with_capacity(input.len());
    terminal_normalizer.push(input, &mut terminal_safe);
    terminal_normalizer.finish(&mut terminal_safe);
    let mut format_filter = UnicodeFormatFilter::default();
    let mut output = Vec::with_capacity(terminal_safe.len());
    format_filter.push(&terminal_safe, &mut output);
    format_filter.finish(&mut output);
    output
}

#[cfg(test)]
#[path = "terminal_test.rs"]
mod terminal_test;
