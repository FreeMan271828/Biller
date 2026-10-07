//! 终端按键的统一抽象。
//!
//! 状态机只认识这里的 [`Key`]；具体终端库（crossterm / ratatui）的按键
//! 由 `event.rs` 翻译成本类型。这样状态机可以完全脱离终端做单元测试，
//! 以后换渲染库也不用动业务逻辑。

/// 与终端库无关的按键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    BackTab,
    Enter,
    Esc,
    Backspace,
    Delete,
    /// 普通字符输入。
    Char(char),
    /// Ctrl + 字符（统一成小写）。
    Ctrl(char),
}

impl Key {
    /// 表单里可直接输入的字符。
    pub fn typed(self) -> Option<char> {
        match self {
            Key::Char(c) => Some(c),
            _ => None,
        }
    }
}
