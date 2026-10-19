// SPDX-License-Identifier: GPL-2.0-only
//! A progress line made to fit the console (`godspeed_answer` in `main.rs`).
//!
//! **Separate so it can be tested on the host**, as `ask`'s `know.rs` is: it names nothing outside `core`, and
//! `scripts/host_test_check.py` compiles it on its own. QEMU's test machines answer no console size, so this
//! is the only place the narrowing runs off a board.

/// A progress line made to fit `width` columns. The bar gives way first: `[###-----]` is redrawn narrower
/// with the same share filled, or left out when fewer than eight cells would remain, so the numbers, the
/// time left and `[q] quit` stay. Only a line still too wide without its bar is cut.
pub(crate) fn fit_progress<'a>(text: &'a str, width: usize, buf: &'a mut [u8; 192]) -> &'a str {
    if text.len() <= width { return text; }
    let b = text.as_bytes();
    let open = b.iter().position(|&c| c == b'[');
    let bar = open.and_then(|o| {
        let len = b[o + 1..].iter().position(|&c| c != b'#' && c != b'-')?;
        (len > 0 && b.get(o + 1 + len) == Some(&b']')).then_some((o, len))
    });
    let mut n = 0usize;
    let mut put = |s: &[u8], buf: &mut [u8; 192]| {
        let take = s.len().min(buf.len() - n);
        buf[n..n + take].copy_from_slice(&s[..take]);
        n += take;
    };
    match bar {
        Some((o, len)) => {
            let over = text.len() - width;
            put(&b[..o], buf);
            if len >= over + 8 {
                let keep = len - over;
                let filled = b[o + 1..o + 1 + len].iter().filter(|&&c| c == b'#').count() * keep / len;
                put(b"[", buf);
                for i in 0..keep { put(if i < filled { b"#" } else { b"-" }, buf); }
                put(b"]", buf);
                put(&b[o + 2 + len..], buf);
            } else {
                // Without the bar, and the two spaces that set it apart.
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

    fn fit(t: &str, w: usize) -> String { let mut b = [0u8; 192]; fit_progress(t, w, &mut b).to_string() }

    const TEN: &str = "  downloading 10mb.bin  [######----------------------]  21%  2.0 MiB / 9.5 MiB  234 KiB/s  4 s, about 37 s left   [q] quit";

    #[test]
    fn a_line_that_fits_is_unchanged() {
        assert_eq!(fit(TEN, 200), TEN);
    }

    #[test]
    fn the_bar_narrows_first_with_its_share_kept() {
        let r = fit(TEN, TEN.len() - 10);
        assert_eq!(r.len(), TEN.len() - 10);
        assert!(r.contains("[###---------------]"), "{r}"); // 6 of 28 is 3.9 of 18, kept as 3
        assert!(r.ends_with("about 37 s left   [q] quit"), "{r}");
    }

    #[test]
    fn a_bar_under_eight_cells_is_left_out() {
        let r = fit(TEN, 99);
        assert_eq!(r, "  downloading 10mb.bin  21%  2.0 MiB / 9.5 MiB  234 KiB/s  4 s, about 37 s left   [q] quit");
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
