# db-studio
 A rich database studio tui client

## Configuration

`db-studio` reads an optional `config.toml` from
`$XDG_CONFIG_HOME/db-studio/config.toml` (or `$HOME/.config/db-studio/config.toml`
if `$XDG_CONFIG_HOME` isn't set), following the XDG Base Directory Spec --
same convention as [`loglume`](https://github.com/t-rust-db/loglume)'s own
config file. Run `db-studio config` to print the resolved path and its
current contents.

### `[theme]`

Overrides individual colors of the default catppuccin Mocha palette. Every
key is optional -- an unset key keeps rendering exactly as it does with no
config file at all. Values are `#rrggbb` hex strings (the leading `#` is
optional).

```toml
[theme]
text = "#cdd6f4"
subtext = "#a6adc8"
base = "#1e1e2e"
accent = "#cba6f7"
error = "#f38ba8"
keyword = "#cba6f7"
string_literal = "#a6e3a1"
number_literal = "#fab387"
identifier = "#89b4fa"
header = "#f9e2af"
selection_bg = "#45475a"
```

The values above are the current catppuccin Mocha defaults themselves --
copy this block and change only the keys you want to override.
