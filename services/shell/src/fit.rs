// SPDX-License-Identifier: GPL-2.0-only
//! A progress frame made to fit the console (`godspeed_answer` in `main.rs`): its words on one line, its bar
//! on the line under them, as wide as the console.
//!
//! **Separate so it can be tested on the host**, as `ask`'s `know.rs` is: it names nothing outside `core`, and
//! `scripts/host_test_check.py` compiles it on its own. QEMU's test machines answer no console size, so this
//! is the only place the narrowing runs off a board.
//!
//! **Why two lines.** With the bar inside the words, a 102-column console (the T630's TV) had no room for it:
//! narrowed to fit, it jumped between sizes and vanished, and with the words shortened it still had nine cells
//! when the time left was long (2026-10-19). Under the words it has the whole width, whatever they say.

/// A frame taken apart: the words with the bar and the two spaces before it taken out, and the bar's share,
/// `(filled, cells)`, when there was one. A frame with no bar comes back whole.
pub(crate) fn split_bar<'a>(text: &'a str, buf: &'a mut [u8; 192]) -> (&'a str, Option<(usize, usize)>) {
    let b = text.as_bytes();
    let open = b.iter().position(|&c| c == b'[');
    let bar = open.and_then(|o| {
        let len = b[o + 1..].iter().position(|&c| c != b'#' && c != b'-')?;
        (len > 0 && b.get(o + 1 + len) == Some(&b']')).then_some((o, len))
    });
    let Some((o, len)) = bar else { return (text, None) };
    let filled = b[o + 1..o + 1 + len].iter().filter(|&&c| c == b'#').count();
    let before = &b[..o];
    let before = before.strip_suffix(b"  ").unwrap_or(before);
    let after = &b[o + 2 + len..];
    let mut n = 0usize;
    for part in [before, after] {
        let take = part.len().min(buf.len() - n);
        buf[n..n + take].copy_from_slice(&part[..take]);
        n += take;
    }
    let buf: &'a [u8; 192] = buf;
    (core::str::from_utf8(&buf[..n]).unwrap_or(""), Some((filled, len)))
}

/// `text` cut to `width` columns, at a character boundary.
pub(crate) fn cut(text: &str, width: usize) -> &str {
    let mut end = text.len().min(width);
    while !text.is_char_boundary(end) { end -= 1; }
    &text[..end]
}

/// The bar line: two spaces, then `[###-----]` filling `width` columns with the frame's share, `filled` of
/// `cells`, kept. `width` of `usize::MAX` (no console answered) draws the frame's own cells.
pub(crate) fn bar_line(filled: usize, cells: usize, width: usize, buf: &mut [u8; 192]) -> &str {
    let keep = if width == usize::MAX { cells } else { width.saturating_sub(4) };
    let keep = keep.min(buf.len() - 4);
    let lit = if cells == 0 { 0 } else { filled * keep / cells };
    buf[0] = b' ';
    buf[1] = b' ';
    buf[2] = b'[';
    for i in 0..keep { buf[3 + i] = if i < lit { b'#' } else { b'-' }; }
    buf[3 + keep] = b']';
    core::str::from_utf8(&buf[..4 + keep]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::{bar_line, cut, split_bar};

    const TEN: &str = "  downloading 10mb.bin  [######----------------------]  21%  2.0 / 9.5 MiB  234 KiB/s  about 37 s left   [q] quit";

    #[test]
    fn the_bar_is_taken_out_of_the_words() {
        let mut b = [0u8; 192];
        let (words, bar) = split_bar(TEN, &mut b);
        assert_eq!(words, "  downloading 10mb.bin  21%  2.0 / 9.5 MiB  234 KiB/s  about 37 s left   [q] quit");
        assert_eq!(bar, Some((6, 28)));
    }

    #[test]
    fn a_frame_with_no_bar_comes_back_whole() {
        let t = "  downloading the snapshot  40%  10 / 25 KiB  about 1 s left   [q] quit";
        let mut b = [0u8; 192];
        assert_eq!(split_bar(t, &mut b), (t, None));
    }

    #[test]
    fn q_quit_is_not_taken_for_a_bar() {
        let t = "  fetching  40%   [q] quit";
        let mut b = [0u8; 192];
        assert_eq!(split_bar(t, &mut b).1, None);
    }

    #[test]
    fn the_bar_fills_the_console_with_its_share_kept() {
        let mut b = [0u8; 192];
        let line = bar_line(6, 28, 101, &mut b);
        assert_eq!(line.len(), 101);
        assert_eq!(line.bytes().filter(|&c| c == b'#').count(), 6 * 97 / 28);
        assert!(line.starts_with("  [#") && line.ends_with("-]"), "{line}");
    }

    #[test]
    fn with_no_console_the_bar_keeps_its_own_cells() {
        let mut b = [0u8; 192];
        assert_eq!(bar_line(28, 28, usize::MAX, &mut b), "  [############################]");
    }

    #[test]
    fn a_console_wider_than_the_buffer_is_held_to_it() {
        let mut b = [0u8; 192];
        assert_eq!(bar_line(1, 2, 1000, &mut b).len(), 192);
    }

    #[test]
    fn words_too_wide_are_cut_to_the_width() {
        assert_eq!(cut(TEN, 40).len(), 40);
        assert_eq!(cut("abc", 40), "abc");
    }
}
