# Ready-made themes

Copy-and-paste color themes for koshi. Each file is a complete theme — pick one,
copy it into your config directory, and name it in `koshi.kdl`.

## How to use one

1. Copy the file into a `themes/` folder inside your config directory:

   | Platform | Where that is |
   |---|---|
   | Linux | `~/.config/koshi/themes/` |
   | macOS | `~/Library/Application Support/koshi/themes/` |
   | Windows | `%APPDATA%\koshi\config\themes\` |

2. Name it in `koshi.kdl`, without the `.kdl`:

   ```kdl
   theme "dracula"
   ```

3. Restart koshi.

Keep as many of these as you like side by side — only the one `koshi.kdl` names
is used. Editing a copy is encouraged: any color you delete falls back to
koshi's built-in value, so you can keep just the handful you want to change.

See [config-docs/theme.md](../config-docs/theme.md) for what each color role
does and how to build a theme from scratch.

## What's here

**Dark**

| Theme | File | Source |
|---|---|---|
| Ayu Dark | `ayu-dark.kdl` | <https://github.com/ayu-theme/ayu-vim> |
| Ayu Mirage | `ayu-mirage.kdl` | <https://github.com/ayu-theme/ayu-vim> |
| Catppuccin Frappé | `catppuccin-frappe.kdl` | <https://catppuccin.com> |
| Catppuccin Macchiato | `catppuccin-macchiato.kdl` | <https://catppuccin.com> |
| Catppuccin Mocha | `catppuccin-mocha.kdl` | <https://catppuccin.com> |
| Dracula | `dracula.kdl` | <https://draculatheme.com> |
| Everforest Dark | `everforest-dark.kdl` | <https://github.com/sainnhe/everforest> |
| Gruvbox Dark | `gruvbox-dark.kdl` | <https://github.com/morhetz/gruvbox> |
| Kanagawa | `kanagawa.kdl` | <https://github.com/rebelot/kanagawa.nvim> |
| Monokai | `monokai.kdl` | <https://monokai.pro> |
| Nord | `nord.kdl` | <https://www.nordtheme.com> |
| One Dark | `one-dark.kdl` | <https://github.com/joshdick/onedark.vim> |
| Rosé Pine | `rose-pine.kdl` | <https://rosepinetheme.com> |
| Rosé Pine Moon | `rose-pine-moon.kdl` | <https://rosepinetheme.com> |
| Solarized Dark | `solarized-dark.kdl` | <https://ethanschoonover.com/solarized> |
| SynthWave '84 | `synthwave-84.kdl` | <https://github.com/robb0wen/synthwave-vscode> |
| Tokyo Night | `tokyo-night.kdl` | <https://github.com/folke/tokyonight.nvim> |
| Tokyo Night Storm | `tokyo-night-storm.kdl` | <https://github.com/folke/tokyonight.nvim> |
| Zenburn | `zenburn.kdl` | <https://github.com/jnurmine/Zenburn> |

**Light**

| Theme | File | Source |
|---|---|---|
| Catppuccin Latte | `catppuccin-latte.kdl` | <https://catppuccin.com> |
| Gruvbox Light | `gruvbox-light.kdl` | <https://github.com/morhetz/gruvbox> |
| One Light | `one-light.kdl` | <https://github.com/navarasu/onedark.nvim> |
| Rosé Pine Dawn | `rose-pine-dawn.kdl` | <https://rosepinetheme.com> |
| Solarized Light | `solarized-light.kdl` | <https://ethanschoonover.com/solarized> |
| Tokyo Night Day | `tokyo-night-day.kdl` | <https://github.com/folke/tokyonight.nvim> |

## How these were built

Every hex value is taken from the upstream project's own published palette,
linked above. These are koshi's own interpretation of each palette — upstream
projects define syntax-highlighting colors, and koshi colors its own chrome
(borders, the tab ribbon, the key-hint bar), so each theme maps the palette onto
koshi's roles rather than copying a syntax scheme.

Two rules shape every mapping, both from the guidance in
[theme.md](../config-docs/theme.md):

- **The ramp has to work twice.** `ramp-start` and `ramp-end` are used both as
  *text* on the bar and as the *background* of a key block, so on a dark theme
  both ends are light, and on a light theme both ends are dark.
- **`accent` sits outside the ramp.** It marks the keys you have already pressed
  in a half-typed shortcut, so it is picked to contrast with the ramp rather
  than blend into it.

## Adding one

Send a pull request. A theme file needs `version 1` and a `colors` block setting
all thirteen roles, and every value must come from the upstream project's
published palette. The test suite parses every file in this folder, so a typo or
an unknown color role fails CI.
