# wpic

A quick & dirty terminal UI for importing, culling, rating and backing up photos
from a camera SD card.

It clusters shots into "occasions" by capture time, lets you review and cull them
in [imv](https://sr.ht/~exec64/imv/) (with full-resolution images and correct
rotation — even straight from NEF), rate them 0–5 into darktable-compatible XMP
sidecars, and upload selected folders to Google Drive via
[rclone](https://rclone.org/).

## Two modes

- **Card mode** — a camera card is mounted: shots are clustered into dated
  occasions which you name, review/cull, and **move** into `~/Pictures`.
- **Library mode** — no card (or `--library`): browse the existing
  `~/Pictures` occasion folders to review/cull, rate, and upload.

It auto-detects a mounted card (`/run/media/$USER/*/DCIM`, any label) and falls
back to library mode when none is present.

## Highlights

- Time-gap clustering (configurable; default: new occasion when shots are >5h apart).
- Keeps RAW+JPEG pairs together; counts a jpg+nef pair as one "pic".
- Viewer shows **full-resolution** images: originals for upright JPEGs, lossless
  `jpegtran` rotation for portraits, and the **embedded full-res JPEG extracted
  from NEFs** when there's no JPEG (so RAW-only days are viewable).
- 0–5 star rating written to / read from **XMP sidecars** (interoperates with darktable).
- Cull to a per-folder `.removed/` (recoverable, never uploaded); view/restore or
  trash it.
- Play an occasion's movies in mpv; jump into [`wcut`](https://github.com/rvalimaki)
  to edit them.
- Upload per occasion (all / photos-only / rated-only) to Google Drive; folders
  already present show their name in green.

## Build

```sh
cargo build --release
# optional: symlink onto PATH
ln -sf "$PWD/target/release/wpic" ~/bin/wpic
```

## Runtime dependencies

External tools invoked at runtime (install what you use):

| tool | used for |
|------|----------|
| `imv` | photo viewer |
| `vipsthumbnail` (libvips), `jpegtran` (libjpeg-turbo) | rotation / preview generation |
| `exiv2` | NEF embedded-preview extraction, XMP rating |
| `mpv` | movie playback (`m`) |
| `rclone` | Google Drive upload + status (a `gdrive:` remote) |
| `gio` | trash `.removed` (`T`) |
| `wcut` | movie editing (`w`) |

## Config

On first run wpic writes `~/.config/wpic/config` (respects `XDG_CONFIG_HOME`):

```ini
rclone_dest = gdrive:Pictures   # uploads go to <rclone_dest>/<occasion>
gap_hours   = 5                 # new occasion when shots are >N hours apart
pictures    = ~/Pictures        # library / sort destination
```

`WPIC_RCLONE_DEST` env var overrides `rclone_dest` for one-off runs.

## Usage

```sh
wpic              # card mode if a card is mounted, else library mode
wpic --library    # force library mode on ~/Pictures
wpic --list       # print the occasion listing and exit
wpic /path/DCIM   # point at a specific DCIM directory
```

### Keys

Navigate with `↑/↓`. `Enter`/`v` review & cull · `s` view starred (1–5) ·
`f` view rated ≥ bar · `1`–`5` set the bar · `m` movies · `w` wcut ·
`R` view removed · `T` trash removed · `u` cycle upload mode · `C` check Drive ·
`r` rename · `x` execute · `q` quit. (Card mode adds `a` move/copy, `d` don't-move.)

In the viewer (imv): `1`–`5` rate (0 clears) · `Delete` cull · `u` undo last cull.

## License

MIT
