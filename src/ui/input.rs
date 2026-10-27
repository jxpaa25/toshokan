//! Single-line text input with a char-indexed cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Default)]
pub struct TextInput {
    value: String,
    /// Cursor position in chars, from 0 to the number of chars in `value`.
    cursor: usize,
}

impl TextInput {
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Applies an editing key. Returns false for keys the input doesn't handle.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.len(),
            KeyCode::Char('u') if ctrl => {
                let end = self.byte_index(self.cursor);
                self.value.drain(..end);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word_before_cursor(),
            KeyCode::Char(_) if ctrl || alt => return false,
            KeyCode::Char(c) => {
                let at = self.byte_index(self.cursor);
                self.value.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.len() {
                    let at = self.byte_index(self.cursor);
                    self.value.remove(at);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }

    /// Draws the input inside `block`, scrolling horizontally to keep the cursor visible.
    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        block: Block,
        placeholder: &str,
        focused: bool,
    ) {
        let inner = block.inner(area);
        let cursor = self.value[..self.byte_index(self.cursor)].width() as u16;
        let scroll = cursor.saturating_sub(inner.width.saturating_sub(1));
        let text = if self.value.is_empty() {
            Paragraph::new(placeholder.dark_gray())
        } else {
            Paragraph::new(self.value.as_str())
        };
        frame.render_widget(text.scroll((0, scroll)).block(block), area);
        if focused {
            frame.set_cursor_position((inner.x + cursor - scroll, inner.y));
        }
    }

    fn delete_word_before_cursor(&mut self) {
        let before: Vec<char> = self.value.chars().take(self.cursor).collect();
        let mut start = before.len();
        while start > 0 && before[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !before[start - 1].is_whitespace() {
            start -= 1;
        }
        let range = self.byte_index(start)..self.byte_index(self.cursor);
        self.value.drain(range);
        self.cursor = start;
    }

    fn len(&self) -> usize {
        self.value.chars().count()
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_index)
            .map_or(self.value.len(), |(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(input: &mut TextInput, code: KeyCode) {
        input.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn ctrl(input: &mut TextInput, c: char) {
        input.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    fn type_str(input: &mut TextInput, text: &str) {
        for c in text.chars() {
            press(input, KeyCode::Char(c));
        }
    }

    #[test]
    fn edits_multibyte_text_at_cursor() {
        let mut input = TextInput::default();
        type_str(&mut input, "ベルク");
        press(&mut input, KeyCode::Left);
        press(&mut input, KeyCode::Left);
        type_str(&mut input, "ル");
        assert_eq!(input.value(), "ベルルク");
        press(&mut input, KeyCode::Backspace);
        press(&mut input, KeyCode::Delete);
        assert_eq!(input.value(), "ベク");
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        let mut input = TextInput::default();
        type_str(&mut input, "one punch  ");
        ctrl(&mut input, 'w');
        assert_eq!(input.value(), "one ");
        ctrl(&mut input, 'u');
        assert_eq!(input.value(), "");
    }
}
