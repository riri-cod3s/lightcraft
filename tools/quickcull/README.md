# QuickCull

Fast, keyboard-driven culling of **already edited** photos: arrow through a folder, tap a key on the
keepers, then copy (or compress) the picks into another folder. Standalone Rust app (egui), separate
from the LightCraft workspace.

```sh
cd tools/quickcull
cargo run --release -- /path/to/wedding-folder     # or run without args and use "Open folder"
```

| Key | Action |
|---|---|
| `←` / `→` (or `A` / `D`) | previous / next photo (hold to scrub) |
| `Space` or `↑` | toggle pick |
| `↓` or `X` | unpick |
| `Enter` or `S` | pick and go to the next photo |
| `Tab` | show only picked photos (second pass to trim down) |
| `PgUp` / `PgDn`, `Home` / `End` | jump ±10, first / last |
| `E` | copy picks… dialog |

- The top bar shows `✔ picked / target` (default target 60, editable) in green, amber once over target.
- Picks are saved after every keypress to `.quickcull.json` in the photo folder, so you can quit and
  resume, and your original files are never touched.
- **Copy picks**: default destination is `<folder>/selected`. Plain copy is byte for byte (metadata kept).
  Tick *Compress* to re-encode as JPEG at a chosen quality and longest side (EXIF is dropped). A bad
  file is reported and skipped; the rest still copy.
- Speed: photos decode on worker threads and the next six (and previous two) are prefetched;
  display copies are fit to 2600 px.
- Reads JPEG, PNG, WebP, TIFF. Raw files are not supported (this is for finished photos).
