//! 把 crossterm 的按键事件翻译成与终端库无关的 [`Key`]。
//!
//! 单独一层的好处：状态机（`app.rs`）不认识任何终端库，换渲染方案时不用改业务。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::input::Key;

/// 翻译一次按键事件；无法识别的按键返回 `None`。
///
/// 只处理「按下」（`KeyEventKind::Press`）：
/// - Windows 下长按会补发 `Repeat`，若不拦住，回车会被连发两次——
///   第一下展开选择列表、第二下立刻确认，看起来就像「根本没展开」；
/// - `Release` 同理必须过滤，否则每个键都会被处理两次。
pub fn translate(event: KeyEvent) -> Option<Key> {
    if !matches!(event.kind, KeyEventKind::Press) {
        return None;
    }

    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);

    let key = match event.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Char(character) => {
            if ctrl {
                Key::Ctrl(character.to_ascii_lowercase())
            } else {
                Key::Char(character)
            }
        }
        _ => return None,
    };

    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::input::Key;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_navigation_and_editing_keys() {
        assert_eq!(translate(press(KeyCode::Up)), Some(Key::Up));
        assert_eq!(translate(press(KeyCode::Tab)), Some(Key::Tab));
        assert_eq!(translate(press(KeyCode::BackTab)), Some(Key::BackTab));
        assert_eq!(translate(press(KeyCode::Enter)), Some(Key::Enter));
        assert_eq!(translate(press(KeyCode::Esc)), Some(Key::Esc));
        assert_eq!(translate(press(KeyCode::Backspace)), Some(Key::Backspace));
        assert_eq!(translate(press(KeyCode::Delete)), Some(Key::Delete));
    }

    #[test]
    fn maps_characters_and_ctrl() {
        assert_eq!(translate(press(KeyCode::Char('a'))), Some(Key::Char('a')));
        let ctrl_c = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::CONTROL);
        assert_eq!(translate(ctrl_c), Some(Key::Ctrl('c')));
    }

    #[test]
    fn ignores_release_and_repeat() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(translate(release), None);

        // 长按补发的 Repeat 也必须忽略，否则「回车展开」会被立刻确认掉
        let repeat = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        );
        assert_eq!(translate(repeat), None);
    }
}
