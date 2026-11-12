# Developing FramePlayerX

How to build, test and release FramePlayerX, and how the code fits
together. For using the app, see the [user guide](USER-GUIDE.md).

## Requirements

- **Linux** (Ubuntu or Debian), or **Windows with Ubuntu under WSL2**. The
  workspace doesn't build on Windows itself.
- Rust through rustup, which installs the version in `rust-toolchain.toml`
  (the one CI uses) with clippy, rustfmt and the ARM64 target.
- The tools in `tools/requirements.txt`, from a virtualenv (the file says
  how): zig and cargo-zigbuild for builds for the Frame, meson for FFmpeg
  (Ubuntu's 1.3 can't use zig's linker), and git-cliff for releases.
- For FFmpeg: ninja, nasm and pkg-config.

On Ubuntu, `tools/local-xr.sh setup` installs Monado and the tools FFmpeg
needs with apt, then builds FFmpeg, in one go.

## Build and test

```bash
tools/build-ffmpeg.sh x86_64 aarch64      # FFmpeg 7.1 with dav1d and zlib into third_party/
export LD_LIBRARY_PATH=$PWD/third_party/ffmpeg/x86_64/lib
cargo test --workspace                    # GPU tests use any Vulkan device (lavapipe is fine)
cargo clippy --workspace --all-targets   # lints are in [workspace.lints]
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
(cd tools/frame-desktop-install && python3 -m unittest)   # the Desktop Mode installer
shellcheck packaging/*.sh tools/*.sh      # as CI does
```

Under WSL, keep the Linux build off the Windows drive with
`export CARGO_TARGET_DIR=~/.cache/frameplayerx-target`, as
`tools/local-xr.sh` does.

## Code style

`cargo fmt` formats. Clippy, with the lints in `[workspace.lints]`, has to
pass with warnings denied, as it does in CI.

Comments say what the code can't:

- Each module starts with `//!`: what it is for, in a sentence or a short
  paragraph.
- `///` documents an item when its name and signature leave something out:
  units, what `None` means, that it blocks, what it costs. A field called
  `title` needs no `/// Title.`
- `//` inside a function says why: a hardware or server quirk, an
  invariant, a workaround and what it works around. Not what the next line
  does.
- Every `unsafe` block has a `// SAFETY:` comment saying why it is sound
  (clippy checks).
- No commented-out code. A known gap is an issue, not a `TODO`.

## Translations

The interface's text is in [Fluent](https://projectfluent.org/) files, one
per language: `crates/frameplayerx/i18n/<language>/frameplayerx.ftl`.
English is the source; every other language translates its messages, and a
message a language lacks shows in English. Translators work on
[Codeberg Translate](https://translate.codeberg.org/projects/frameplayerx/),
which sends their changes as pull requests (see [Help translate](../README.md#help-translate)).

- Text the interface shows goes through `tr!("message-id")`, or
  `tr!("message-id", name = value)` with arguments. Add the message to the
  English file only, with a comment saying where it shows, "Short." for a
  button, chip or tab. The build checks each `tr!` against the English
  file: an unknown id or arguments that don't match don't compile.
- `tr!` without arguments returns a `&'static str`, kept until the
  language changes. `const` and `static` items can't call it, so lists of
  labels are functions.
- Errors from `fp-sources` and `fp-media` are put into words in
  `ui/errors.rs`. Text from servers, decoders and drivers shows as it comes.
- The phone remote's pages (`crates/fp-remote/src/web/`) are filled in as
  they are served, with the messages `remote_page_text` in `services.rs`
  hands over in the app's language: `{{remote-send:Send}}` in the HTML,
  `t('remote-sent', 'Sent to headset')` in `app.js`, each with the English
  for one it doesn't.
- A new language: its folder (Weblate adds it) and an entry in `LANGUAGES`
  in `src/i18n.rs`, named in itself. Steam's name for it goes in
  `steam_code` if Steam has it. egui can't yet show right-to-left text or
  scripts that need shaping (Arabic, Hebrew, Thai, the Indic scripts).

```bash
cargo test -p frameplayerx every_translation -- --nocapture   # each file parses, uses only English's ids and arguments; prints how much is translated
UI_LANGUAGE=pseudo tools/ui-preview.sh /tmp/pseudo   # every screen in a made-up language
UI_LANGUAGE=de tools/ui-preview.sh /tmp/de           # or es, fr, ja, zh-CN
```

The made-up language is English with its letters accented and its vowels
doubled: text left untranslated stands out, and text that only just fits
in English shows where German won't. Japanese and Chinese use the
headset's Noto Sans CJK; for the preview, install it with
`sudo apt install fonts-noto-cjk`.

## Repository layout

| Crate | Purpose |
|---|---|
| `frameplayerx` | The app: the XR loop, the interface, and wiring everything together |
| `fp-core` | Shared types: formats and their detection, view settings, the source contract, data folders |
| `fp-ffmpeg-sys` | Bindings to the bundled FFmpeg 7.1, per architecture (`tools/gen-ffmpeg-bindings.sh`) |
| `fp-media` | Demuxing, decoding (V4L2 hardware, 10-bit HEVC as Q10C; software), the A/V clock, ALSA audio, subtitles, thumbnails |
| `fp-render` | The Vulkan renderer: the projection shader for every format, the Q10C to P010 conversion, egui panels, quads |
| `fp-xr` | OpenXR: the session and swapchains, controller input, passthrough |
| `fp-xr-loader` | Finds the active OpenXR runtime and loads it directly, with no Khronos loader to ship (shared with `frame-probe`) |
| `fp-library` | The SQLite database: folder scanning, the metadata and thumbnail worker, search, watch history |
| `fp-sources` | Sources: local folders, HTTP, WebDAV, DLNA, SMB, SFTP, DeoVR and HereSphere feeds, Jellyfin, Emby and Plex |
| `fp-haptics` | Funscripts, the sync engine, and TCode, Buttplug and Handy devices |
| `fp-remote` | The DeoVR remote API and the phone web remote |
| `fp-updater` | Signed update manifests, download, install and rollback; the `fp-release` maintainer tool |
| `frameplayerx-install` | The PC installer: pairs with the headset and installs over SSH |
| `frame-probe` | Reports what a Steam Frame offers a native app (see [The platform probe](#the-platform-probe)) |

| Folder | What's in it |
|---|---|
| `assets/` | The logo (`logo.svg`), the Steam library artwork made from it, and the docs' install buttons |
| `site/` | The GitHub Pages site: the page the Frame Control button opens (see [Install buttons](#install-buttons)) |
| `packaging/` | What goes in the release zip besides the binary: the launcher, a README and the licences |
| `tools/` | Build, package, preview and test scripts |
| `tools/frame-desktop-install/` | The Desktop Mode installer: a Python script, and the install and uninstall `.desktop` launchers that download and run it (see its [README](../tools/frame-desktop-install/README.md)) |
| `tools/crash-relay/` | The Cloudflare Worker crash reports go to (see [Crash reports](#crash-reports)) |

## How it fits together

**The app loop.** One loop owns the OpenXR session and the Vulkan device.
Each frame it reads the controllers, builds the interface with egui and
draws both eyes: the video through the projection shader, then the panels
as quads in the scene. Interface code doesn't change the app's state
itself: it returns `Action`s that `App::apply` carries out. Anything slow
(opening a video, browsing a server, scanning folders, checking for
updates) runs as a background job whose result comes back on a channel, so
a frame doesn't wait on the network.

**Video.** FFmpeg demuxes through fp-core's `ByteSource`, so every network
protocol is implemented in Rust and looks like a file to FFmpeg. H.264 and
HEVC go to the Frame's V4L2 stateful decoder; anything it can't take is
decoded in software. Frames are uploaded to Vulkan as YUV and converted in
the shader, which maps each pixel of each eye onto the video for the chosen
projection and stereo layout. HDR is tone mapped there too, with BT.2390's
curve to the scene's peak, after compressing BT.2020 colours into BT.709
(`fp-render/src/hdr.rs`); a small compute pass counts each new HDR frame's
brightness into a histogram the peak comes from (`peak.rs`). 10-bit HEVC
comes out of the decoder as Q10C (Qualcomm's compressed TP10), which only
the GPU can read: `fp-render/src/q10c.rs` reads it through TP10 texture
descriptors made from Turnip's own (`VK_EXT_descriptor_buffer`) and turns
it into P010 planes in a compute pass.

The player's hardware-decoded frames aren't copied at all on the Frame:
the decoder lends each frame its buffer, exported as a DMA-BUF
(`VideoFrame::shared`), and the renderer imports each buffer once and
reads it where it is (`fp-render/src/shared.rs`), NV12 by the eye passes
and Q10C by the conversion. A frame holds its buffer until the GPU is done
with it, then the decoder has it back. When the driver would be left with
fewer buffers than it needs, or an import fails, frames are copied out as
before. So are 8-bit HDR frames, whose scenes are measured in the planes
the renderer uploads.

An SDR 10-bit frame read in place isn't converted either: the eye passes
sample its Q10C planes through TP10 descriptors (`fp-render/src/tp10_eyes.rs`,
`shaders/planes_tp10.wgsl`), which saves the conversion pass, 5.7 ms for an
8K frame. A descriptor reads at most 2^24 samples, so each plane is read in
overlapping bands of rows, and a sample comes from the band whose middle is
nearest, where its bilinear neighbours are too. TP10 descriptors exist only
as bytes, so this pipeline takes all its descriptors from descriptor
buffers, the scene's parameters included. HDR 10-bit frames are still
converted, as their scenes are measured in the converted planes.

**Sources and browsing.** Every source implements one `Source` trait
(`list`, `open`, `details`). Each kind gets its own browser in `ui/`:
`local.rs`, `folders.rs`, `catalog.rs` and `feeds.rs`, built from the
shared pieces in `ui/page.rs`. Network browsing does not index new videos
in the database; a video gets a row when it's opened. Browsing can migrate
existing rows after learning their stable identities.

Persistent source references use
`source://source/<encoded-source-id>/<kind>/<encoded-key>`
(`fp-sources/src/location.rs`). Each component is encoded separately; the
key is one opaque segment, including any encoded path separators. HTTP(S),
WebDAV, SMB and SFTP keys are relative to the configured root; Jellyfin and Emby
keys retain item and version IDs, and DLNA uses object IDs rather than
resource URLs. Feeds use published stable scene IDs where available;
otherwise scene, media and asset URLs remain exactly as published,
including external and signed URLs. Changing the configured feed address
does not rebase those URLs. Plex keeps its existing format described below.
Sources resolve these references against their current configuration at
open time. The configured source ID is the namespace: editing its endpoint
preserves identity, while removing and adding it again creates a new one.
Local paths and removable-volume identities keep their existing formats.

`Source::canonical_location` performs no I/O: it recognizes owned legacy
locations from configuration or aliases learned while browsing. External
or signed URLs are not guessed; a published feed ID can identify a signed
scene URL, while an unknown old DLNA URL cannot be mapped from its address
alone. The opener dispatches every source-qualified location to its exact
owner, including artwork, scripts and subtitles; missing or mismatched
owners fail without trying another source or plain HTTP.

`Services` migrates legacy references before starting the metadata worker
and before replacing source configurations, while the old endpoint/root
is still known. Browsing retries migration after learning aliases.
`Library::rewrite_source_locations` updates only locator columns, including
sidecars, artwork and selected subtitle files, in one source-scoped
transaction. Media primary keys, user settings and history/playlist
foreign keys stay intact. A location collision rolls back the whole
transaction and is logged; rows are never merged or replaced.

Jellyfin, Emby and Plex implement `fp_sources::Catalog`, the shared catalog
query, item and capability contract. `ui/catalog.rs` owns their page stack;
wire parsing stays in `mediabrowser/` and `plex/`. Capability flags hide
unsupported server favorites and filters. The old `Mb*` names remain
aliases for callers of the MediaBrowser API.

Plex genre filters use facet IDs. Studio filters use studio names with the
string equality operator (`studio==...`), encoded once; PMS may already
URL-encode studio choice keys.

Plex linking (`ui/plex.rs`, `app/plex.rs`) uses the existing action/job
flow. Its default **Link with code** method displays a code to enter at
`plex.tv/link` on a phone or computer, then polls for approval; it reuses
the common pairing-code and cancellation labels. **Access token** is the
secondary method for an existing server URL and token. A tested configuration
records the machine identifier, connection choices, server token and account
credentials. New links use Plex's traditional short-PIN flow at
`plex.tv/api/v2/pins`, without JWK registration or a device assertion, for
PMS compatibility. Existing saved device-key/JWT credentials remain readable
and retain their refresh path; credential updates are persisted without
rebuilding active sources. A live test confirmed server identity and library
access with a traditional token after the same server rejected a JWT with
HTTP 401. Playback and remote access still need live validation.
`plex://source/<source-id>/<machine-id>/...` locations contain no tokens;
the opener routes them only to their owning source. Artwork and external
subtitles use that same authenticated opener. Named HTTP headers are
origin-scoped, including range reads and redirects.

The Plex form has one **Connection** selector: **Local only** (the default
for new sources), **Direct connections**, or **Direct + Relay**. Its UI enum
maps to the existing `PlexConfig.local_only`/`allow_relay` flags, so saved
configurations need no migration and edits retain their previous mode.
The descriptions distinguish free local playback from remote video, which
requires Plex Pass for the viewer or server owner, or Remote Watch Pass for
the viewer. A LAN address does not guarantee local classification: Plex can
treat another subnet or VLAN as remote. Relay uses the same subscription
requirements and is limited to 2 Mbit/s.

`HttpClient` also supports an origin-scoped address fallback after DNS
resolution or connection failure, including a TLS handshake timeout. It
reserves time for the advertised socket within the connection timeout and
logs the attempt at Info level. The URL hostname, TLS SNI and certificate
verification stay unchanged; certificate rejection never triggers a retry.
Plex supplies the same discovered connection's advertised IP address and
port only for non-relay `plex.direct` URLs; server machine
identity validation still applies. This resolves DNS rebinding failures
where the server is reachable, without changing routing or firewall rules.

Original Plex playback requests a decision with `directPlay=1`, `hasMDE=1`
and explicit `mediaIndex`/`partIndex` values. `subtitles=none` leaves
subtitle rendering to the local player while keeping embedded tracks and
sidecars available. Codec and container support is decided by the local
player instead of Plex's generic client profile;
authentication and server playback denials still apply. If the local
player cannot open the original, Automatic uses the existing transcode
fallback. Both MDE and legacy decision errors are checked before opening
the original Part.

Plex transcodes use progressive Matroska over `ByteSource`, not FFmpeg
networking. Restarting after a seek creates an encoding session distinct
from the watch-report session and stops the previous encoding. Original
server track IDs survive the restart.

**Libraries and cards.** A library is a source (`SourceConfig::Library`)
whose folder is scanned into the database under its own id
(`Library::scan_folder`), on start, when its card is mounted, when its page
opens and on Scan now (`app/tasks.rs`). Files on microSD cards and USB
drives are located as `volume:<filesystem ID>/<path on the card>`
(`fp_core::volume`): the local source lists them so, the scanner stores
them so, and the opener resolves them to wherever the card is mounted, so
two cards with the same label and folders never share rows. A card's ID
comes from `/dev/disk/by-uuid`.

**Watch sync.** Servers that keep watch state (Jellyfin, Emby and Plex, HereSphere
APIs that name an `eventServer`) also implement `Source::watch_session` and
`Source::report_watch` (`fp-sources/src/watch.rs`). The open video's `Watch`
(`frameplayerx/src/watch.rs`) turns what the player does each frame into
events: start, pause, resume, seek, speed, a tick every 10 s, and stop. One
thread sends every report in order, so a video's stop reaches the server
before the next one's start, and quitting waits up to 3 s for it. A
server's resume point (`Entry::resume`) wins over the database's.

**Subtitles.** `frameplayerx/src/subtitles.rs` picks the track a video starts
with, from the Subtitles setting and the tracks' languages
(`fp_core::language` reads `ara`, `pt-BR`, `de.forced`). The player decodes
embedded tracks itself; files are loaded whole. Text cues are set by
cosmic-text (`ui/subtitle_text.rs`) with the system's fonts, loaded on a
thread at startup, so every script is shaped; egui sets them until then.

**The database.** SQLite, with the schema version in `PRAGMA user_version`
and forward-only migrations in `fp-library/src/schema.rs`. The scanner
marks files missing rather than deleting them, so ratings, resume points
and adjustments survive a card being taken out.

**Blu-ray.** `fp-media/src/mvc` rewrites H.264 MVC's second view into
plain H.264 pictures any decoder takes. In MKV the second view rides in the
base view's packets. In transport streams (`.ssif`, and `.m2ts` holding
both views) it has a PID of its own, 0x1012, which FFmpeg's H.264 parser
holds back whole, so FFmpeg discards it and `second_view.rs` reads it from
the file, adding each access unit to the base view's packet of the same
decoding time. `fp-media/src/bluray` opens disc images: a read-only UDF
reader (`udf.rs`, 2.50 metadata partitions included), the playlists
(`mpls.rs`) and the main title, its longest playlist. `Input` plays its
clips one after another, their timestamps moved onto the title's clock;
3D titles read each clip's SSIF straight out of the image.

**Frame sequential 3D.** `fp-media/src/frame_seq.rs` wraps an ordinary
decoder and puts each left frame and the right frame after it side by side,
the same step 3D Blu-ray ends with. `StreamInfo` then reports what comes out:
twice the width and half the frame rate, and its format is
`StereoLayout::FrameSequential`, drawn as full side by side. A viewer who picks
another format, or frame sequential for an unlabelled file, reopens the video
with `PlayerConfig::frame_sequential` set. A frame's eye comes
from its timestamp, not a count, so a seek or a lost frame can't leave later
frames on the wrong eye. An odd gap in a file's timestamps would swap them,
which no file tried has.

**Interlaced video.** `info::interlaced` calls a stream coded as fields
when FFmpeg's field order says so or, for H.264, when a sequence parameter
set allows field pictures (`frame_mbs_only_flag` 0, `mvc::field_coded`):
FFmpeg reads an H.264 field order from the first picture alone, and none
from field pictures (PAFF camcorder clips). Such H.264 never goes to the
hardware decoder: FFmpeg's V4L2 wrapper is refused every packet of 1080i,
and the native path gives up after seconds. `fp-media/src/deinterlace.rs`
runs a software decoder's frames through libavfilter's bwdif
(`mode=send_field`), the one filter the FFmpeg build enables: the frames
the decoder marks interlaced for a stream found by its headers
(`deint=interlaced`), every frame when the viewer turns it on (`deint=all`).
Each field comes out a picture, timed in a time base half the stream's, and
`StreamInfo` reports `deinterlaced` and twice the codec's frame rate.
`VideoFormat::interlaced` holds the viewer's choice from the Format tab's
**Interlaced** switch, reaching the player as `PlayerConfig::deinterlace`; a
change reopens the video, as frame sequential does.

**Pictures.** A card shows `MediaRecord::picture`: the frame where a partly
watched video was left off, else its server's picture, else a thumbnail the
metadata worker made (`fp-library/src/worker.rs`; only for videos whose
server offers none). Each pass of the worker first deletes the files in its
folder no video points to (thumbnails of videos since removed, interrupted
writes), then makes the frames for videos left off since the last pass,
after playback, and deletes those of videos since watched; each new
position gets a new file name, so the UI never shows an old frame from its
texture cache. Settings › General › Thumbnails reaches a running worker
through `MetadataWorker::set_options` (`WorkerOptions::thumbnails`,
`resume_thumbnails`). Server pictures go through
`frameplayerx/src/image_cache.rs`: kept on disk up to Settings › General ›
Image cache, least recently used out first. One its server marks
`immutable` (Jellyfin's, whose address carries the image's tag) is never
checked again; any other is checked once a day with `If-None-Match` /
`If-Modified-Since`, shown from the cache meanwhile.

**Data on the headset.**

| Folder | What's in it |
|---|---|
| `~/frameplayerx` | The app: binary, launcher, `lib/` (FFmpeg, dav1d), artwork, `VERSION` |
| `~/frameplayerx.old` | The previous version, kept for rollback |
| `~/.config/frameplayerx` | `settings.json`, `sources.json` (passwords, readable only by the user) and the optional `env` file |
| `~/.local/share/frameplayerx` | The database, `frameplayerx.log` (and `.log.old`), and `Interactive/`, searched for scripts not next to their video |
| `~/.cache/frameplayerx` | `thumbnails/` (made thumbnails and the frames where videos were left off), `images/` (server pictures), and `feeds/`: each scene feed's last listing, shown at once the next time it's opened (readable only by the user: its URLs can carry the server's API key) |

## Running without a headset

### On this computer with Monado

`tools/local-xr.sh` runs FramePlayerX against [Monado](https://monado.freedesktop.org),
the open-source OpenXR runtime, so a change can be tried without building
and installing on the Frame. Monado shows both eyes in a desktop window and
fakes the headset and controllers from the keyboard and mouse. It works on
Ubuntu and Debian, and on Windows in Ubuntu under WSL2 (WSLg shows the
windows and plays the sound; Vulkan runs on llvmpipe there, so expect a low
frame rate).

```bash
tools/local-xr.sh setup                   # once: Monado, build tools, FFmpeg
tools/local-xr.sh run                     # Monado + FramePlayerX
tools/local-xr.sh run ~/Videos/clip.mp4   # ...opening a video
tools/local-xr.sh headless --exit-after 10 clip.mp4   # no window: smoke test
tools/local-xr.sh preview /tmp/shots      # every screen as PNGs (ui-preview.sh)
```

Click into Monado's debug window (*Monado!*) to drive it:

| Input | What it does |
|---|---|
| Keyboard and mouse | Move the headset |
| Left Ctrl / left Alt held | Move the left / right controller instead |
| WASD, Q/E | Move |
| Arrow keys, right-button drag | Turn |
| Left click | Trigger |
| Shift | Faster |

Its *Qwerty › Help* tab lists every key. The controllers present
themselves as Khronos simple controllers (select and menu), which
FramePlayerX binds.

- The window with the eyes opens at 1600×900; `LOCAL_XR_WINDOW=1280x720`
  picks another size, and it can be resized. Under WSL it's Monado's X11
  window, as the Wayland one stays black there.
- The eyes are 150% of the fake headset's 640×720.
  `XRT_COMPOSITOR_SCALE_PERCENTAGE=200 tools/local-xr.sh run` is sharper,
  `=60` smoother on a software renderer.
- Monado's log is in `/tmp/monado-service.log`; `-v` turns on
  FramePlayerX's debug log.

By hand, with Monado's simulated headset and no window:

```bash
XRT_COMPOSITOR_NULL=1 SIMULATED_ENABLE=1 monado-service &
XR_RUNTIME_JSON=/usr/share/openxr/1/openxr_monado.json \
  cargo run -p frameplayerx -- --exit-after 10 video.mp4
```

### The preview harness

`--preview` needs no OpenXR runtime at all: it renders offscreen with a
simulated head and right controller, driven by a script, and saves
screenshots. `tools/ui-preview.sh` uses it to render every screen; the
script commands are listed in `crates/frameplayerx/src/preview.rs`.

```bash
cat > /tmp/demo.fps <<'X'
wait 2
point main 0.24 0.04     # Watch history tab
click
shot history
open ~/Videos/clip_180_LR.mp4
wait-playing 20
shot playing
X
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo run -p frameplayerx -- --preview /tmp/shots --script /tmp/demo.fps
```

### Other modes

| Command | What it does |
|---|---|
| `frameplayerx --info FILE` | Prints what FramePlayerX detects about a video: format, streams, HDR |
| `frameplayerx --decode-bench FILE [--sw]` | Decodes without displaying and prints the decoder and frame rate |
| `frameplayerx --seek-test FILE\|URL` | Skips through a video headless, checking buffering and that hardware decoders are given back, and measures reads |

### Measuring stream reads

`--seek-test` reports what the demuxer read by each first picture and skip, and ends with medians. `--play SECONDS` plays first, reporting every 10 s. `--trace-reads` prints every read's offset and length, and each jump with how much was read in order before it. `--read-test SECONDS` plays nothing and just reads from the start as fast as the source delivers. `--read-ahead SECONDS` replaces the player's buffer of video read ahead.

```bash
frameplayerx --seek-test URL --rounds 1 --play 60 --seeks 8 --step 120   # playback and skips
frameplayerx --seek-test URL --rounds 5 --seeks 0 --play 0               # first picture
```

A skip slower than 2 s counts as a failure, so a slow route ends in `FAIL`; the numbers are still good. "Read speed while the buffer fills" counts only bursts of at least 16 MiB with no gap over 5 ms, so time the demuxer spends waiting with a full queue doesn't lower it, and reads served from the block cache don't raise it. A link's speed can swing 2x between runs, so compare builds in one session, on medians.

### Measuring GPU time

Every 10 s the log has a `frame GPU ms` line beside `frame CPU ms`: the GPU's time per frame (median, 95th percentile, longest) and the median of each part of a frame (`video`: an upload, a 10-bit conversion and HDR measuring; `eyes`: both eye passes; `panels`: painting the menus; `layers`: copying them into the compositor's layers), from timestamps the renderer writes between the parts. The compositor's own work isn't in it. The same work takes longer at a lower GPU clock, so the line ends with the clock. In `--preview`, the `gpu` script command prints the same line for the frames since the last one, and the run ends with one for the rest; `--size 2160` renders the eyes at the headset's size.

```bash
printf 'open clip_8k.mp4\nwait-playing 30\nwait 2\ngpu\nwait 10\ngpu\n' > /tmp/gpu.fps
frameplayerx --preview /tmp/shots --size 2160 --script /tmp/gpu.fps
```

## On the headset

```bash
tools/build-frame.sh -p frameplayerx      # cross-compile for the Frame
tools/package.sh                          # release zip and manifests in dist/
frameplayerx-install --zip dist/frameplayerx-<version>-aarch64.zip
ssh frame tail -f .local/share/frameplayerx/frameplayerx.log
```

Builds target `aarch64-unknown-linux-gnu` with glibc 2.28, so they run on
any SteamOS image. `tools/package.sh` checks that the binary is ARM64,
needs nothing newer than glibc 2.28 and finds its libraries through
`$ORIGIN/lib`.

### Environment variables

Steam can't pass environment variables to an app, so the launcher reads
`KEY=value` lines from `~/.config/frameplayerx/env` before starting.

| Variable | Default | What it does |
|---|---|---|
| `FRAMEPLAYERX_DEBUG_INPUT` | | `1` logs each controller's input when it changes |
| `FRAMEPLAYERX_V4L2` | | `0` turns off the native V4L2 decoder |
| `FRAMEPLAYERX_V4L2_DEVICE` | | The V4L2 device to decode with, instead of the one found |
| `FRAMEPLAYERX_ZERO_COPY` | | `0` copies hardware-decoded frames out of the decoder's buffers instead of reading them there, as before; for comparing the two |
| `FRAMEPLAYERX_TP10_EYES` | | `0` converts SDR 10-bit frames read in place into P010 planes instead of sampling them in the eye passes, as before; for comparing the two |

### Steam Frame notes

What the code relies on about the platform:

- SteamVR runs **on the headset** and is the OpenXR runtime. FramePlayerX
  finds the active runtime's manifest and talks to it directly, so there's
  no Khronos loader to ship.
- Native ARM64 apps run **outside the Steam Linux Runtime** container, so
  the release carries FFmpeg and dav1d in `lib/` and asks nothing newer
  than glibc 2.28 of the system.
- The Frame controllers have their own interaction profile,
  `/interaction_profiles/valve/frame_controller_valve`, from the
  `XR_VALVE_frame_controller_interaction` extension. Without it SteamVR
  presents them as Touch controllers.
- SteamVR runs each app at its per-app refresh rate:
  `XR_FB_display_refresh_rate` offers only the current rate and refuses
  requests. Setting the app's `preferredRefreshRate` with `vrcmd` changes
  the rate at once, which is how Settings › General › Display sets it. The
  eye picture size is the app's own: it makes its swapchains at the
  chosen resolution (SteamVR allows up to 8192 per eye); the per-app
  resolution only changes what SteamVR recommends.
- Turnip applies a fragment density map once per rendering bin, about 576
  pixels wide for one 8-bit target, so foveation uses a fragment shading
  rate attachment instead: 8×8-pixel texels, `R8_UINT` in optimal tiling
  only, padded to 64 texels (one 270 texels wide was misread near its
  right edge). The gaze comes from `XR_EXT_eye_gaze_interaction`.
- The hardware decoder is a V4L2 stateful decoder for 4:2:0 H.264 (8-bit)
  and HEVC (8- and 10-bit). One session takes at most 8192×4320. Every
  session open on the driver lowers that, idle ones and encoders too, so
  Steam's recording and browser can leave it busy. `/proc` shows who holds
  it. It writes 10-bit pictures correctly only as Q10C: asked for NV12, it
  clamps the samples to 8 bits.
- Turnip has no TP10 images, though the GPU reads TP10. A TP10 texture
  reads wrongly past 2^24 samples, so 10-bit planes are read in bands.
- SteamOS only mounts ext4 microSD cards itself, and no USB drives;
  FramePlayerX mounts exFAT, FAT32 and NTFS itself under
  `/run/media/steamos/<name>`.
- Apps installed in Developer Mode appear in the Steam library as non-Steam
  games; `frameplayerx-install` writes that entry to `shortcuts.vdf`.

### The platform probe

`frame-probe` answers what a Frame offers a native app in one run: the
OpenXR runtime and its extensions, refresh rates, render size, swapchain
formats, controller profiles, V4L2 decoders, Vulkan Video and DMA-BUF
import. `tools/frame-probe.sh --build` builds it, runs it on a paired
headset and copies the text and JSON reports into `probe-reports/`.

## Releasing

1. Once: run `fp-release keygen`, paste the printed public key into
   `crates/fp-updater/src/lib.rs` (`RELEASE_PUBLIC_KEY`), and store the
   whole `.key` file as the `FP_RELEASE_KEY` repository secret. Keep the key
   offline otherwise.
2. Stable: push (or merge) to `main`. Beta: run the `ci` workflow on the
   `beta` branch from the Actions tab (Run workflow). CI calculates the
   version, stamps it into `crates/frameplayerx/Cargo.toml`, builds,
   packages, signs `manifest.json` and publishes the GitHub release with
   the zip, `manifest.json(.sig)`, `framedrop.json`, `frameplayerx-install`
   for each PC, and the Desktop Mode installer and its install and uninstall launchers.

   - `main` gets the next stable `X.Y.Z` from the conventional commits
     since the last stable tag (`git cliff --bumped-version`): a breaking
     change (`feat!:`, `BREAKING CHANGE:`) bumps X, a `feat` bumps Y,
     anything else bumps Z. Beta tags don't count. With no stable tag yet
     it's `initial_tag` in `cliff.toml`.
   - A push to `main` only releases if something that ships changed since
     the last stable tag. Markdown, `docs/` and `site/` alone run nothing, on
     pull requests too. `.github/`, `cliff.toml` and git's own files also
     build, but don't release. A tag or a run from the Actions tab always
     goes all the way.
   - Other branches get that version as `X.Y.Z-beta.N` (N one above the
     highest existing beta of it, and always above the latest stable),
     published as a GitHub pre-release. FramePlayerX offers it on the Beta
     update channel only.
   - Alpha: tick **Alpha** when running the workflow (any branch, `main`
     too) to get `X.Y.Z-alpha.N`, N being the run number, left as a draft
     pre-release. Only people with push access see a draft, and the app
     and `frameplayerx-install` never offer one, so install its zip by hand
     to test it, then run the workflow again without Alpha to release a
     beta (or merge for a stable release). Delete the draft once done; a
     draft has no tag until it is published.
   - Pushing a `vX.Y.Z` or `vX.Y.Z-anything` tag releases exactly that
     version (a pre-release if it has a `-`).

   The release starts as a draft: it is published, and becomes the latest
   unless it's a beta, only once every file is in and downloads back
   identical to the one built (an alpha stays a draft). A run that fails
   leaves a draft nobody sees, which the next run of that version replaces.

CI runs only GitHub's own Actions. Dependabot opens one pull request a
month with the crates' compatible updates (`Cargo.lock` only, within what
`Cargo.toml` allows), Rust (`rust-toolchain.toml`), the tools
(`tools/requirements.txt`) and the Actions. CI tests it like any pull
request; when a new Rust's clippy finds something, fix it on that branch.
Merging it releases, as any merge to `main` does. A breaking crate upgrade
(a new egui, say) needs code changes, so it's made by hand.

The app and `frameplayerx-install` find releases in GitHub's release list:
the highest version on the Stable channel without a `-` part, on Beta any
version (`releases/latest` never offers a pre-release). In-app updates
and `frameplayerx-install` without `--zip` only install releases whose
manifest is signed with the release key.

### Install buttons

The README's install buttons are images in `assets/`, because GitHub strips
the inline styles and SVG of the HTML buttons FrameDrop and Frame Control
publish. They all point at the latest release, so they need no change per
release:

| Button | Opens |
|---|---|
| Desktop Mode / Uninstall | The release's `Install-FramePlayerX.desktop` / `Uninstall-FramePlayerX.desktop` |
| FrameDrop | `https://framedropvr.com/install?manifest=` with the release's `framedrop.json`, FrameDrop's documented link: it opens FrameDrop, or FrameDrop's site when it isn't installed |
| Frame Control | `site/frame-control.html` on GitHub Pages, which opens `frame-control://install?manifest=` with the release's `frameplayerx.json`. GitHub doesn't keep links to app schemes like `frame-control://` in Markdown, so a web page has to |

`.github/workflows/pages.yml` publishes `site/` (with the logo) when it
changes on `main`. Once, in the repository settings: **Pages › Build and
deployment › Source: GitHub Actions**.

### Crash reports

The log records panics and fatal signals. When the previous run's log
(`frameplayerx.log.old`) ends in one, the next start shows a page saying
what a crash report contains and where it goes, with the report itself on
request; only **Send report** sends it. The report holds the app and
system versions, what happened, and that run's log (its first 8 KB and
last 40 KB) with every video title and location in the library, everything
in `sources.json` (libraries included), haptic devices, URLs, IP addresses,
email addresses, paths under home and media folders, and media file names
replaced by placeholders such as `<video 1>` (`crates/frameplayerx/src/crash.rs`).

It goes to `tools/crash-relay/`, a Cloudflare Worker that files it as an
issue in a private repository, so the GitHub token never ships in the app.
Without a relay (`REPORT_URL` is `None`), the app never asks. To set one
up, once:

1. Create a private repository for the reports (`REPO` in
   `tools/crash-relay/wrangler.toml`, `frameplayerx-crashes` by default).
2. Create a fine-grained GitHub token with access to that repository only
   and the **Issues: Read and write** permission.
3. In `tools/crash-relay/`, with a Cloudflare account:
   `npx wrangler login`, `npx wrangler secret put GITHUB_TOKEN` (paste the
   token), then `npx wrangler deploy`. Leave the Worker's logs off: the
   relay only uses the sender's address to rate-limit them (5 a minute).
4. Set `REPORT_URL` in `crates/frameplayerx/src/crash.rs` to the deployed
   URL plus `/report`, e.g. `Some("https://frameplayerx-crash-relay.<account>.workers.dev/report")`.

Check it with
`curl -X POST <url>/report -d '{"app":"frameplayerx","version":"test","summary":"test","text":"test"}'`:
`201` and a new issue. The preview's `crash-report` command shows the page
with a report made from its own log.

## Artwork

`assets/logo.svg` is the logo. `python3 tools/make-artwork.py` renders the
Steam library artwork in `assets/steam/` from it (needs Pillow and
`rsvg-convert`); re-run it after changing the logo.
