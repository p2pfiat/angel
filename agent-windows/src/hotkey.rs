use anyhow::{bail, Result};

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

impl Hotkey {
    /// Parse "ctrl+alt+shift+k", "ctrl+alt+f9", ... At least two modifiers are required so a
    /// hotkey is never pressed by accident.
    pub fn parse(s: &str) -> Result<Self> {
        let mut modifiers = 0;
        let mut vk = None;
        for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => modifiers |= MOD_CONTROL,
                "alt" => modifiers |= MOD_ALT,
                "shift" => modifiers |= MOD_SHIFT,
                "win" => modifiers |= MOD_WIN,
                key => {
                    if vk.is_some() {
                        bail!("hotkey {s:?}: more than one key");
                    }
                    vk = Some(key_code(key).ok_or_else(|| anyhow::anyhow!("hotkey {s:?}: unknown key {key:?}"))?);
                }
            }
        }
        let Some(vk) = vk else { bail!("hotkey {s:?}: no key") };
        if modifiers.count_ones() < 2 {
            bail!("hotkey {s:?}: use at least two modifiers (ctrl, alt, shift, win)");
        }
        Ok(Self { modifiers, vk })
    }
}

fn key_code(key: &str) -> Option<u32> {
    let b = key.as_bytes();
    match b {
        [c] if c.is_ascii_lowercase() => Some(c.to_ascii_uppercase() as u32),
        [c] if c.is_ascii_digit() => Some(*c as u32),
        [b'f', ..] => match key[1..].parse::<u32>() {
            Ok(n @ 1..=24) => Some(0x70 + n - 1),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(
            Hotkey::parse("ctrl+alt+shift+k").unwrap(),
            Hotkey { modifiers: MOD_CONTROL | MOD_ALT | MOD_SHIFT, vk: 'K' as u32 }
        );
        assert_eq!(Hotkey::parse("Ctrl + Alt + F9").unwrap().vk, 0x78);
        assert_eq!(Hotkey::parse("win+alt+7").unwrap().vk, '7' as u32);
    }

    #[test]
    fn rejects_weak_or_invalid() {
        assert!(Hotkey::parse("ctrl+k").is_err());
        assert!(Hotkey::parse("ctrl+alt").is_err());
        assert!(Hotkey::parse("ctrl+alt+k+j").is_err());
        assert!(Hotkey::parse("ctrl+alt+f25").is_err());
        assert!(Hotkey::parse("ctrl+alt+enter").is_err());
    }
}
