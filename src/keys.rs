//! Parse human-friendly key strings into raw bytes for a PTY master.
//!
//! Input is a free-form string. Anything outside `<...>` is sent as
//! literal UTF-8 bytes. Names inside `<...>` map to common terminal
//! key escapes:
//!
//! * `<Enter>` `<CR>` -> `\r`
//! * `<LF>`           -> `\n`
//! * `<Tab>`          -> `\t`
//! * `<Esc>`          -> `\x1b`
//! * `<Backspace>` `<BS>` -> `\x7f`
//! * `<Space>`        -> ` `
//! * `<C-x>` (any letter) -> Ctrl+X (`\x01`..=`\x1a`)
//! * Arrows: `<Up>` `<Down>` `<Left>` `<Right>` -> CSI sequences
//! * Function: `<F1>`..`<F12>`
//! * `<Home>` `<End>` `<PageUp>` `<PageDown>` `<Delete>` `<Insert>`
//!
//! Unknown names are an error rather than silently passed through —
//! the model should get a clear message back instead of mysterious
//! literal `<Unknown>` showing up on the remote.

use anyhow::{Result, bail};

pub fn parse(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'<' {
            // Look for a matching '>'. Bail if the input is "naked '<'" —
            // that'd be ambiguous with a literal less-than, so we err on
            // the side of strictness for v1.
            let close = match input[i..].find('>') {
                Some(off) => i + off,
                None => bail!("unterminated key escape starting at byte {i}"),
            };
            let name = &input[i + 1..close];
            out.extend_from_slice(&named_key(name)?);
            i = close + 1;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Ok(out)
}

fn named_key(name: &str) -> Result<Vec<u8>> {
    // Lowercase comparisons; the "C-" prefix preserves case so we
    // separate it.
    if let Some(rest) = name.strip_prefix("C-").or_else(|| name.strip_prefix("c-")) {
        if rest.len() != 1 || !rest.chars().next().unwrap().is_ascii_alphabetic() {
            bail!("Ctrl key must be a single ASCII letter, got <C-{rest}>");
        }
        let c = rest.chars().next().unwrap().to_ascii_uppercase();
        return Ok(vec![(c as u8) - b'A' + 1]);
    }
    let lower = name.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "enter" | "cr" | "return" => b"\r".to_vec(),
        "lf" | "newline" => b"\n".to_vec(),
        "tab" => b"\t".to_vec(),
        "esc" | "escape" => b"\x1b".to_vec(),
        "backspace" | "bs" => b"\x7f".to_vec(),
        "space" => b" ".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "pageup" | "pgup" => b"\x1b[5~".to_vec(),
        "pagedown" | "pgdn" | "pgdown" => b"\x1b[6~".to_vec(),
        "delete" | "del" => b"\x1b[3~".to_vec(),
        "insert" | "ins" => b"\x1b[2~".to_vec(),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => b"\x1b[15~".to_vec(),
        "f6" => b"\x1b[17~".to_vec(),
        "f7" => b"\x1b[18~".to_vec(),
        "f8" => b"\x1b[19~".to_vec(),
        "f9" => b"\x1b[20~".to_vec(),
        "f10" => b"\x1b[21~".to_vec(),
        "f11" => b"\x1b[23~".to_vec(),
        "f12" => b"\x1b[24~".to_vec(),
        other => bail!("unknown key name <{other}>"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_text_passes_through() {
        assert_eq!(parse("hello world").unwrap(), b"hello world");
    }
    #[test]
    fn enter_is_cr() {
        assert_eq!(parse("hi<Enter>").unwrap(), b"hi\r");
    }
    #[test]
    fn ctrl_c_is_0x03() {
        assert_eq!(parse("<C-c>").unwrap(), vec![0x03]);
        assert_eq!(parse("<C-C>").unwrap(), vec![0x03]);
        assert_eq!(parse("<c-z>").unwrap(), vec![0x1a]);
    }
    #[test]
    fn arrow_is_csi() {
        assert_eq!(parse("<Up>").unwrap(), b"\x1b[A");
    }
    #[test]
    fn unknown_key_errs() {
        assert!(parse("<Wat>").is_err());
    }
    #[test]
    fn unterminated_brace_errs() {
        assert!(parse("hello <Enter ").is_err());
    }
    #[test]
    fn mix_literal_and_named() {
        assert_eq!(parse("vim foo<Enter>:q!<Enter>").unwrap(), b"vim foo\r:q!\r");
    }
}
