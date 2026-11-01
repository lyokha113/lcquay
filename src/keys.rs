//! Key map: crossterm key events to `Action`s.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::state::{Action, KeyMode, Tab};

/// `mode`: `State::key_mode()`. Ctrl-C quits in every mode.
pub fn key_to_action(k: KeyEvent, mode: KeyMode) -> Option<Action> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('c') {
        return Some(Action::ForceQuit);
    }
    match mode {
        KeyMode::Text => {
            return match k.code {
                KeyCode::Char(c) if !ctrl => Some(Action::FilterChar(c)),
                KeyCode::Backspace => Some(Action::FilterBackspace),
                KeyCode::Enter => Some(Action::FilterApply),
                KeyCode::Esc => Some(Action::FilterCancel),
                KeyCode::Up => Some(Action::Up),
                KeyCode::Down => Some(Action::Down),
                _ => None,
            };
        }
        KeyMode::List if !ctrl => {
            return match k.code {
                KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
                KeyCode::Enter => Some(Action::Enter),
                KeyCode::Esc => Some(Action::Back),
                _ => None,
            };
        }
        KeyMode::Help if !ctrl => {
            return match k.code {
                KeyCode::Esc | KeyCode::Char('?') => Some(Action::ToggleHelp),
                KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
                _ => None,
            };
        }
        KeyMode::Form => {
            return match k.code {
                KeyCode::Char('t') if ctrl => Some(Action::FormTest),
                KeyCode::Char('s') if ctrl => Some(Action::FormSave),
                KeyCode::Char(c) if !ctrl => Some(Action::FilterChar(c)),
                KeyCode::Backspace => Some(Action::FilterBackspace),
                KeyCode::Tab | KeyCode::Down => Some(Action::FormNext),
                KeyCode::BackTab | KeyCode::Up => Some(Action::FormPrev),
                KeyCode::Left => Some(Action::Left),
                KeyCode::Right => Some(Action::Right),
                KeyCode::Esc => Some(Action::Back),
                _ => None,
            };
        }
        KeyMode::Confirm if !ctrl => {
            return match k.code {
                KeyCode::Char('y') => Some(Action::Confirm(true)),
                KeyCode::Char('n') | KeyCode::Esc => Some(Action::Confirm(false)),
                _ => None,
            };
        }
        KeyMode::Preview if !ctrl => {
            return match k.code {
                KeyCode::Char('w') => Some(Action::ToggleWhole),
                KeyCode::Enter => Some(Action::Enter),
                KeyCode::Esc => Some(Action::Back),
                KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
                _ => None,
            };
        }
        KeyMode::Scroll if !ctrl => {
            return match k.code {
                KeyCode::Up | KeyCode::Char('k') => Some(Action::Up),
                KeyCode::Down | KeyCode::Char('j') => Some(Action::Down),
                KeyCode::Char('/') => Some(Action::FilterStart),
                KeyCode::Char('y') => Some(Action::Copy),
                KeyCode::Esc => Some(Action::Back),
                _ => None,
            };
        }
        KeyMode::ExportDetails if !ctrl => {
            return match k.code {
                KeyCode::Char('a') => Some(Action::ToggleAll),
                KeyCode::Enter => Some(Action::Enter),
                KeyCode::Esc => Some(Action::Back),
                _ => None,
            };
        }
        KeyMode::List
        | KeyMode::Help
        | KeyMode::Preview
        | KeyMode::Scroll
        | KeyMode::Confirm
        | KeyMode::ExportDetails => {
            return None;
        }
        KeyMode::Normal => {}
    }
    if ctrl {
        return match k.code {
            KeyCode::Char('d') => Some(Action::ScrollDown),
            KeyCode::Char('u') => Some(Action::ScrollUp),
            _ => None,
        };
    }
    Some(match k.code {
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Tab => Action::NextPane,
        KeyCode::BackTab => Action::PrevPane,
        KeyCode::Right | KeyCode::Char('l') => Action::Right,
        KeyCode::Left | KeyCode::Char('h') => Action::Left,
        KeyCode::Enter => Action::Enter,
        KeyCode::Char('1') => Action::Tab(Tab::Overview),
        KeyCode::Char('2') => Action::Tab(Tab::Layers),
        KeyCode::Char('3') => Action::Tab(Tab::Config),
        KeyCode::Char('4') => Action::Tab(Tab::History),
        KeyCode::Char('5') => Action::Tab(Tab::Raw),
        KeyCode::Char('[') => Action::PlatformPrev,
        KeyCode::Char(']') => Action::PlatformNext,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('/') => Action::FilterStart,
        KeyCode::Char('o') => Action::SortCycle,
        KeyCode::Char('O') => Action::SortReverse,
        KeyCode::Char('y') => Action::Copy,
        KeyCode::Char('m') => Action::ToggleReveal,
        KeyCode::Char('?') => Action::ToggleHelp,
        KeyCode::Char('L') => Action::Login,
        KeyCode::Char('p') => Action::PushStart,
        KeyCode::Char('P') => Action::PullStart,
        KeyCode::Char('C') => Action::CopyStart,
        KeyCode::Char('X') => Action::ExportStart,
        KeyCode::Char('e') => Action::EditConfig,
        KeyCode::Char('t') => Action::ThemePick,
        KeyCode::Char(' ') => Action::Mark,
        KeyCode::Char('u') => Action::MarkUntagged,
        KeyCode::Char('a') => Action::MarkGlob,
        KeyCode::Char('v') => Action::RangeMark,
        KeyCode::Char('=') => Action::CompareMark,
        KeyCode::Char('!') => Action::Requests,
        KeyCode::Char('d') => Action::DeleteStart,
        KeyCode::Char('c') => Action::RegistryEdit,
        KeyCode::Char('n') => Action::RegistryNew,
        KeyCode::Char('x') => Action::RegistryRemove,
        KeyCode::Esc => Action::ClearMarks,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn nav(e: KeyEvent) -> Option<Action> {
        key_to_action(e, KeyMode::Normal)
    }

    #[test]
    fn normal_mode_mappings() {
        use Action::*;
        let c = |ch| k(KeyCode::Char(ch));
        let cases = [
            (k(KeyCode::Up), Up),
            (c('k'), Up),
            (k(KeyCode::Down), Down),
            (c('j'), Down),
            (k(KeyCode::Tab), NextPane),
            (k(KeyCode::Right), Right),
            (c('l'), Right),
            (
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                PrevPane,
            ),
            (k(KeyCode::Left), Left),
            (c('h'), Left),
            (k(KeyCode::Enter), Enter),
            (c('1'), Tab(crate::app::state::Tab::Overview)),
            (c('2'), Tab(crate::app::state::Tab::Layers)),
            (c('3'), Tab(crate::app::state::Tab::Config)),
            (c('4'), Tab(crate::app::state::Tab::History)),
            (c('5'), Tab(crate::app::state::Tab::Raw)),
            (c('['), PlatformPrev),
            (c(']'), PlatformNext),
            (ctrl('d'), ScrollDown),
            (ctrl('u'), ScrollUp),
            (c('r'), Refresh),
            (c('/'), FilterStart),
            (c('o'), SortCycle),
            (c('O'), SortReverse),
            (c('y'), Copy),
            (c('m'), ToggleReveal),
            (c('?'), ToggleHelp),
            (c('L'), Login),
            (c('q'), Quit),
            (c('p'), PushStart),
            (c('P'), PullStart),
            (c('C'), CopyStart),
            (c('X'), ExportStart),
            (c('e'), EditConfig),
            (c('t'), ThemePick),
            (c(' '), Mark),
            (c('u'), MarkUntagged),
            (c('a'), MarkGlob),
            (c('v'), RangeMark),
            (c('!'), Requests),
            (c('d'), DeleteStart),
            (c('c'), RegistryEdit),
            (c('n'), RegistryNew),
            (c('x'), RegistryRemove),
            (k(KeyCode::Esc), ClearMarks),
            (ctrl('c'), ForceQuit),
        ];
        for (e, a) in cases {
            assert_eq!(nav(e), Some(a), "{e:?}");
        }
        assert_eq!(nav(c('z')), None);
        assert_eq!(nav(ctrl('x')), None);
    }

    #[test]
    fn filter_mode_captures_printables() {
        let f = |e| key_to_action(e, KeyMode::Text);
        assert_eq!(f(k(KeyCode::Char('q'))), Some(Action::FilterChar('q')));
        assert_eq!(f(k(KeyCode::Char('/'))), Some(Action::FilterChar('/')));
        assert_eq!(f(k(KeyCode::Char('L'))), Some(Action::FilterChar('L')));
        assert_eq!(
            f(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT)),
            Some(Action::FilterChar('Q'))
        );
        assert_eq!(f(k(KeyCode::Backspace)), Some(Action::FilterBackspace));
        assert_eq!(f(k(KeyCode::Enter)), Some(Action::FilterApply));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::FilterCancel));
        for ch in ['r', 'd', 'p', 'e', 'j', 'k', 'u', 'a', 'w', ' '] {
            assert_eq!(f(k(KeyCode::Char(ch))), Some(Action::FilterChar(ch)));
        }
        assert_eq!(f(k(KeyCode::Up)), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::Down));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        assert_eq!(f(ctrl('d')), None);
        assert_eq!(f(k(KeyCode::Tab)), None);
    }

    #[test]
    fn list_mode_moves_enters_and_backs() {
        let f = |e| key_to_action(e, KeyMode::List);
        assert_eq!(f(k(KeyCode::Up)), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Enter)), Some(Action::Enter));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        for ch in ['q', 'r', 'p', 'L', '?', 'h', 'l'] {
            assert_eq!(f(k(KeyCode::Char(ch))), None, "{ch}");
        }
        assert_eq!(f(k(KeyCode::Tab)), None);
        assert_eq!(f(ctrl('d')), None);
    }

    #[test]
    fn help_mode_closes_and_scrolls() {
        let f = |e| key_to_action(e, KeyMode::Help);
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::ToggleHelp));
        assert_eq!(f(k(KeyCode::Char('?'))), Some(Action::ToggleHelp));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        assert_eq!(f(k(KeyCode::Char('p'))), None);
        assert_eq!(f(k(KeyCode::Up)), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Char('j'))), Some(Action::Down));
    }

    #[test]
    fn form_mode_keys() {
        let f = |e| key_to_action(e, KeyMode::Form);
        for ch in ['q', 'c', 'x', 'j', 'k', 'y', 'n', ' ', '/', ':'] {
            assert_eq!(
                f(k(KeyCode::Char(ch))),
                Some(Action::FilterChar(ch)),
                "{ch}"
            );
        }
        assert_eq!(f(k(KeyCode::Backspace)), Some(Action::FilterBackspace));
        assert_eq!(f(k(KeyCode::Tab)), Some(Action::FormNext));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::FormNext));
        assert_eq!(
            f(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Action::FormPrev)
        );
        assert_eq!(f(k(KeyCode::Up)), Some(Action::FormPrev));
        assert_eq!(f(k(KeyCode::Left)), Some(Action::Left));
        assert_eq!(f(k(KeyCode::Right)), Some(Action::Right));
        assert_eq!(f(ctrl('t')), Some(Action::FormTest));
        assert_eq!(f(ctrl('s')), Some(Action::FormSave));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(f(k(KeyCode::Enter)), None);
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        assert_eq!(f(ctrl('d')), None);
    }

    #[test]
    fn confirm_mode_keys() {
        let f = |e| key_to_action(e, KeyMode::Confirm);
        assert_eq!(f(k(KeyCode::Char('y'))), Some(Action::Confirm(true)));
        assert_eq!(f(k(KeyCode::Char('n'))), Some(Action::Confirm(false)));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Confirm(false)));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        for ch in ['Y', 'q', 'x'] {
            assert_eq!(f(k(KeyCode::Char(ch))), None, "{ch}");
        }
        assert_eq!(f(k(KeyCode::Enter)), None);
    }

    #[test]
    fn export_details_keys() {
        let f = |e| key_to_action(e, KeyMode::ExportDetails);
        assert_eq!(f(k(KeyCode::Char('a'))), Some(Action::ToggleAll));
        assert_eq!(f(k(KeyCode::Enter)), Some(Action::Enter));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        for ch in ['q', 'X', 'j', 'y', ' '] {
            assert_eq!(f(k(KeyCode::Char(ch))), None, "{ch}");
        }
        assert_eq!(f(ctrl('a')), None);
        // Normal mode keeps `a` for the glob prompt.
        assert_eq!(nav(k(KeyCode::Char('a'))), Some(Action::MarkGlob));
    }

    #[test]
    fn preview_mode_keys() {
        let f = |e| key_to_action(e, KeyMode::Preview);
        assert_eq!(f(k(KeyCode::Char('w'))), Some(Action::ToggleWhole));
        assert_eq!(f(k(KeyCode::Enter)), Some(Action::Enter));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(f(k(KeyCode::Up)), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        for ch in ['d', 'u', 'a', ' ', 'q'] {
            assert_eq!(f(k(KeyCode::Char(ch))), None, "{ch}");
        }
        assert_eq!(f(ctrl('d')), None);
    }

    #[test]
    fn scroll_mode_keys() {
        let f = |e| key_to_action(e, KeyMode::Scroll);
        assert_eq!(f(k(KeyCode::Up)), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(f(k(KeyCode::Down)), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(f(k(KeyCode::Char('/'))), Some(Action::FilterStart));
        assert_eq!(f(k(KeyCode::Char('y'))), Some(Action::Copy));
        assert_eq!(f(k(KeyCode::Esc)), Some(Action::Back));
        assert_eq!(f(ctrl('c')), Some(Action::ForceQuit));
        for ch in ['q', '!', 'r', 'p', '?', 'h', 'l'] {
            assert_eq!(f(k(KeyCode::Char(ch))), None, "{ch}");
        }
        assert_eq!(f(k(KeyCode::Tab)), None);
        assert_eq!(f(ctrl('d')), None);
    }

    #[test]
    fn release_events_ignored() {
        let mut e = k(KeyCode::Char('q'));
        e.kind = KeyEventKind::Release;
        assert_eq!(nav(e), None);
    }
}
