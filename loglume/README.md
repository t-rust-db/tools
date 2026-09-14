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

# Saved filters/queries, referenced on the command line as "@name".
[filters]
myerr = "SELECT * FROM log WHERE severity >= 'WARN'"
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
