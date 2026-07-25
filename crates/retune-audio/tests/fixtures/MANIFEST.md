# Retune local-playback audio fixtures

All audio content comes from Wikimedia Commons [`Audio.wav`](https://commons.wikimedia.org/wiki/File:Audio.wav), created by Bamjos and released under the [CC0 1.0 Universal Public Domain Dedication](https://creativecommons.org/publicdomain/zero/1.0/). CC0 permits copying, modification, distribution, performance, and commercial use without permission or attribution.

| Filename | Exact format / container | Source URL | License / reuse | Caveat |
|---|---|---|---|---|
| `cc0-audio.mp3` | MPEG-1 Layer III audio / MP3 | https://upload.wikimedia.org/wikipedia/commons/transcoded/b/b5/Audio.wav/Audio.wav.mp3 | CC0 1.0 (official Wikimedia transcode of the CC0 source) | Downloaded directly; 44.1 kHz stereo. |
| `cc0-audio.flac` | FLAC audio / native FLAC container | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable FLAC fixture was used. |
| `cc0-audio-vorbis.ogg` | Vorbis audio / Ogg container | https://upload.wikimedia.org/wikipedia/commons/transcoded/b/b5/Audio.wav/Audio.wav.ogg | CC0 1.0 (official Wikimedia transcode of the CC0 source) | Downloaded directly; 44.1 kHz stereo. |
| `cc0-audio-aac-lc.aac` | AAC Low Complexity / raw ADTS AAC | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable raw AAC-LC fixture was found. |
| `cc0-audio-aac-lc.m4a` | AAC Low Complexity / MPEG-4 audio (`.m4a`) | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable AAC-LC/M4A fixture was found. |
| `cc0-audio-alac.m4a` | Apple Lossless Audio Codec / MPEG-4 audio (`.m4a`) | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable ALAC/M4A fixture was found. |
| `cc0-audio.wav` | 16-bit little-endian PCM / RIFF WAVE | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 | Original file downloaded directly; 48 kHz stereo. |
| `cc0-audio.aiff` | 16-bit big-endian PCM / AIFF | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable AIFF fixture was found. |
| `cc0-audio-opus.ogg` | Opus audio / Ogg container | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable Ogg Opus fixture was found. |
| `cc0-audio-opus.webm` | Opus audio / audio-only WebM container | https://upload.wikimedia.org/wikipedia/commons/b/b5/Audio.wav | CC0 1.0 derivative | Locally transcoded from the source WAV with FFmpeg 8.1.2; no separate clearly asset-licensed downloadable WebM Opus fixture was found. |

## Verification

Verified locally with `ffprobe` from FFmpeg 8.1.2 and `file` 5.41. Every file contains one valid stereo audio stream and is approximately 2.4 seconds long. `ffprobe` reported the expected codecs and containers: `mp3/mp3`, `flac/flac`, `vorbis/ogg`, `aac (LC)/aac`, `aac (LC)/mov,mp4,m4a`, `alac/mov,mp4,m4a`, `pcm_s16le/wav`, `pcm_s16be/aiff`, `opus/ogg`, and `opus/matroska,webm`.

No planned format is missing. The seven entries marked as local transcodes are included because targeted searches did not turn up small, pre-encoded downloads with equally clear asset-level reuse terms; their underlying audio remains CC0.

## Retune-added fixtures

| Filename | Purpose | Provenance |
|---|---|---|
| `cc0-audio-tagged.mp3` | Known-tag import tests: ID3v2.3 title/artist/album/genre/track 7/disc 2 + embedded PNG front cover | `cc0-audio.mp3` retagged with FFmpeg 8.1.2; cover is the Retune app icon (project asset) resized to 64px |
| `cc0-audio-tagged.flac` | Known-tag import tests: Vorbis comments (same values) + embedded PNG picture block | `cc0-audio.flac` retagged with FFmpeg 8.1.2 |
| `cc0-audio-tagged.m4a` | Known-tag import tests: MP4 atoms (same values), no artwork | `cc0-audio-aac-lc.m4a` retagged with FFmpeg 8.1.2 |
| `not-audio.mp3` | Negative fixture: 4KB of random bytes with an audio extension; must fail probe without aborting a batch | `/dev/urandom` |

Tag values are identical across the three tagged fixtures: title "Fixture Song",
artist "Fixture Artist", album "Fixture Album", genre "Fixture Genre", track 7, disc 2.
