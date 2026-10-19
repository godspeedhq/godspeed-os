// SPDX-License-Identifier: GPL-2.0-only
//! A progress line made to fit the console (`godspeed_answer` in `main.rs`).
//!
//! **Separate so it can be tested on the host**, as `ask`'s `know.rs` is: it names nothing outside `core`, and
//! `scripts/host_test_check.py` compiles it on its own. QEMU's test machines answer no console size, so this
//! is the only place the narrowing runs off a board.

/// The fewest cells a bar is drawn with; a narrower one is left out.
const BAR_MIN: usize = 8;
/// Cells a narrowed bar leaves free beside it, because the numbers after it grow as the transfer goes on (a
/// rate gains a digit, the time left appears).
const BAR_SLACK: usize = 2;

/// A progress line made to fit `width` columns. The bar gives way first: `[###-----]` is redrawn narrower
/// with the same share filled, or left out when fewer than eight cells would remain, so the numbers, the
/// time left and `[q] quit` stay. Only a line still too wide without its bar is cut.
///
/// **One size for the whole command.** `held` is the bar's width so far (`usize::MAX` before the first
/// frame), and a bar is never drawn wider than it was: fitted afresh to each frame, it jumped between sizes
/// and vanished whenever a frame's numbers came out a digit longer, which on the T630's 102 columns read as a
/// tiny bar that kept disappearing (2026-10-19).
pub(crate) fn fit_progress<'a>(text: &'a str, width: usize, held: &mut usize, buf: &'a mut [u8; 192]) -> &'a str {
    let b = text.as_bytes();
    let open = b.iter().position(|&c| c == b'[');
    let bar = open.and_then(|o| {
        let len = b[o + 1..].iter().position(|&c| c != b'#' && c != b'-')?;
        (len > 0 && b.get(o + 1 + len) == Some(&b']')).then_some((o, len))
    });
    let keep = match bar {
        Some((_, len)) => {
            let room = width.saturating_sub(text.len() - len);
            let fits = if room >= len { len } else { room.saturating_sub(BAR_SLACK) };
            fits.min(*held)
        }
        None => 0,
    };
    if text.len() <= width && bar.map_or(true, |(_, len)| keep == len) {
        if let Some((_, len)) = bar { *held = len; }
        return text;
    }
    let mut n = 0usize;
    let mut put = |s: &[u8], buf: &mut [u8; 192]| {
        let take = s.len().min(buf.len() - n);
        buf[n..n + take].copy_from_slice(&s[..take]);
        n += take;
    };
    match bar {
        Some((o, len)) => {
            put(&b[..o], buf);
            if keep >= BAR_MIN {
                *held = keep;
                let filled = b[o + 1..o + 1 + len].iter().filter(|&&c| c == b'#').count() * keep / len;
                put(b"[", buf);
                for i in 0..keep { put(if i < filled { b"#" } else { b"-" }, buf); }
                put(b"]", buf);
                put(&b[o + 2 + len..], buf);
            } else {
                // Without the bar, and the two spaces that set it apart. `held` is left alone, so a frame
                // with room again draws it at the size it had.
                let rest = &b[o + 2 + len..];
                put(rest.strip_prefix(b"  ").unwrap_or(rest), buf);
            }
        }
        None => put(b, buf),
    }
    let mut end = n.min(width);
    let buf: &'a [u8; 192] = buf;
    let s = core::str::from_utf8(&buf[..n]).unwrap_or("");
    while !s.is_char_boundary(end) { end -= 1; }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::fit_progress;

    fn fit(t: &str, w: usize) -> String { let mut h = usize::MAX; fit_held(t, w, &mut h) }
    fn fit_held(t: &str, w: usize, h: &mut usize) -> String { let mut b = [0u8; 192]; fit_progress(t, w, h, &mut b).to_string() }

    const TEN: &str = "  downloading 10mb.bin  [######----------------------]  21%  2.0 MiB / 9.5 MiB  234 KiB/s  4 s, about 37 s left   [q] quit";

    #[test]
    fn a_line_that_fits_is_unchanged() {
        assert_eq!(fit(TEN, 200), TEN);
    }

    #[test]
    fn the_bar_narrows_first_with_its_share_kept_and_room_to_grow() {
        let r = fit(TEN, TEN.len() - 10);
        assert_eq!(r.len(), TEN.len() - 12); // two cells left for the numbers to grow into
        assert!(r.contains("[###-------------]"), "{r}"); // 6 of 28 is 3.4 of 16, kept as 3
        assert!(r.ends_with("about 37 s left   [q] quit"), "{r}");
    }

    #[test]
    fn the_bar_keeps_one_size_when_a_frame_is_shorter() {
        let mut h = usize::MAX;
        let w = TEN.len() - 10;
        let first = fit_held(TEN, w, &mut h);
        let short = "  downloading 10mb.bin  [##########################--]  95%  9.0 MiB / 9.5 MiB  700 KiB/s  done   [q] quit";
        let second = fit_held(short, w, &mut h);
        let cells = |s: &str| s.bytes().filter(|&c| c == b'#' || c == b'-').count();
        assert_eq!(cells(&first), 16);
        assert_eq!(cells(&second), 16, "{second}");
    }

    #[test]
    fn a_bar_under_eight_cells_is_left_out_and_comes_back_at_its_size() {
        let mut h = usize::MAX;
        assert_eq!(fit_held(TEN, TEN.len() - 10, &mut h).len(), TEN.len() - 12);
        let r = fit_held(TEN, 99, &mut h);
        assert_eq!(r, "  downloading 10mb.bin  21%  2.0 MiB / 9.5 MiB  234 KiB/s  4 s, about 37 s left   [q] quit");
        assert_eq!(fit_held(TEN, TEN.len() - 10, &mut h).len(), TEN.len() - 12);
    }

    #[test]
    fn a_line_too_wide_even_without_its_bar_is_cut_to_the_width() {
        let r = fit(TEN, 40);
        assert_eq!(r.len(), 40);
        assert!(!r.contains('['), "{r}");
    }

    #[test]
    fn a_line_with_no_bar_is_cut_and_q_quit_is_not_taken_for_one() {
        let t = "  downloading the snapshot  40%  10 / 25 KiB  1 s   [q] quit";
        assert_eq!(fit(t, 30), &t[..30]);
    }
}
