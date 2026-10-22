# Touch Bar Settings app

`dfr-configurator`, the native GTK4/libadwaita app for editing the deck: layers,
buttons, appearance and behavior, with a live preview. Back to the
[README](../README.md).

## Opening it

`make install` installs `dfr-configurator` and adds **Touch Bar Settings** to the
application menu. The **Settings** button on the Home bar opens it too, through
the [capture helper](../integrations/capture/README.md) in your desktop session
(installed by `make install-user-session`). Run it as your normal desktop user,
never with sudo; the app never elevates privileges.

Building and running it needs GTK **4.10+** and libadwaita **1.5+**. The daemon
does not depend on them: `cargo build -p tiny-dfr` never compiles the app.

```sh
dfr-configurator [--config PATH] [--base PATH]
cargo run -p dfr-configurator -- [--config PATH] [--base PATH]   # from a checkout
```

## Save and Apply

The daemon reads `/etc/tiny-dfr/config.toml`, which belongs to root. The app
does not write there by default. Instead:

- **Save** (or Ctrl+S) writes your *draft*:
  `$XDG_CONFIG_HOME/dfr-deck/config.toml`, or `~/.config/dfr-deck/config.toml`.
  Saving a draft does not change the live bar.
- **Apply** is a separate administrator step: validate the draft with
  `tiny-dfr-deck check` and install it to `/etc/tiny-dfr/config.toml`. The exact
  commands are in [INSTALL.md](../INSTALL.md), and the app's Apply Instructions
  action shows them too. The daemon picks up the new file on its own.

A persistent Save/Apply banner, the Apply Instructions action and a
"draft saved" toast make the difference clear without a dialog on every save.

With `--config PATH` the app edits that file directly. If that file is the live
`/etc/tiny-dfr/config.toml` and you can write it, the daemon reloads as soon as
you save; the toast tells you which kind of save happened.

### Drafts

- With no draft yet, the app loads the installed `/etc` configuration (or the
  full-deck example if there is none) into memory. Opening never writes a file,
  and browsing inherited layers does not create changes.
- `--base` defaults to `/usr/share/tiny-dfr/config.toml`. All pages edit one
  shared document.
- Invalid drafts cannot be saved. Field diagnostics and **Review Issue** take
  you to the problem. Validation uses the same `dfr-config` code as the daemon,
  at both bar widths, including icon resolution.
- The first save in a session backs up an existing file as `.bak-<epoch>`. Saves
  write canonical TOML to a temporary file and rename it into place atomically.
- A failed write stays visible with **Retry Save** until it succeeds or you
  dismiss it. Closing with unsaved changes offers Cancel, Discard and, when the
  draft is valid, Save and Close.

## Pages

**Layers** shows the layer list, a compact preview and the ordered buttons.
Selecting a button opens Button Properties inside the page; Back returns to the
list. **Add Button** sits beside the list heading. Button Options has Move
Earlier/Later and Delete; Layer Options has Rename, reorder and Delete.

- Rename commits with the entry's Apply button and updates the startup and Fn
  assignments and every button that links to the layer.
- Deleting a layer that is linked or assigned asks for a replacement, and
  incoming links can be opened for repair.
- The last layer and the last button cannot be deleted.

**Appearance** starts with theme switching: pick a theme to audition it in the
preview, then **Switch to Theme** saves it as a fixed startup theme
(`ActiveTheme`) in the draft. Below that are Color Theme, Background and Button
Style: fixed or remembered startup theme, palette overrides and custom themes,
color pickers with validated hex entry, named hourly background bands, and
typography. See [themes.md](themes.md).

**Behavior** groups Layer Switching, Display, Battery and Pet.

Everyday controls use plain labels and units. **Advanced** sections hold
fontconfig patterns, XDG icon themes, icon sizes, waveform and visualizer
smoothing and lag, and pixel shift. Every content kind and all ten action forms
are editable. Shortcut capture has a distinct listening state with Finish and
Cancel.

### Inherited values

Each setting shows its effective value (base plus your overrides), labelled
**Using Base Setting** or **Customized**, with a reset that removes your
override. A value equal to the code default is still saved when it overrides a
different base value. Because a missing value inherits the base, the app cannot
erase a startup theme or Fn layer pinned in the base: it explains the former and
offers Fn **None** only when the base allows it. Theme tables are replaced as a
whole map, and an explicitly empty map is preserved.

## Preview

The preview uses the same `dfr-render` Cairo code as the daemon. **Enlarge**
opens a dialog with Fit and 1:1 scale, horizontal scrolling, both bar widths
(2008 and 2170), layer and theme choices, a background-hour audition, and
accessible names for each rendered button. Generated buttons (Pet, theme, Home,
Esc) are marked separately from your own, and your buttons can open their
properties from the preview.

- The preview never runs actions.
- Theme and hour auditions use an in-memory copy; they never touch the document
  or the saved theme state.
- The theme remembered on the device is not read; the caption says when the
  preview falls back.
- Live feeds, hardware brightness and actions are not simulated.
- An invalid draft shows diagnostics instead of an old frame. Superseded
  refreshes are cancelled and temporary render files are removed.

## Design

The app uses only GTK4 and libadwaita, with system styling and a single primary
Save action. It follows libadwaita's
[NavigationSplitView](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.NavigationSplitView.html)
sizing and collapse and
[NavigationView](https://gnome.pages.gitlab.gnome.org/libadwaita/doc/1-latest/class.NavigationView.html)
Back navigation. At narrow widths the layer list becomes its own page and the
view switcher moves to the bottom. Where the original wireframes differ from
platform conventions, the conventions win: the header bar supplies Back, the
properties page keeps a stable identity, and `Adw.Dialog` picks its
presentation from the window size. The baseline is GTK 4.10 / libadwaita 1.5.

Tests for the app are described in [development.md](development.md).
