# website/ - the GodspeedOS documentation site

This directory builds the public documentation site with [mdBook](https://rust-lang.github.io/mdBook/).

## The one rule: this site owns almost no content

The site is a **derived view** of the repository, not a second copy of it. Commandment III ("do not
duplicate truth") applies to docs as much as to code. Concretely:

- Every chapter that shows the constitution, a commandment, the glossary, the almanac, or a design
  doc is a two-line `{{#include ../../<file>}}` **stub**. The content lives once, in its real home
  (`CLAUDE.md`, `COMMANDMENTS.md`, `GLOSSARY.md`, `docs/`, `milestones/ALMANAC.md`). mdBook pulls it
  in at build time.
- **Do not paste content into a stub page.** If a design doc is wrong, fix the doc in `docs/` and
  rebuild - the site follows automatically. The source wins; the site is subordinate.
- The only files that hold original content are the ones the site legitimately owns: `SUMMARY.md`
  (navigation), `introduction.md` (a thin landing page that frames and links, never restates),
  `gallery.md` (framing around captured screenshots), and the images under `src/images/`.

The `docs` GitHub Action (`.github/workflows/pages.yml`) rebuilds and republishes on every push to
`main` that touches a source file. That push-triggered rebuild **is** the reconcile step that keeps
the derived view honest.

## Build locally

```bash
# one-time: install mdBook (matches the pinned CI version)
cargo install mdbook --version 0.4.40

# from this directory
mdbook build      # -> website/book/
mdbook serve      # live preview at http://localhost:3000
```

## Screenshots

The gallery images are captured from the real OS in QEMU by the scripts in `capture/`. `fb_shot.py`
boots `build/os.img` headless with an emulated GPU, grabs the framebuffer via the QEMU monitor's
`screendump` (PPM), and converts it to PNG with the Python standard library; `fb_capture.py` also
drives the shell over COM1 serial to reach a chosen state (observe, chaos) before the screendump.
Copy the result into `src/images/`.

## Cross-doc links inside included files

`CLAUDE.md` and the `docs/` files link to each other with repo-relative paths (`./COMMANDMENTS.md`,
`docs/ahci.md`). The `link-fixup.py` mdBook preprocessor rewrites these at build time so they resolve
within the book: a link to a repo doc that is a book page becomes the page URL; a link to a repo file
that is not a page (`README.md`, `examples/`) becomes a GitHub URL. The source docs are never edited.
In-page anchors already resolve (each included file is one page). Set `GODSPEED_REPO_URL` to change
the GitHub base used for non-page links (defaults to the current repo).

## Launch checklist (do once, when the org/repo names are final)

- Set `git-repository-url` in `book.toml`.
- Repo Settings -> Pages -> Source = "GitHub Actions".
- Confirm the first `workflow_dispatch` run deploys, then let push-to-`main` drive it.
