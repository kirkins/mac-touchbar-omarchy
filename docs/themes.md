# Themes and the day cycle

Color themes, the theme buttons, how the chosen theme is remembered, and the
time-of-day background. Back to the [README](../README.md).

## Color themes

A theme sets the colors behind the eight accent names every button uses
(`neutral`, `cyan`, `amber`, `blue`, `violet`, `orange`, `red`, `green`) and,
optionally, the time-of-day background bands.

Fourteen themes are built in, in this order: `Default` (the original deck
palette), `Midnight`, `Nord`, `Ember`, `Rosewater`, `Forest`, `Solarized`,
`Mono`, `Daybreak`, `Tidal`, `Arcade`, `Gilded`, `Orchard` and `Aurora Noir`.

Every built-in keeps its accents bright and distinct over the dark bands, keeps
white labels on dark button fills, and keeps the color roles: cyan for audio,
amber for brightness, violet for navigation, orange for warnings, red for
critical, green for charging. A few have their own character: Daybreak is light
and airy while its dark bands keep contrast; Tidal uses deep-sea bands; Arcade
puts neon accents over dark violet; Gilded is brass and lapis over walnut;
Orchard is botanical greens and berry over dark leaf; Aurora Noir is mint,
cyan and violet over an inky strip that deepens at night.

### Custom themes

```toml
ThemesEnabled = true     # false removes the theme picker and theme buttons
ActiveTheme = "Midnight" # optional: pin the startup theme

[Themes.Mine.Accents]
neutral = "#9AA0A6"
cyan = "#40E0D0"
# ... any of the eight accent names, values "#RRGGBB"

[[Themes.Mine.DayCycleBands]]
Name = "day"
StartHour = 12
Color = "#1A1B1E"
```

Themes are partial. Anything a theme does not set is inherited - from the
default palette for a new theme, or from the built-in it overrides when it
reuses a built-in's name. Invalid accent names, colors or bands reject the
candidate file like any other knob. Up to 21 themes fit the picker; more are
rejected. A few alarm colors never follow a theme, such as the waveform's
dead-feed red.

## Theme buttons

Theme buttons are not on the shipped deck. Add them to any layer:

```toml
[[Layers]]
Name = "more"
Keys = [
  { Text = "Theme", Action = "CycleTheme" },
  { Text = "Themes", Action = { Layer = "themes" } },
]
```

- **`CycleTheme`** applies the next theme in order, wrapping at the end. The
  button briefly shows the applied theme's name, then its own label again. No
  keys fire and the bar stays put.
- **`{ Layer = "themes" }`** opens the picker: one button per theme across the
  whole bar, plus Home. Tapping a name applies it and returns home.
- **`{ Theme = "Nord" }`** applies one theme by name. Add `Layer = "home"` to
  switch bars in the same press.

A user layer named `themes` replaces the synthesized picker. With
`ThemesEnabled = false` the picker and the synthesized buttons disappear, and
buttons that still target `themes` are rejected like any unknown layer.

## Applying and remembering

Applying a theme is live: the palette and background bands swap on the next
frame - buttons, slider fills, accents, the waveform tint, the strip - with no
restart or reload.

The choice made on the glass is saved to `/var/lib/tiny-dfr/active-theme` and
survives restarts. The service drop-in prepares that directory for the daemon's
account. Precedence:

1. `ActiveTheme` in the configuration, if set, always wins at startup and on
   reload. This is what Settings saves with **Switch to Theme**.
2. Otherwise the remembered choice is used.
3. A remembered name that no longer exists falls back to the first theme and
   the file is rewritten, so a stale file can never wedge the bar.

## Time-of-day background

The strip behind every bar follows local time: a warm dawn gray in the morning,
a brighter neutral slate at midday, warm amber in the evening and a dark cool
blue overnight. The band color shows through the gaps between buttons and
behind every widget on every layer, including the open ground of the pet layer.
Button fills, accents, the slider and the pet keep their own colors on top.

The daemon repaints once when the band changes, on the wake cadence it already
keeps for the clock and battery. There is no per-frame animation.

```toml
DayCycleEnabled = true  # false keeps the plain black background

# Optional: replace the default bands entirely. Each band runs from its
# StartHour (local time) until the next one; the last wraps past midnight.
[[DayCycleBands]]
Name = "day"
StartHour = 9
Color = "#1A1B1E"

[[DayCycleBands]]
Name = "night"
StartHour = 21
Color = "#04060D"
```

Each band needs a unique nonempty `Name`, a `StartHour` from 0 to 23 that no
other band uses, and a six-digit hex `Color` (the leading `#` is optional). The
list replaces the four default bands as a whole, so a single band paints one
color all day. A theme that defines its own `DayCycleBands` replaces the bands
while it is active; a theme without bands keeps the configured ones.
