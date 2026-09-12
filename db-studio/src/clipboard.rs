//! System clipboard via OSC 52 (db-studio#42): writes the terminal
//! escape sequence directly to stdout rather than shelling out to
//! `pbcopy`/`xclip`/`wl-copy` -- works the same over SSH (the terminal
//! emulator, not the remote shell, owns the clipboard), and needs no
//! platform-specific dependency. What gets copied is always the pane's
//! plain text (no ANSI styling, no box-drawing border characters) --
//! "copy without formatting" is true by construction here, not
//! something the terminal's own text-selection could promise (a mouse
//! drag-select in the terminal *does* pick up border glyphs).

use std::io::Write;

/// Copies `text` to the system clipboard via `OSC 52`. Best-effort: a
/// terminal that doesn't support OSC 52 (or has it disabled) silently
/// ignores the sequence -- there's no ack to fail on, so this can't
/// report success or failure either way.
pub fn copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    // ESC ] 52 ; c ; <base64> BEL -- `c` targets the system clipboard
    // (as opposed to `p`, the primary selection some terminals track
    // separately). BEL (`\x07`) terminates more widely than ST
    // (`\x1b\\`) across common terminal emulators.
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A minimal standard-alphabet base64 encoder (with `=` padding) --
/// OSC 52's payload is always base64, and pulling in a whole crate for
/// this one encode felt like the wrong trade for a handful of lines.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk.first().copied().unwrap_or(0);
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n =
            (u32::from(b0) << 16) | (u32::from(b1.unwrap_or(0)) << 8) | u32::from(b2.unwrap_or(0));
        let sextet = |shift: u32| ALPHABET.get(((n >> shift) & 0x3F) as usize).copied();
        out.push(sextet(18).unwrap_or(b'A') as char);
        out.push(sextet(12).unwrap_or(b'A') as char);
        out.push(if b1.is_some() {
            sextet(6).unwrap_or(b'A') as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            sextet(0).unwrap_or(b'A') as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
