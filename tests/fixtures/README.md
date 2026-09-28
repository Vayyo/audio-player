# Binary decode fixtures

The original three fixtures are one second of a 440 Hz sine, 44100 Hz, mono at
roughly 0.75 full scale (`volume=6`, since ffmpeg's `sine` source outputs 1/8
amplitude). The WAV test fixture is the same signal, generated with `hound`.

Generated with `ffmpeg` (9.0.2, libmp3lame / libvorbis) from the repository root:

```
ffmpeg -hide_banner -y -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1,volume=6" -ac 1 -c:a libmp3lame -b:a 128k tests/fixtures/tone.mp3
ffmpeg -hide_banner -y -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1,volume=6" -ac 1 -c:a flac -sample_fmt s16 tests/fixtures/tone.flac
ffmpeg -hide_banner -y -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1,volume=6" -ac 1 -c:a libvorbis -q:a 5 tests/fixtures/tone.ogg
ffmpeg -hide_banner -y -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=3,volume=6" -ac 1 -c:a libvorbis -q:a 5 tests/fixtures/tone-long.ogg
```

* `tone.mp3` — MPEG audio, demuxed/decoded by `symphonia` (`mp3` feature).
* `tone.flac` — native FLAC (default `symphonia` feature).
* `tone.ogg` — Ogg Vorbis (default `symphonia` features).
* `tone-long.ogg` — the same signal for three seconds, with multiple audio
  pages so truncation can preserve decoded frames before the cut page.

The WAV fixture is not committed: `src/core/decode.rs` writes it into
`std::env::temp_dir()` with `hound` at test time.

Tests treat the lossy fixtures leniently: they assert the rate, the channel
count, a non-empty, non-clipping buffer with a non-zero peak and a rough duration,
never an exact frame count (encoder delay and padding shift the frame count by
up to a few thousand frames).
