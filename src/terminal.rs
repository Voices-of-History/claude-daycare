//! A small terminal emulator for reading Claude Code's interactive screen.
//!
//! Claude's TUI redraws only the cells that changed, with relative cursor
//! moves and line erases. A byte capture of the session therefore does not
//! read top to bottom: a refreshed "74%" may arrive as a lone "4" written over
//! the "3" of an earlier "73%". Replaying the capture onto a grid gives back
//! what a person would see, which is what the usage meter needs to read.
//!
//! Only what Claude Code emits is modelled: printable text, CR/LF/BS/TAB,
//! cursor movement, line and screen erases, save/restore, and the alternate
//! screen. Everything else (colours, modes, keyboard protocols, titles) is
//! skipped. There is no auto-wrap and no scroll-off: Claude lays out its own
//! lines, and a grid that only grows keeps every row a relative move can
//! reach.

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Screen {
    rows: Vec<Vec<char>>,
}

#[derive(Default)]
struct Cursor {
    row: usize,
    col: usize,
}

impl Screen {
    /// Replay a whole capture from its first byte.
    pub fn render(input: &[u8]) -> Screen {
        let mut terminal = Terminal::default();
        terminal.feed(input);
        terminal.screen
    }

    /// The visible rows, right-trimmed. Blank rows stay so row distances hold.
    pub fn lines(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect()
    }

    /// Every visible row joined with newlines, for saving a failed screen.
    pub fn text(&self) -> String {
        let mut text = self.lines().join("\n");
        text.push('\n');
        text
    }

    /// Case-insensitive search across the visible rows.
    pub fn contains(&self, needle: &str) -> bool {
        self.text().to_lowercase().contains(&needle.to_lowercase())
    }

    fn row_mut(&mut self, row: usize) -> &mut Vec<char> {
        if self.rows.len() <= row {
            self.rows.resize_with(row + 1, Vec::new);
        }
        &mut self.rows[row]
    }

    fn put(&mut self, row: usize, col: usize, ch: char) {
        let line = self.row_mut(row);
        if line.len() <= col {
            line.resize(col + 1, ' ');
        }
        line[col] = ch;
    }

    fn erase_line_from(&mut self, row: usize, col: usize) {
        if let Some(line) = self.rows.get_mut(row) {
            line.truncate(col);
        }
    }

    fn erase_line_to(&mut self, row: usize, col: usize) {
        if let Some(line) = self.rows.get_mut(row) {
            for cell in line.iter_mut().take(col + 1) {
                *cell = ' ';
            }
        }
    }

    fn erase_chars(&mut self, row: usize, col: usize, count: usize) {
        if let Some(line) = self.rows.get_mut(row) {
            for cell in line.iter_mut().skip(col).take(count) {
                *cell = ' ';
            }
        }
    }

    fn delete_chars(&mut self, row: usize, col: usize, count: usize) {
        if let Some(line) = self.rows.get_mut(row) {
            if col < line.len() {
                let end = (col + count).min(line.len());
                line.drain(col..end);
            }
        }
    }
}

#[derive(Default)]
struct Terminal {
    screen: Screen,
    cursor: Cursor,
    saved: Option<(usize, usize)>,
}

impl Terminal {
    fn feed(&mut self, input: &[u8]) {
        let mut index = 0;
        while index < input.len() {
            let byte = input[index];
            match byte {
                0x1b => index = self.escape(input, index + 1),
                b'\r' => {
                    self.cursor.col = 0;
                    index += 1;
                }
                b'\n' | 0x0b | 0x0c => {
                    self.cursor.row += 1;
                    index += 1;
                }
                0x08 => {
                    self.cursor.col = self.cursor.col.saturating_sub(1);
                    index += 1;
                }
                b'\t' => {
                    self.cursor.col = (self.cursor.col / 8 + 1) * 8;
                    index += 1;
                }
                0x00..=0x1f | 0x7f => index += 1,
                _ => {
                    let (ch, width) = decode_utf8(&input[index..]);
                    index += width;
                    if let Some(ch) = ch {
                        self.screen.put(self.cursor.row, self.cursor.col, ch);
                        self.cursor.col += 1;
                    }
                }
            }
        }
    }

    /// Handle the sequence after an ESC at `index`; returns the next index.
    fn escape(&mut self, input: &[u8], index: usize) -> usize {
        let Some(&kind) = input.get(index) else {
            return index;
        };
        match kind {
            b'[' => self.csi(input, index + 1),
            // OSC (titles, hyperlinks) and DCS/APC/PM strings end at BEL or ST.
            b']' | b'P' | b'_' | b'^' => skip_string(input, index + 1),
            b'7' => {
                self.saved = Some((self.cursor.row, self.cursor.col));
                index + 1
            }
            b'8' => {
                if let Some((row, col)) = self.saved {
                    self.cursor = Cursor { row, col };
                }
                index + 1
            }
            b'M' => {
                self.cursor.row = self.cursor.row.saturating_sub(1);
                index + 1
            }
            b'c' => {
                *self = Terminal::default();
                index + 1
            }
            // Charset designations carry one more byte.
            b'(' | b')' | b'*' | b'+' => index + 2,
            _ => index + 1,
        }
    }

    fn csi(&mut self, input: &[u8], start: usize) -> usize {
        let mut index = start;
        while index < input.len() && (0x30..=0x3f).contains(&input[index]) {
            index += 1;
        }
        let params_end = index;
        while index < input.len() && (0x20..=0x2f).contains(&input[index]) {
            index += 1;
        }
        let Some(&final_byte) = input.get(index) else {
            return input.len();
        };
        let params = &input[start..params_end];
        let private = params
            .first()
            .is_some_and(|byte| matches!(byte, b'<' | b'=' | b'>' | b'?'));
        let numbers = parse_params(if private { &params[1..] } else { params });
        let first = |default: usize| match numbers.first() {
            Some(Some(value)) if *value > 0 => *value,
            _ => default,
        };
        let cursor = &mut self.cursor;
        match (private, final_byte) {
            (false, b'A') => cursor.row = cursor.row.saturating_sub(first(1)),
            (false, b'B') => cursor.row += first(1),
            (false, b'C') => cursor.col += first(1),
            (false, b'D') => cursor.col = cursor.col.saturating_sub(first(1)),
            (false, b'E') => {
                cursor.row += first(1);
                cursor.col = 0;
            }
            (false, b'F') => {
                cursor.row = cursor.row.saturating_sub(first(1));
                cursor.col = 0;
            }
            (false, b'G') | (false, b'`') => cursor.col = first(1) - 1,
            (false, b'd') => cursor.row = first(1) - 1,
            (false, b'H') | (false, b'f') => {
                cursor.row = first(1) - 1;
                cursor.col = match numbers.get(1) {
                    Some(Some(value)) if *value > 0 => value - 1,
                    _ => 0,
                };
            }
            (false, b'J') => {
                let (row, col) = (cursor.row, cursor.col);
                match first(0) {
                    0 => {
                        self.screen.erase_line_from(row, col);
                        self.screen.rows.truncate(row + 1);
                    }
                    1 => {
                        for line in self.screen.rows.iter_mut().take(row) {
                            line.clear();
                        }
                        self.screen.erase_line_to(row, col);
                    }
                    _ => self.screen.rows.clear(),
                }
            }
            (false, b'K') => {
                let (row, col) = (cursor.row, cursor.col);
                match first(0) {
                    0 => self.screen.erase_line_from(row, col),
                    1 => self.screen.erase_line_to(row, col),
                    _ => self.screen.erase_line_from(row, 0),
                }
            }
            (false, b'X') => {
                let (row, col) = (cursor.row, cursor.col);
                self.screen.erase_chars(row, col, first(1));
            }
            (false, b'P') => {
                let (row, col) = (cursor.row, cursor.col);
                self.screen.delete_chars(row, col, first(1));
            }
            (false, b's') => self.saved = Some((cursor.row, cursor.col)),
            (false, b'u') => {
                if let Some((row, col)) = self.saved {
                    *cursor = Cursor { row, col };
                }
            }
            // Entering or leaving the alternate screen shows a different
            // buffer; what matters afterwards is only what gets drawn next.
            (true, b'h') | (true, b'l')
                if numbers
                    .iter()
                    .any(|mode| matches!(mode, Some(47) | Some(1047) | Some(1049))) =>
            {
                self.screen.rows.clear();
                *cursor = Cursor::default();
            }
            _ => {}
        }
        index + 1
    }
}

fn parse_params(params: &[u8]) -> Vec<Option<usize>> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(|byte| *byte == b';' || *byte == b':')
        .map(|part| std::str::from_utf8(part).ok()?.parse().ok())
        .collect()
}

fn skip_string(input: &[u8], mut index: usize) -> usize {
    while index < input.len() {
        match input[index] {
            0x07 => return index + 1,
            0x1b if input.get(index + 1) == Some(&b'\\') => return index + 2,
            _ => index += 1,
        }
    }
    index
}

/// One character from the front of `bytes`, and how many bytes it used. An
/// invalid or truncated sequence yields no character and consumes one byte.
fn decode_utf8(bytes: &[u8]) -> (Option<char>, usize) {
    let width = match bytes[0] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (None, 1),
    };
    match bytes
        .get(..width)
        .and_then(|slice| std::str::from_utf8(slice).ok())
    {
        Some(text) => (text.chars().next(), width),
        None => (None, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_redraw_lands_on_the_cell_it_replaces() {
        // "73% used", then the renderer moves back up and rewrites one digit.
        let screen = Screen::render(
            b"Current week (all models)\r\n\x1b[3G\xe2\x96\x88\x1b[54G73%\x1b[58Gused\r\nResets Sep 30\r\n\x1b[2A\x1b[55G4\r\x1b[2B",
        );
        assert_eq!(
            screen.lines(),
            vec![
                "Current week (all models)".to_string(),
                format!("  \u{2588}{}74% used", " ".repeat(50)),
                "Resets Sep 30".to_string(),
            ]
        );
    }

    #[test]
    fn erased_rows_do_not_linger() {
        let screen = Screen::render(b"Refreshing\xe2\x80\xa6\r\nEsc to cancel\r\n\x1b[2A\x1b[2KUsage credits\x1b[1B\r\x1b[K");
        assert_eq!(screen.lines(), vec!["Usage credits", ""]);
        assert!(!screen.contains("refreshing"));
    }

    #[test]
    fn a_full_clear_starts_over() {
        let screen = Screen::render(b"old frame\r\n\x1b[2J\x1b[Hnew frame");
        assert_eq!(screen.lines(), vec!["new frame"]);
    }

    #[test]
    fn skips_titles_modes_and_keyboard_protocols() {
        let screen = Screen::render(
            b"\x1b]0;\xe2\x9c\xb3 Claude Code\x07\x1b[?2004h\x1b[>4;2m\x1b[<u\x1b[1mhello\x1b[0m\x1b(B there",
        );
        assert_eq!(screen.lines(), vec!["hello there"]);
    }

    #[test]
    fn save_and_restore_return_to_the_saved_cell() {
        let screen = Screen::render(b"\x1b7abc\r\ndef\x1b8X");
        assert_eq!(screen.lines(), vec!["Xbc", "def"]);
    }
}
