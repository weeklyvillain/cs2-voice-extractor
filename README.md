# cs2-voice-extractor

[![Release](https://github.com/weeklyvillain/cs2-voice-extractor/actions/workflows/release.yml/badge.svg)](https://github.com/weeklyvillain/cs2-voice-extractor/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/weeklyvillain/cs2-voice-extractor)](https://github.com/weeklyvillain/cs2-voice-extractor/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pull player voice chat out of **Counter-Strike 2** demos, fast, without breaking every time Valve ships an update.

- ⚡ **Fast.** A 175 MB match demo is done in about **0.1 seconds**.
- 🛡️ **Update-proof by design.** It never parses entities, string tables or game events, only the voice messages.
- 🎧 **Lossless.** The original Opus voice packets are copied into `.ogg` as-is, with no re-encoding.
- 📦 **One small binary.** No runtime dependencies on Windows or Linux. ffmpeg is optional, for other formats.

## Download

Grab the latest build for your platform from the
[**Releases page**](https://github.com/weeklyvillain/cs2-voice-extractor/releases/latest):

| Platform | File |
|---|---|
| Windows x64 | `cs2-voice-extractor-windows-x64.zip` |
| Linux x64 (static, any distro) | `cs2-voice-extractor-linux-x64.tar.gz` |

Or build it yourself with `cargo install --git https://github.com/weeklyvillain/cs2-voice-extractor`.

## Usage

```sh
# Who talked, and for how long?
cs2-voice-extractor match.dem --list

# One file per player, only the voice (silence removed)
cs2-voice-extractor match.dem -o voice/

# One specific player, voice placed at its real time across the whole demo
cs2-voice-extractor match.dem -s 76561197962803240 -m full -o voice/

# Everyone mixed into a single mp3 (requires ffmpeg)
cs2-voice-extractor match.dem -m merge -f mp3 -o voice/
```

Files are named `<demo>_<steamid64>.<ext>`, or `<demo>.<ext>` in merge mode.

### Modes

| Mode | Output |
|---|---|
| `compact` *(default)* | One file per player with only their voice, back to back. Aliases: `remove-empty`, `split-compact`. |
| `full` | One file per player as long as the demo. Voice sits at the moment it was spoken, so it lines up with demo playback. Alias: `split-full`. |
| `merge` | All selected players mixed into one file as long as the demo. Requires ffmpeg. Alias: `single-full`. |

### Options

| Option | Default | Description |
|---|---|---|
| `-s, --steam-id <ID>` | everyone | SteamID64 to extract. Repeat the flag or comma-separate. |
| `-m, --mode <MODE>` | `compact` | See [Modes](#modes). |
| `-f, --format <EXT>` | `ogg` | `ogg`/`opus` are written natively. Anything else ffmpeg can write (`mp3`, `wav`, `flac`, `m4a`, ...) is converted through ffmpeg. |
| `-o, --out <DIR>` | `.` | Output directory, created if missing. |
| `-l, --list` | | List players with voice, don't write files. |
| `--json` | | Print a machine-readable summary to stdout. Logs go to stderr. |
| `--ffmpeg <PATH>` | `ffmpeg` | ffmpeg binary, if it's not on `PATH`. |

Multiple demos can be passed at once.

### Scripting

`--json` prints one object per demo:

```json
[{"demo":"match.dem","map":"de_anubis","duration_seconds":1688.20,"tick_interval":0.015625,
  "players":[{"steam_id":"76561197962803240","voice_seconds":10.61,"file":"voice/match_76561197962803240.ogg"}]}]
```

Exit codes: `0` success · `1` error · `2` no voice found.

## How it works

Most tools run a full demo parser just to reach the voice messages. That is slow, and it
breaks whenever Valve changes something unrelated, like a new entity field or a new
string table. This tool does the minimum:

```
.dem file
 └─ frames (cmd, tick, size, snappy)        ← stable container, unchanged since CS2 launch
     └─ DEM_Packet / DEM_SignonPacket
         └─ net messages (id, size, bytes)  ← everything is skipped by length…
             ├─ svc_ServerInfo  (40)        ← …except the tick interval
             └─ svc_VoiceData   (47)        ← …and the voice
                 └─ SteamID64 + Opus packets ──► remuxed straight into Ogg Opus
```

- The protobuf reader is about 60 lines and reads only the handful of fields it needs. New fields are ignored.
- Audio is never decoded. Gaps (silence in `full` mode, dropped packets) are filled with
  Opus frames that decode to digital silence.
- Timing comes from the demo tick each message was recorded at, so `full`-mode files line up with demo playback.

Supported voice formats:

| Format | Demos |
|---|---|
| `VOICEDATA_FORMAT_OPUS` | CS2 since the Arms Race update (Feb 2024) |
| `VOICEDATA_FORMAT_STEAM` | Earlier CS2 demos |

CS:GO demos (`HL2DEMO`) are not supported.

### If an update does break it

Only two things could realistically change: the `svc_VoiceData` message id and the
`CMsgVoiceAudio` field numbers. Both are constants at the top of [`src/demo.rs`](src/demo.rs).
Please [open an issue](https://github.com/weeklyvillain/cs2-voice-extractor/issues) with a demo that fails.

## Building

```sh
cargo build --release
cargo test
```

The project is also a library crate (`cs2_voice_extractor`): `demo::scan` returns the raw voice
messages and `voice::render` writes them to Ogg Opus.

## Releases

Every push to `master` builds Windows and Linux binaries and publishes a GitHub release.
The version is `<major>.<minor>` from `Cargo.toml` plus the CI run number as the patch,
for example `v0.1.12`.

## Credits

Inspired by [akiver/csgo-voice-extractor](https://github.com/akiver/csgo-voice-extractor),
which documented the Steam voice framing used by older CS2 demos.

## License

[MIT](LICENSE)
