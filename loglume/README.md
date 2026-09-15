# loglume
A rust desktop log client

## Configuration

loglume reads `config.toml` from `$XDG_CONFIG_HOME/loglume/config.toml`,
falling back to `$HOME/.config/loglume/config.toml` if `XDG_CONFIG_HOME`
is unset (per the XDG Base Directory Spec, applied on every platform).
If the file doesn't exist, loglume runs with defaults — no file is
required.

Run `loglume config` to print the resolved config file's path and its
current contents.

### Schema

```toml
[tui]
# Fallback for --scope when it isn't passed on the command line.
default_scope = "1h"

# Reserved for future key-remapping support; not yet applied by the TUI.
[tui.keybindings]

# TUI color overrides. Every key is optional -- unset keys fall back to
# the built-in Catppuccin Mocha defaults shown below.
[tui.theme]
border_focused = "#89b4fa"  # focused pane's border (blue)
highlight_bg   = "#f9e2af"  # --highlight match background (yellow)
highlight_fg   = "#1e1e2e"  # --highlight match text (base)
detail_dim     = "#6c7086"  # expanded detail field lines (overlay1)
status_error   = "#f38ba8"  # filter/highlight compile-error text (red)
zebra_bg       = "#313244"  # alternating row background (surface0)

# Saved filters/queries, referenced on the command line as "@name".
[filters]
myerr = "SELECT * FROM log WHERE severity >= 13"
```

### Saved filters

`--save-filter <name>` saves the current invocation's resolved filter/SQL
under `<name>` and writes it back to `config.toml`:

```bash
loglume "severity >= WARN" --save-filter myerr app.log
```

Reuse it later with the `@name` syntax in place of a filter expression:

```bash
loglume "@myerr" app.log
```

## Highlighting

`--filter` (the positional filter argument) *restricts* what's shown.
`--highlight <expr>` *annotates* instead: matching lines are marked (bold,
highlighted background) without hiding the rest — the CLI equivalent of
`grep --color` layered on top of the existing filter.

```bash
# Show everything at INFO or above, but make ERROR-or-worse lines stand out
loglume "severity >= INFO" --highlight "severity >= ERR" app.log
```

`--highlight` accepts loglume's short forms (`severity`/`facility`) or any
boolean expression `db-core` understands (e.g. `message LIKE '%oom%'`).

The TUI (`--tui`) has the same distinction as a first-class concept, per
pane: a highlight bar sits below the filter bar.

| Key | Action |
|-----|--------|
| `?` | Edit the highlight expression (`Enter` to apply, `Esc` to cancel) |
| `h` | Toggle highlight rendering on/off without clearing the expression |

## Display order (TUI)

By default each pane lists rows newest-first (top-down) — new lines from
a live-appended file arrive at the *top*, so you don't need to scroll to
see what just happened. Press `R` to flip a pane to oldest-first
("[oldest-first]" appears in its title), matching the file's natural
top-down order. `R` toggles per pane and jumps the view to the current
latest row; `j`/`k` still move down/up the list, which means "further
back in time" when in the default newest-first mode. This is a TUI-only
feature — the plain CLI's `--follow` output is a real scrolling terminal
stream, which can't retroactively insert new lines above older ones, so
it stays oldest-first regardless.

## Detail pane (TUI)

Press `d` to expand the currently selected row inline, accordion-style:
every column/value pair (except the raw line, already shown on the
row's own summary line) appears directly beneath it as extra dim-styled
lines prefixed `+-`, and the selected row itself gets a `-` prefix.
There's no separate panel or border — the list simply grows to fit the
extra lines. `j`/`k` while it's open moves the selection and the
expansion follows, updating the shown fields live. Close it with `d`
again, or `Esc` (which closes the expansion first rather than quitting,
even in a single-pane session).

## Alerts (standing queries)

`--alert` turns the resolved filter/SQL into a standing query: instead of
printing results once, loglume watches the file and runs `--exec <cmd>`
each time the query fires, piping the fired rows to the command's stdin
(one formatted line each). Requires exactly one file and `--exec`.

```bash
# Run a command every time a new WARN-or-worse line shows up
loglume "severity >= WARN" --alert --exec "mail -s alert ops@example.com" app.log
```

`--window <duration>` (default `1m`; accepts `s`/`m`/`h`/`d` suffixes, e.g.
`30s`) sets the poll cadence and, for `Threshold` mode, how long the
condition must hold before firing.

Two firing modes:

- **`OnChange`** (default): fires whenever the query's result set changes
  from the last time it fired. Works with any plain `SELECT`.
- **`Threshold`**: fires once a range-vector query's reduced value crosses
  a comparison and holds for `--window`. Select it with
  `--alert-op <op> --alert-threshold <value>` (both required together,
  e.g. `--alert-op ">=" --alert-threshold 3`). The alert SQL must be a
  range-vector query for this mode, e.g.:

  ```bash
  loglume "select count_over_time(message) range 10 seconds from log" \
    --alert --alert-op ">=" --alert-threshold 3 --exec "notify-send alert" app.log
  ```
