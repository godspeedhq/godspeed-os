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

Regenerating the site is automatic: the **`docs` GitHub Action** (`.github/workflows/pages.yml`) runs
`website/sync.sh` on the runner and deploys to GitHub Pages on every push to `main` that touches a
source doc, the SDK, or the site. That build **is** the reconcile step that keeps the derived view
honest - edit a source doc, push, and the site follows. Run `bash website/sync.sh` locally (below)
only to **preview** the exact same output before you push.

## Build locally - one command for both docs and website

`website/sync.sh` regenerates the **whole site in one command**: the mdBook narrative (from the
`{{#include}}` source-of-truth stubs) *and* the SDK rustdoc under `/api`. It is exactly what runs in
CI (`pages.yml`), so what you preview locally is what you publish.

```bash
# one-time: install mdBook (need python3 too, for the link-fixup preprocessor)
cargo install mdbook --version 0.4.40

# regenerate everything into website/book/ (+ /api) - run from anywhere:
bash website/sync.sh
```

For fast iteration on the narrative alone, use mdBook directly from this directory:

```bash
mdbook build      # -> website/book/   (narrative only, no /api)
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

## Publishing (automatic, via GitHub Actions)

The **`docs` workflow** (`.github/workflows/pages.yml`) builds and deploys the site to GitHub Pages
automatically - no `gh-pages` branch, no generated HTML committed to `main`. It runs `website/sync.sh`
on the runner and publishes `website/book/` through the official Pages deployment
(`upload-pages-artifact` + `deploy-pages`).

**One-time setup:** in the repo, **Settings -> Pages -> Build and deployment -> Source: "GitHub
Actions"**. Without it, the deploy step has nowhere to publish.

After that:

1. Edit a source doc (or the SDK, or the site) and push to `main` - the workflow rebuilds and
   republishes. It is scoped to run only when `website/`, `docs/`, `sdk/`, or an included root doc
   changes, so ordinary code pushes do not spend Actions minutes here.
2. For the **first** deploy (right after setting the source), trigger it by hand: the Actions tab ->
   "docs" -> Run workflow.
3. To preview before pushing: `bash website/sync.sh`, then open `website/book/index.html` (or
   `mdbook serve` for live reload).
