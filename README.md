# tx

A vim-style tree file explorer for the terminal. Includes a simple file preview and text editor. 

Runs on Linux and macOS.

## Usage

```sh
tx [PATH]
```

To have the shell `cd` to where you quit, add to your shell's rc file:

```sh
eval "$(tx --init zsh)"   # or bash
tx --init fish | source   # fish
```

`q` quits and changes directory, `<c-c>` quits without changing it.

## Keys

Press `?` for the full list. The main ones:

| Keys | Action |
|------|--------|
| `h` `j` `k` `l`, arrows | leave / down / up / enter |
| `gg` `G` `<c-d>` `<c-u>` | first / last / half page |
| `J` `K` | scroll the preview |
| `/` `n` `N`, `f` `F` `;` `,` | search, jump by first letter |
| `<cr>` | open folder, or edit the file in the built-in editor |
| `i` | open in your own editor or the desktop app |
| `d` `y` `x` + motion (`dd`, `yy`) | trash / copy / cut |
| `v`, `<space>` | visual range, toggle selection |
| `p` `P` | paste here / into the directory under the cursor |
| `r` `o` | rename / new file (folder if it ends in `/`) |
| `u` `<c-r>` | undo / redo file operations |
| `m{a-z}` `'{a-z}` | marks (uppercase ones persist) |
| `<c-o>` `<tab>` | jump history |
| `zh` | toggle dotfiles |
| `:` | command line: `cd`, `mkdir`, `touch`, `chmod 644`, `set hidden`, `marks`, `images`, `q` |

## Configuration

`~/.config/tx/config.toml` (or `$XDG_CONFIG_HOME/tx/config.toml`), every key optional:

```toml
editor = ["nvim", "code --wait"]  # first one on PATH wins; default nvim, nano
show_hidden = false
mouse = true                      # false leaves the mouse to the terminal for selecting text
images = "auto"                   # auto, kitty, sixel, iterm2, blocks, halfblocks, off
colors = "auto"                   # auto, truecolor, 256, 16, none

[keys]
"gh" = "leave"                    # command names: see COMMANDS in src/keys.rs
"zh" = ""                         # empty unbinds
```

Marks are saved in `~/.local/state/tx/marks` (or `$XDG_STATE_HOME/tx/marks`).

