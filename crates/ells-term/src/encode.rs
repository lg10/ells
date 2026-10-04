use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Translate a local key event into the byte sequence a real terminal would
/// send to a PTY. Returns `None` for release/no-op events.
pub fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);
    let shift = m.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                Some(ctrl_char(c))
            } else if alt {
                let mut v = vec![0x1b];
                v.extend_from_slice(&utf(c));
                Some(v)
            } else {
                Some(utf(c))
            }
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(vec![0x1b, b'[', b'Z']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(csi('A', ctrl, alt, shift)),
        KeyCode::Down => Some(csi('B', ctrl, alt, shift)),
        KeyCode::Right => Some(csi('C', ctrl, alt, shift)),
        KeyCode::Left => Some(csi('D', ctrl, alt, shift)),
        KeyCode::Home => Some(if alt {
            vec![0x1b, b'[', b'1', b';', b'5', b'H']
        } else {
            vec![0x1b, b'[', b'H']
        }),
        KeyCode::End => Some(if alt {
            vec![0x1b, b'[', b'1', b';', b'5', b'F']
        } else {
            vec![0x1b, b'[', b'F']
        }),
        KeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        KeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::F(n) => Some(function_key(n, shift, ctrl, alt)),
        _ => None,
    }
}

fn utf(c: char) -> Vec<u8> {
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes().to_vec()
}

fn ctrl_char(c: char) -> Vec<u8> {
    let lower = c.to_ascii_lowercase();
    match lower {
        'a'..='z' => vec![lower as u8 - b'a' + 1],
        ' ' | '2' => vec![0],
        '[' | '3' => vec![27],
        '\\' | '4' => vec![28],
        ']' | '5' => vec![29],
        '^' | '6' => vec![30],
        '_' | '7' | '/' => vec![31],
        '8' => vec![127],
        _ => vec![c as u8],
    }
}

fn csi(letter: char, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
    let letter = letter as u8;
    let mut modifier = 1;
    if shift {
        modifier += 1;
    }
    if alt {
        modifier += 2;
    }
    if ctrl {
        modifier += 4;
    }
    let mut v: Vec<u8> = Vec::new();
    if alt {
        v.push(0x1b);
    }
    if modifier == 1 {
        v.extend_from_slice(&[0x1b, b'[', letter]);
    } else {
        v.extend_from_slice(&[0x1b, b'[', b'1']);
        v.extend_from_slice(b";");
        v.extend_from_slice(modifier.to_string().as_bytes());
        v.push(letter);
    }
    v
}

fn function_key(n: u8, shift: bool, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut modifier = 1u8;
    if shift {
        modifier += 1;
    }
    if alt {
        modifier += 2;
    }
    if ctrl {
        modifier += 4;
    }
    let mut v: Vec<u8> = Vec::new();
    if alt {
        v.push(0x1b);
    }
    match n {
        1..=4 => {
            let tail = b'P' + (n - 1);
            if modifier == 1 {
                v.extend_from_slice(&[0x1b, b'O', tail]);
            } else {
                v.extend_from_slice(&[0x1b, b'[', b'1', b';']);
                v.extend_from_slice(modifier.to_string().as_bytes());
                v.push(tail);
            }
        }
        other => {
            let code = match other {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return vec![],
            };
            v.extend_from_slice(&[0x1b, b'[']);
            v.extend_from_slice(code.to_string().as_bytes());
            if modifier != 1 {
                v.push(b';');
                v.extend_from_slice(modifier.to_string().as_bytes());
            }
            v.push(b'~');
        }
    }
    v
}
