# Rap

Rap is a local desktop audio player written in Rust. Its `egui` window has an animated ASCII donut and a PCM-reactive visualizer, alongside track controls, a seek bar and colour settings. Audio decoding uses Symphonia; CPAL sends converted PCM to the **default** output device. The app does not stream music or require an account.

## Build and run

Use a recent stable Rust toolchain that supports edition 2024. On Linux, install a C compiler, `pkg-config`, ALSA development headers/libraries (for example `libasound2-dev` on Debian/Ubuntu or `alsa-lib` on Arch), and the development/runtime libraries required by your X11 or Wayland desktop. A running graphical session and a usable default audio output device are needed for playback. File and folder dialogs on Linux use the XDG desktop portal; ensure a suitable portal backend is running. The Linux system tray uses a StatusNotifierItem watcher supplied by the desktop environment.

From this directory:

```sh
cargo run --release
cargo run --release -- "/path/to/song.flac"  # optional initial file
cargo test
cargo build
```

The optional first argument loads one file into a single-track queue, stopped until Play is pressed. Other arguments are not processed. To install a per-user launcher and icon on Linux, run `scripts/build-install.sh`; it builds the release binary and calls `scripts/install-desktop.sh`. The launcher points to the release binary **in this checkout**, so keep the checkout (and its `target` directory or `CARGO_TARGET_DIR` release build) in place while the launcher is installed. `rsvg-convert` is optional for a PNG icon; the SVG is installed regardless. Uninstall the owned launcher and icons with `scripts/install-desktop.sh --uninstall`. The scripts modify files in your home directory; review them before running them.

## Playing music

- On the **Music** page, **Open file** selects one local file; **Music folder** scans supported files recursively, sorts them by path and queues them. Directory symlinks are not traversed. The first track is prepared but does not start automatically. You can also open a file with the command above.
- MP3, FLAC, WAV and Ogg Vorbis are the supported file types in the picker and folder scan. Actual decoding depends on Symphonia recognizing the container/codec. The filename without its extension and, if available, the artist tag are displayed.
- Previous/next navigate the queue. Repeat cycles **Off → All → One**: at the end, Off stops, All starts over, and One replays the same track. Click or drag the seek bar to change playback position; dragging commits when released.
- Playback uses the current default output configuration. Volume applies to Rap only, from 0 to 100% in 5% steps; it does not change system volume. Opening and decoding happen away from the UI thread; an unsuccessful selection/load leaves the current track in place and reports an error.
- **Colour** offers B&W, Solid and RGB visualizer modes and a hue slider. The mode and hue persist only for the current session. On Linux, a working tray offers **Show** and **Quit**; closing the window can keep playback running via the tray on X11 or Hyprland. On other Wayland compositors, or without a working tray, closing normally exits instead of leaving an inaccessible background process.

### Keyboard and mouse (while the Rap window is focused)

| Input | Action |
| --- | --- |
| Space | Play/pause |
| Ctrl+Left / Ctrl+Right | Previous / next track |
| R | Repeat Off → All → One |
| Ctrl+O / Ctrl+F | Open file / choose music folder |
| Up / Down, or mouse wheel over the window | Rap volume ±5% |
| Alt+C / Alt+M | Colour / Music page |
| Alt+B / Alt+S / Alt+G | B&W / Solid / RGB |
| Escape | Back / collapse side menu |
| Ctrl+B | Expand / collapse side menu |
| Tab / Shift+Tab, Enter | Focus and activate controls |

The seek bar also accepts keyboard input when focused. The window close shortcut follows your desktop environment's normal close-window behavior; it is not a global Rap hotkey.

## Limits

Each file is decoded and converted into memory before playback, so large files can take time and memory. Rap uses the default device only (no device selection); channel conversion repeats or averages channels rather than providing surround-aware mixing. It does not implement playlists such as M3U, network streams, cover art, a searchable library, DSP effects or MPRIS/media-key control. Supported default-device sample formats are f32, signed 16-bit and unsigned 16-bit; other configurations produce an error. The automated tests cover decoding and player logic, but **audible output on a particular device has not been verified here**.
