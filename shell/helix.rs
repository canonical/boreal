use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use reedline::{
    Direction, EditCommand, EditMode, FindStop, Keybindings, MotionTarget, PromptEditMode,
    PromptViMode, ReedlineEvent, ReedlineRawEvent, WordEdge, WordKind,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Insert,
    Normal,
    Select,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    None,
    Goto,
    Find {
        direction: Direction,
        stop: FindStop,
    },
    Replace,
}

/// Selection-first Helix-style modal line editor built on Reedline's block-cursor primitives.
pub struct Helix {
    insert_keys: Keybindings,
    mode: Mode,
    pending: Pending,
}

impl Helix {
    pub fn new(insert_keys: Keybindings) -> Self {
        Self {
            insert_keys,
            mode: Mode::Insert,
            pending: Pending::None,
        }
    }

    fn is_select(&self) -> bool {
        self.mode == Mode::Select
    }

    fn select_motion(&self, target: MotionTarget) -> ReedlineEvent {
        if self.is_select() {
            ReedlineEvent::Edit(vec![EditCommand::Extend(target)])
        } else {
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Esc,
                ReedlineEvent::Edit(vec![EditCommand::Extend(target)]),
            ])
        }
    }

    fn handle_pending(
        &mut self,
        pending: Pending,
        modifiers: KeyModifiers,
        code: KeyCode,
    ) -> ReedlineEvent {
        if modifiers == KeyModifiers::NONE && code == KeyCode::Esc {
            return ReedlineEvent::Repaint;
        }
        match pending {
            Pending::None => ReedlineEvent::None,
            Pending::Replace => match (modifiers, code) {
                (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(raw)) => {
                    let ch = if modifiers == KeyModifiers::SHIFT {
                        raw.to_ascii_uppercase()
                    } else {
                        raw
                    };
                    ReedlineEvent::Edit(vec![EditCommand::ReplaceChar(ch)])
                }
                _ => ReedlineEvent::None,
            },
            Pending::Find { direction, stop } => match (modifiers, code) {
                (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(raw)) => {
                    let ch = if modifiers == KeyModifiers::SHIFT {
                        raw.to_ascii_uppercase()
                    } else {
                        raw
                    };
                    self.select_motion(MotionTarget::Find {
                        ch,
                        direction,
                        stop,
                    })
                }
                _ => ReedlineEvent::None,
            },
            Pending::Goto => {
                let select = self.is_select();
                match (modifiers, code) {
                    (KeyModifiers::NONE, KeyCode::Char('h')) => {
                        ReedlineEvent::Edit(vec![EditCommand::MoveToLineStart { select }])
                    }
                    (KeyModifiers::NONE, KeyCode::Char('l')) => {
                        ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd { select }])
                    }
                    (KeyModifiers::NONE, KeyCode::Char('s')) => {
                        ReedlineEvent::Edit(vec![EditCommand::MoveToLineNonBlankStart { select }])
                    }
                    (KeyModifiers::NONE, KeyCode::Char('g')) => {
                        ReedlineEvent::Edit(vec![EditCommand::MoveToStart { select }])
                    }
                    (KeyModifiers::NONE, KeyCode::Char('e')) => {
                        ReedlineEvent::Edit(vec![EditCommand::MoveToEnd { select }])
                    }
                    _ => ReedlineEvent::None,
                }
            }
        }
    }

    fn handle_insert_key(&mut self, modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        match (modifiers, code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.mode = Mode::Normal;
                self.pending = Pending::None;
                ReedlineEvent::Multiple(vec![
                    ReedlineEvent::Esc,
                    ReedlineEvent::Edit(vec![EditCommand::Move(MotionTarget::Grapheme(
                        Direction::Backward,
                    ))]),
                    ReedlineEvent::Repaint,
                ])
            }
            (modifier, KeyCode::Char(c)) => {
                let lookup = match modifier {
                    KeyModifiers::NONE => c,
                    _ => c.to_ascii_lowercase(),
                };
                self.insert_keys
                    .find_binding(modifier, KeyCode::Char(lookup))
                    .unwrap_or_else(|| {
                        if modifier == KeyModifiers::NONE
                            || modifier == KeyModifiers::SHIFT
                            || modifier == KeyModifiers::CONTROL | KeyModifiers::ALT
                            || modifier
                                == KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT
                        {
                            ReedlineEvent::Edit(vec![EditCommand::InsertChar(
                                if modifier == KeyModifiers::SHIFT {
                                    c.to_ascii_uppercase()
                                } else {
                                    c
                                },
                            )])
                        } else {
                            ReedlineEvent::None
                        }
                    })
            }
            _ => self
                .insert_keys
                .find_binding(modifiers, code)
                .unwrap_or_else(|| {
                    if modifiers == KeyModifiers::NONE && code == KeyCode::Enter {
                        ReedlineEvent::Enter
                    } else {
                        ReedlineEvent::None
                    }
                }),
        }
    }

    fn handle_modal_key(&mut self, modifiers: KeyModifiers, code: KeyCode) -> ReedlineEvent {
        if self.pending != Pending::None {
            let pending = std::mem::replace(&mut self.pending, Pending::None);
            return self.handle_pending(pending, modifiers, code);
        }

        if modifiers == KeyModifiers::ALT && matches!(code, KeyCode::Char(';')) {
            return ReedlineEvent::Edit(vec![EditCommand::SwapCursorAndAnchor]);
        }

        let select = self.is_select();
        match (modifiers, code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                if self.mode == Mode::Select {
                    self.mode = Mode::Normal;
                    ReedlineEvent::Repaint
                } else {
                    ReedlineEvent::Esc
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.mode = Mode::Insert;
                ReedlineEvent::Multiple(vec![
                    ReedlineEvent::Esc,
                    ReedlineEvent::Edit(vec![EditCommand::MoveToEnd { select: false }]),
                    ReedlineEvent::Enter,
                ])
            }
            (KeyModifiers::NONE, KeyCode::Tab) => ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion".into()),
                ReedlineEvent::MenuNext,
            ]),
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                ReedlineEvent::MenuPrevious
            }
            (KeyModifiers::NONE, KeyCode::Left) => ReedlineEvent::UntilFound(vec![
                ReedlineEvent::MenuLeft,
                ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select }]),
            ]),
            (KeyModifiers::NONE, KeyCode::Right) => ReedlineEvent::UntilFound(vec![
                ReedlineEvent::HistoryHintComplete,
                ReedlineEvent::MenuRight,
                ReedlineEvent::Edit(vec![EditCommand::MoveRight { select }]),
            ]),
            (KeyModifiers::NONE, KeyCode::Up) => {
                if select {
                    ReedlineEvent::Edit(vec![EditCommand::MoveLineUp { select: true }])
                } else {
                    ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuUp, ReedlineEvent::Up])
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if select {
                    ReedlineEvent::Edit(vec![EditCommand::MoveLineDown { select: true }])
                } else {
                    ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuDown, ReedlineEvent::Down])
                }
            }
            (KeyModifiers::NONE, KeyCode::Home) => {
                ReedlineEvent::Edit(vec![EditCommand::MoveToLineStart { select }])
            }
            (KeyModifiers::NONE, KeyCode::End) => {
                ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd { select }])
            }
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select }])
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                self.mode = Mode::Normal;
                ReedlineEvent::Edit(vec![EditCommand::CutSelection])
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(raw)) => {
                let ch = if modifiers == KeyModifiers::SHIFT {
                    raw.to_ascii_uppercase()
                } else {
                    raw
                };
                match ch {
                    'i' => {
                        self.mode = Mode::Insert;
                        ReedlineEvent::Multiple(vec![ReedlineEvent::Esc, ReedlineEvent::Repaint])
                    }
                    'a' => {
                        self.mode = Mode::Insert;
                        ReedlineEvent::Multiple(vec![
                            ReedlineEvent::Esc,
                            ReedlineEvent::Edit(vec![EditCommand::MoveRight { select: false }]),
                            ReedlineEvent::Repaint,
                        ])
                    }
                    'I' => {
                        self.mode = Mode::Insert;
                        ReedlineEvent::Multiple(vec![
                            ReedlineEvent::Esc,
                            ReedlineEvent::Edit(vec![EditCommand::MoveToLineNonBlankStart {
                                select: false,
                            }]),
                            ReedlineEvent::Repaint,
                        ])
                    }
                    'A' => {
                        self.mode = Mode::Insert;
                        ReedlineEvent::Multiple(vec![
                            ReedlineEvent::Esc,
                            ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd { select: false }]),
                            ReedlineEvent::Repaint,
                        ])
                    }
                    'v' => {
                        self.mode = if select { Mode::Normal } else { Mode::Select };
                        ReedlineEvent::Repaint
                    }
                    ';' => ReedlineEvent::Esc,
                    'x' => ReedlineEvent::Edit(vec![
                        EditCommand::MoveToLineStart { select: false },
                        EditCommand::MoveToLineEnd { select: true },
                    ]),
                    '%' => ReedlineEvent::Edit(vec![EditCommand::SelectAll]),
                    'h' => ReedlineEvent::UntilFound(vec![
                        ReedlineEvent::MenuLeft,
                        ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select }]),
                    ]),
                    'l' => ReedlineEvent::UntilFound(vec![
                        ReedlineEvent::HistoryHintComplete,
                        ReedlineEvent::MenuRight,
                        ReedlineEvent::Edit(vec![EditCommand::MoveRight { select }]),
                    ]),
                    'k' => {
                        if select {
                            ReedlineEvent::Edit(vec![EditCommand::MoveLineUp { select: true }])
                        } else {
                            ReedlineEvent::UntilFound(vec![
                                ReedlineEvent::MenuUp,
                                ReedlineEvent::Up,
                            ])
                        }
                    }
                    'j' => {
                        if select {
                            ReedlineEvent::Edit(vec![EditCommand::MoveLineDown { select: true }])
                        } else {
                            ReedlineEvent::UntilFound(vec![
                                ReedlineEvent::MenuDown,
                                ReedlineEvent::Down,
                            ])
                        }
                    }
                    '0' => ReedlineEvent::Edit(vec![EditCommand::MoveToLineStart { select }]),
                    '$' => ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd { select }]),
                    'w' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::Word,
                        edge: WordEdge::Start,
                        direction: Direction::Forward,
                    }),
                    'W' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::LongWord,
                        edge: WordEdge::Start,
                        direction: Direction::Forward,
                    }),
                    'b' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::Word,
                        edge: WordEdge::Start,
                        direction: Direction::Backward,
                    }),
                    'B' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::LongWord,
                        edge: WordEdge::Start,
                        direction: Direction::Backward,
                    }),
                    'e' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::Word,
                        edge: WordEdge::End,
                        direction: Direction::Forward,
                    }),
                    'E' => self.select_motion(MotionTarget::Word {
                        kind: WordKind::LongWord,
                        edge: WordEdge::End,
                        direction: Direction::Forward,
                    }),
                    'g' => {
                        self.pending = Pending::Goto;
                        ReedlineEvent::None
                    }
                    'f' => {
                        self.pending = Pending::Find {
                            direction: Direction::Forward,
                            stop: FindStop::On,
                        };
                        ReedlineEvent::None
                    }
                    'F' => {
                        self.pending = Pending::Find {
                            direction: Direction::Backward,
                            stop: FindStop::On,
                        };
                        ReedlineEvent::None
                    }
                    't' => {
                        self.pending = Pending::Find {
                            direction: Direction::Forward,
                            stop: FindStop::Before,
                        };
                        ReedlineEvent::None
                    }
                    'T' => {
                        self.pending = Pending::Find {
                            direction: Direction::Backward,
                            stop: FindStop::Before,
                        };
                        ReedlineEvent::None
                    }
                    'r' => {
                        self.pending = Pending::Replace;
                        ReedlineEvent::None
                    }
                    'd' => {
                        self.mode = Mode::Normal;
                        ReedlineEvent::Edit(vec![EditCommand::CutSelection])
                    }
                    'c' => {
                        self.mode = Mode::Insert;
                        ReedlineEvent::Multiple(vec![
                            ReedlineEvent::Edit(vec![EditCommand::CutSelection]),
                            ReedlineEvent::Repaint,
                        ])
                    }
                    'y' => ReedlineEvent::Edit(vec![EditCommand::CopySelection]),
                    'p' => ReedlineEvent::Edit(vec![EditCommand::PasteCutBufferAfter]),
                    'P' => ReedlineEvent::Edit(vec![EditCommand::PasteCutBufferBefore]),
                    'u' => ReedlineEvent::Edit(vec![EditCommand::Undo]),
                    'U' => ReedlineEvent::Edit(vec![EditCommand::Redo]),
                    '~' => ReedlineEvent::Edit(vec![EditCommand::SwitchcaseChar]),
                    _ => ReedlineEvent::None,
                }
            }
            _ => ReedlineEvent::None,
        }
    }
}

impl EditMode for Helix {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        match event.into() {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => match (modifiers, code) {
                (KeyModifiers::CONTROL, KeyCode::Char('c' | 'C')) => {
                    self.pending = Pending::None;
                    self.mode = Mode::Insert;
                    ReedlineEvent::CtrlC
                }
                (KeyModifiers::CONTROL, KeyCode::Char('d' | 'D')) => {
                    self.pending = Pending::None;
                    ReedlineEvent::CtrlD
                }
                (KeyModifiers::CONTROL, KeyCode::Char('l' | 'L')) => {
                    self.pending = Pending::None;
                    ReedlineEvent::ClearScreen
                }
                (KeyModifiers::CONTROL, KeyCode::Char('r' | 'R')) => {
                    self.pending = Pending::None;
                    ReedlineEvent::SearchHistory
                }
                _ if self.mode == Mode::Insert => self.handle_insert_key(modifiers, code),
                _ => self.handle_modal_key(modifiers, code),
            },
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(button),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }) => ReedlineEvent::Mouse {
                column,
                row,
                button: button.into(),
            },
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost => ReedlineEvent::None,
            Event::Resize(width, height) => ReedlineEvent::Resize(width, height),
            Event::Paste(body) => ReedlineEvent::Edit(vec![EditCommand::InsertString(
                body.replace("\r\n", "\n").replace('\r', "\n"),
            )]),
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        match self.mode {
            Mode::Insert => PromptEditMode::Vi(PromptViMode::Insert),
            Mode::Normal | Mode::Select => PromptEditMode::Vi(PromptViMode::Visual),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::default_vi_insert_keybindings;

    fn raw_key(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
    }

    fn mock_helix() -> Helix {
        let mut keys = default_vi_insert_keybindings();
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion".into()),
                ReedlineEvent::MenuNext,
            ]),
        );
        Helix::new(keys)
    }

    #[test]
    fn test_helix_mode_transitions_and_submission() {
        let mut hx = mock_helix();
        assert_eq!(hx.edit_mode(), PromptEditMode::Vi(PromptViMode::Insert));
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('w'), KeyModifiers::NONE)),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('w')])
        );

        let esc = hx.parse_event(raw_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(hx.edit_mode(), PromptEditMode::Vi(PromptViMode::Visual));
        assert!(matches!(esc, ReedlineEvent::Multiple(_)));

        // 'v' toggles select mode, Esc returns to normal mode without dropping selection.
        hx.parse_event(raw_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(hx.mode, Mode::Select);
        hx.parse_event(raw_key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(hx.mode, Mode::Normal);

        // 'c' cuts current selection and returns to insert mode.
        let change = hx.parse_event(raw_key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(hx.edit_mode(), PromptEditMode::Vi(PromptViMode::Insert));
        assert_eq!(
            change,
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::CutSelection]),
                ReedlineEvent::Repaint,
            ])
        );

        // Enter in normal mode submits cleanly and resets to insert mode.
        hx.parse_event(raw_key(KeyCode::Esc, KeyModifiers::NONE));
        let submit = hx.parse_event(raw_key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(hx.edit_mode(), PromptEditMode::Vi(PromptViMode::Insert));
        assert_eq!(
            submit,
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Esc,
                ReedlineEvent::Edit(vec![EditCommand::MoveToEnd { select: false }]),
                ReedlineEvent::Enter,
            ])
        );
    }

    #[test]
    fn test_helix_selection_first_motions_and_prefixes() {
        let mut hx = mock_helix();
        hx.parse_event(raw_key(KeyCode::Esc, KeyModifiers::NONE));

        // Normal 'w' resets anchor then extends forward to next word start.
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('w'), KeyModifiers::NONE)),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Esc,
                ReedlineEvent::Edit(vec![EditCommand::Extend(MotionTarget::Word {
                    kind: WordKind::Word,
                    edge: WordEdge::Start,
                    direction: Direction::Forward,
                })]),
            ])
        );

        // Select ('v') 'w' extends existing selection without resetting anchor.
        hx.parse_event(raw_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('w'), KeyModifiers::NONE)),
            ReedlineEvent::Edit(vec![EditCommand::Extend(MotionTarget::Word {
                kind: WordKind::Word,
                edge: WordEdge::Start,
                direction: Direction::Forward,
            })])
        );
        hx.parse_event(raw_key(KeyCode::Esc, KeyModifiers::NONE));

        // Goto ('gh' / 'gl'), find ('fa'), replace ('rx'), and redo ('U').
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('g'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('l'), KeyModifiers::NONE)),
            ReedlineEvent::Edit(vec![EditCommand::MoveToLineEnd { select: false }])
        );

        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('f'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('a'), KeyModifiers::NONE)),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Esc,
                ReedlineEvent::Edit(vec![EditCommand::Extend(MotionTarget::Find {
                    ch: 'a',
                    direction: Direction::Forward,
                    stop: FindStop::On,
                })]),
            ])
        );

        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('r'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('z'), KeyModifiers::NONE)),
            ReedlineEvent::Edit(vec![EditCommand::ReplaceChar('z')])
        );
        assert_eq!(
            hx.parse_event(raw_key(KeyCode::Char('u'), KeyModifiers::SHIFT)),
            ReedlineEvent::Edit(vec![EditCommand::Redo])
        );
    }
}
