<!-- SPDX-License-Identifier: Apache-2.0 -->
# gsh syntax highlighting for VS Code

Real syntax highlighting for **gsh** (`.gsh`) - the GodspeedOS shell scripting language
(`docs/scripting.md`). Stop borrowing bash's colors: bash's keywords (`function`, `elif`,
`esac`), operators, and `$(...)` semantics don't match gsh, so bash highlighting mis-colors
real scripts. This colors gsh *as gsh*.

It is a **grammar, not a language server** - it gives colors, bracket matching, `#`-comment
toggling, and auto-closing quotes/braces. It does **not** do autocomplete, go-to-definition,
or live error squiggles (that would be a separate LSP; gsh already fails *loudly* at runtime,
so most of what diagnostics would catch is caught when you run the script).

## What it highlights

- **Control keywords** - `let` / `mut`, `if` / `else`, `switch`, `for` / `in`, `loop`,
  `break` / `continue`, `fn`, `return`, `defer`, `import` / `from` / `as`, `range`
- **Builtins** - `echo` `read` `write` `assert` `input` `mkdir` `ls` `cd` ... and modifiers
  (`secret`, `sealed`, `append`, `prepend`, `recursive`, `save`, ...)
- **Pipe stages** - `where` `select` `sort` `count` `sum`/`min`/`max`/`avg` `to` `match` ...
- **Variables** - `$name`, params `$1..$9` / `$@` / `$#` / `$0`
- **Capture** - `$( … )` (highlighted recursively as gsh)
- **Strings** - `"…"` (with `$var` interpolation) vs `'…'` (literal)
- **Constants** - `Ok` / `Err`, `true` / `false`, result variants (`FileNotFound`, `Denied`,
  `AssertFailed`, ...), the `switch` default `_`
- **Operators** - `== != < > <= >=`, `+ - * / %`, the pipe `|`, assignment `=`
- **Comments** - `#` to end of line (respecting quotes, so a `#` inside a string is literal)
- **Function names** - the name after `fn`

## File icon

`.gsh` files carry a wordmark icon (`icons/gsh.svg`) - "gsh" outlined from Consolas Bold on the
terminal-dark ground with the shell-prompt green bar - contributed via `contributes.languages[].icon`.
It shows in the explorer, tabs, and breadcrumbs under any file-icon theme that uses language icons
(VS Code's default *Seti* theme does). The letters are real vector paths, not live SVG text, so they
render identically on every OS and stay crisp at 16px. The eight directions it was picked from are the
numbered files in `icons/`.

## Install

The extension is self-contained (no build step - it is pure grammar + config).

**A. Drop it in your extensions folder (quickest).** Copy the whole `gsh-vscode` folder to
- Linux/macOS: `~/.vscode/extensions/gsh-vscode`
- Windows: `%USERPROFILE%\.vscode\extensions\gsh-vscode`

then **fully quit and reopen VS Code**. A hand-copied extension is only picked up on startup, and a
plain `Developer: Reload Window` is sometimes not enough - a full restart always is.

**B. Package a `.vsix` and install it** (needs [`vsce`](https://github.com/microsoft/vscode-vsce)) - a
single file you can install anywhere, or later publish to the Marketplace:
```
cd editors/gsh-vscode
npx @vscode/vsce package          # -> gsh-0.1.0.vsix
code --install-extension gsh-0.1.0.vsix
```

## Verify it worked

1. Open any `.gsh` file - e.g. `scripts/smoke.gsh` or `scripts/selfcheck.gsh`.
2. Look at the **bottom-right of the status bar**: it should read **`gsh`** (not "Shell Script" or
   "Plain Text").
3. Keywords, builtins, `$vars`, `"…"` / `'…'` strings, and `#` comments should all be colored.

## If `.gsh` still shows the wrong language

- Click the language name in the status bar -> **"Configure File Association for '.gsh'"** -> pick
  **gsh**, or
- add to your `settings.json` (per-workspace, or global once the extension is loaded):
  ```json
  { "files.associations": { "*.gsh": "gsh" } }
  ```
- if a token looks uncolored, your color theme simply does not style that TextMate scope - the grammar
  is fine; try another theme.

## Other editors

The grammar (`syntaxes/gsh.tmLanguage.json`) is a portable **TextMate grammar** - the same file
works in Sublime Text, Zed, and GitHub's Linguist. Point your editor's grammar loader at it and
map `.gsh` to scope `source.gsh`.

## Later: a language server?

Only if the ergonomics demand it. gsh's interpreter already pre-scans functions, tracks the
variable table, and fails loudly on undefined vars / redeclares / unbalanced braces - so "run it
and read the loud error" covers most of what an LSP's diagnostics would. If autocomplete and
go-to-definition become worth it, an LSP would be a separate host program (Rust, like `osdev`)
reusing the same parser. Grammar first (§26.2).
