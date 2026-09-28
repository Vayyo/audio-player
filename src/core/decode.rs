//! Decoding of local audio files into interleaved, normalized `f32` PCM.
//!
//! All formats and codecs that the MVP needs (wav, mp3, flac, ogg/vorbis) are
//! handled by `symphonia` with the feature set enabled in `Cargo.toml`.

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag, Tag};

/// Decoded audio: interleaved normalized `f32` samples plus the native stream layout.
///
/// The samples are owned by this struct; nothing borrows the decoder or the file.
#[derive(Clone, Debug)]
pub struct DecodedAudio {
    /// Short name of the demuxer that produced the samples (for example `wave`, `ogg`).
    pub format: String,
    /// Standard artist metadata, if the container supplies a nonblank value.
    pub artist: Option<String>,
    /// Sample rate of the stream in Hz.
    pub rate: u32,
    /// Number of channels of the stream.
    pub channels: u16,
    /// Channel-interleaved samples, exactly `frames * channels` long.
    pub samples: Vec<f32>,
}

impl DecodedAudio {
    /// Number of whole frames in the buffer. Zero if no channels were reported.
    pub fn frames(&self) -> u64 {
        if self.channels == 0 {
            return 0;
        }
        (self.samples.len() / usize::from(self.channels)) as u64
    }

    /// Duration in seconds. Zero if the rate is unknown (never for decoded audio).
    pub fn duration_secs(&self) -> f64 {
        if self.rate == 0 {
            return 0.0;
        }
        self.frames() as f64 / f64::from(self.rate)
    }

    /// Largest absolute sample value; `0.0` for an empty buffer.
    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
    }
}

/// Failure modes of [`decode_file`].
#[derive(Debug)]
pub enum DecodeError {
    /// The file could not be opened or read, including a truncated container during probing.
    Io(std::io::Error),
    /// No registered demuxer recognized the container.
    UnsupportedFormat(String),
    /// The container exposes no decodable audio track.
    NoAudioTrack,
    /// No registered decoder supports the track's codec.
    UnsupportedCodec(String),
    /// The stream ended without yielding a single audio frame.
    NoAudioData,
    /// The stream is corrupt, or its layout changed mid-stream.
    Decode(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "cannot read file: {err}"),
            Self::UnsupportedFormat(name) => write!(f, "unsupported container format: {name}"),
            Self::NoAudioTrack => f.write_str("no audio track in the file"),
            Self::UnsupportedCodec(name) => write!(f, "unsupported audio codec: {name}"),
            Self::NoAudioData => f.write_str("the stream produced no audio frames"),
            Self::Decode(msg) => write!(f, "decode error: {msg}"),
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

fn artist_in_tags(tags: &[Tag]) -> Option<String> {
    tags.iter().find_map(|tag| match &tag.std {
        Some(StandardTag::Artist(value)) if !value.trim().is_empty() => Some(value.trim().to_owned()),
        _ => None,
    })
}

fn artist_in_revision(revision: Option<&MetadataRevision>, track_id: u32) -> Option<String> {
    let revision = revision?;
    revision.per_track.iter()
        .find(|track| track.track_id == u64::from(track_id))
        .and_then(|track| artist_in_tags(&track.metadata.tags))
        .or_else(|| artist_in_tags(&revision.media.tags))
}

/// Decode the audio stream of `path` into interleaved normalized `f32` samples.
///
/// The container format is probed with the file extension as a hint, so all MVP
/// formats (wav, mp3, flac, ogg/vorbis) work through a single call. Packets that
/// fail to decode are skipped. An unexpected end of file ends the stream normally
/// and returns the frames decoded so far; other demux errors are fatal.
/// [`SymphoniaError::ResetRequired`] also ends the stream (the MVP has no
/// chained-stream support).
pub fn decode_file(path: impl AsRef<Path>) -> Result<DecodedAudio, DecodeError> {
    let path = path.as_ref();

    let file = File::open(path).map_err(DecodeError::Io)?;
    let source = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }

    let mut format = symphonia::default::get_probe()
        .probe(&hint, source, FormatOptions::default(), MetadataOptions::default())
        .map_err(|err| match err {
            SymphoniaError::IoError(err) => DecodeError::Io(err),
            SymphoniaError::DecodeError(err) => DecodeError::Decode(err.to_string()),
            err => DecodeError::UnsupportedFormat(err.to_string()),
        })?;

    // Copy everything the decode loop needs out of the borrowed track so that the
    // format reader can be used mutably afterwards.
    let (track_id, codec_params, format_name) = {
        let track = format.default_track(TrackType::Audio).ok_or(DecodeError::NoAudioTrack)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or(DecodeError::NoAudioTrack)?
            .clone();
        (track.id, params, format.format_info().short_name.to_owned())
    };
    let artist = artist_in_revision(format.metadata().current(), track_id);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|err| DecodeError::UnsupportedCodec(err.to_string()))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut layout: Option<(u32, u16)> = None;

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => break,
            // Symphonia 0.6 has no dedicated EOF error variant: IoError with
            // UnexpectedEof means the container ended mid-stream. Keep decoded frames.
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(DecodeError::Decode(err.to_string())),
        };

        if packet.track_id != track_id {
            continue;
        }

        let buffer = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // Damaged or truncated packets are skipped, matching symphonia's own
            // example code; the rest of the stream stays decodable.
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(DecodeError::Decode(err.to_string())),
        };

        let rate = buffer.spec().rate();
        let channel_count = buffer.spec().channels().count();
        let channels = u16::try_from(channel_count)
            .map_err(|_| DecodeError::Decode(format!("{channel_count} channels exceed u16")))?;
        if rate == 0 || channels == 0 {
            return Err(DecodeError::Decode("stream reports zero rate or channels".to_owned()));
        }

        match layout {
            None => layout = Some((rate, channels)),
            Some(current) if current != (rate, channels) => {
                return Err(DecodeError::Decode("sample layout changed mid-stream".to_owned()));
            }
            Some(_) => {}
        }

        // `copy_to_vec_interleaved` resizes instead of appending, so copy through
        // a reused scratch buffer and append the frames to the result.
        scratch.resize(buffer.samples_interleaved(), 0.0);
        buffer.copy_to_slice_interleaved(&mut scratch[..]);
        samples.extend_from_slice(&scratch);
    }

    let (rate, channels) = layout.ok_or(DecodeError::NoAudioData)?;
    if samples.is_empty() {
        return Err(DecodeError::NoAudioData);
    }

    let mut metadata = format.metadata();
    let mut artist = artist;
    let mut id3v2_artist = None;
    while let Some(revision) = metadata.current() {
        if let Some(value) = artist_in_revision(Some(revision), track_id) {
            if revision.info.short_name == "id3v2" {
                id3v2_artist = Some(value);
            } else {
                artist = Some(value);
            }
        }
        if metadata.pop().is_none() {
            break;
        }
    }
    let artist = id3v2_artist.or(artist);
    Ok(DecodedAudio { format: format_name, artist, rate, channels, samples })
}

#[cfg(test)]
mod tests {
    use super::{DecodeError, decode_file};
    use std::f64::consts::TAU;
    use std::path::{Path, PathBuf};

    /// Path of a committed binary fixture.
    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
    }

    /// A file in the temporary directory that deletes itself on drop, so that
    /// neither a passing nor a failing test leaves anything behind.
    struct TempFile(PathBuf);

    impl TempFile {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            // Best effort: a failed unlink must not mask the test result.
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// First `keep` bytes of a committed fixture, written to a temp file.
    fn truncated_fixture(name: &str, keep: usize) -> TempFile {
        let data = std::fs::read(fixture(name)).expect("read fixture");
        let file = TempFile(
            std::env::temp_dir()
                .join(format!("audio-player-truncated-{}-{name}", std::process::id())),
        );
        std::fs::write(file.path(), &data[..keep]).expect("write truncated fixture");
        file
    }

    /// Write a 1 second, 44100 Hz, mono, 440 Hz sine WAV file and return it.
    fn write_wav_fixture(name: &str) -> TempFile {
        const RATE: u32 = 44_100;
        const FRAMES: u32 = RATE;

        let file = TempFile(std::env::temp_dir().join(format!(
            "audio-player-{name}-{}.wav",
            std::process::id()
        )));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(file.path(), spec).expect("create wav fixture");
        for frame in 0..FRAMES {
            let phase = TAU * 440.0 * f64::from(frame) / f64::from(RATE);
            let sample = (phase.sin() * f64::from(i16::MAX) * 0.8) as i16;
            writer.write_sample(sample).expect("write sample");
        }
        writer.finalize().expect("finalize wav fixture");
        file
    }

    #[test]
    fn decodes_generated_wav() {
        let file = write_wav_fixture("wav");
        let decoded = decode_file(file.path()).expect("wav decodes");

        assert_eq!(decoded.rate, 44_100);
        assert_eq!(decoded.artist, None);
        assert_eq!(decoded.channels, 1);
        // hound writes exactly 44100 mono frames; mono means one sample per frame.
        assert_eq!(decoded.samples.len(), 44_100);
        // PCM in a WAV container is exact, but keep a small tolerance anyway.
        assert!(
            (44_000..=44_200).contains(&decoded.frames()),
            "unexpected frame count {}",
            decoded.frames()
        );
        assert!(decoded.peak() > 0.5, "signal degenerated: peak {}", decoded.peak());
        assert!(decoded.samples.iter().all(|s| s.is_finite()));
        assert!(decoded.duration_secs() > 0.9 && decoded.duration_secs() < 1.1);
    }

    /// MP3, FLAC and OGG fixtures: same 1 s / 44100 Hz / mono / 440 Hz tone.
    fn assert_tone_fixture(name: &str) {
        let decoded = decode_file(fixture(name)).unwrap_or_else(|err| panic!("{name}: {err}"));

        assert_eq!(decoded.channels, 1, "{name}: channels");
        assert_eq!(decoded.rate, 44_100, "{name}: rate");
        assert!(decoded.frames() > 0, "{name}: no frames");
        // Encoder delay/padding moves the frame count by up to a few thousand
        // frames, so only the rough duration is checked.
        let duration = decoded.duration_secs();
        assert!((0.5..1.5).contains(&duration), "{name}: duration {duration}");
        assert!(decoded.peak() > 0.1, "{name}: signal degenerated, peak {}", decoded.peak());
        assert!(decoded.samples.iter().all(|s| s.is_finite()), "{name}: non-finite sample");
    }

    #[test]
    fn decodes_mp3_fixture() {
        assert_tone_fixture("tone.mp3");
    }

    #[test]
    fn decodes_standard_artist_from_id3_without_changing_pcm() {
        let source = std::fs::read(fixture("tone.mp3")).expect("read mp3 fixture");
        let original = decode_file(fixture("tone.mp3")).expect("decode original mp3");
        for (artist, expected) in [("Тестовый артист", Some("Тестовый артист")), ("   ", None)] {
            let payload_len = 1 + artist.len();
            let frame_len = 10 + payload_len;
            let file = TempFile(std::env::temp_dir().join(format!(
                "audio-player-tagged-{}.mp3", std::process::id()
            )));
            let mut tagged = Vec::with_capacity(10 + frame_len + source.len());
            tagged.extend_from_slice(b"ID3\x04\x00\x00");
            tagged.extend_from_slice(&[0, 0, (frame_len >> 7) as u8, (frame_len & 0x7f) as u8]);
            tagged.extend_from_slice(b"TPE1");
            tagged.extend_from_slice(&(payload_len as u32).to_be_bytes());
            tagged.extend_from_slice(&[0, 0, 3]); // ID3v2.4 flags + UTF-8 text encoding.
            tagged.extend_from_slice(artist.as_bytes());
            tagged.extend_from_slice(&source);
            if !artist.trim().is_empty() {
                // Trailing ID3v1 is older than the leading ID3v2 artist.
                let mut id3v1 = [b' '; 128];
                id3v1[..3].copy_from_slice(b"TAG");
                id3v1[33..39].copy_from_slice(b"Legacy");
                id3v1[127] = 0;
                tagged.extend_from_slice(&id3v1);
            }
            std::fs::write(file.path(), tagged).expect("write tagged mp3");

            let decoded = decode_file(file.path()).expect("decode tagged mp3");
            assert_eq!(decoded.artist.as_deref(), expected);
            assert_eq!(decoded.rate, original.rate);
            assert_eq!(decoded.channels, original.channels);
            assert_eq!(decoded.samples, original.samples);
        }
    }


    #[test]
    fn decodes_flac_fixture() {
        assert_tone_fixture("tone.flac");
    }

    #[test]
    fn decodes_ogg_fixture() {
        assert_tone_fixture("tone.ogg");
    }

    #[test]
    fn corrupt_recognized_wav_is_a_decode_error() {
        let file = TempFile(std::env::temp_dir().join(format!(
            "audio-player-corrupt-header-{}.wav",
            std::process::id()
        )));
        std::fs::write(file.path(), b"RIFF\x00\x00\x00\x00WAVE")
            .expect("write corrupt wav fixture");

        assert!(matches!(decode_file(file.path()), Err(DecodeError::Decode(_))));
    }

    #[test]
    fn truncated_ogg_container_is_an_io_error() {
        let path = fixture("tone.ogg");
        let keep = std::fs::metadata(path).expect("ogg fixture metadata").len() as usize / 2;
        let file = truncated_fixture("tone.ogg", keep);
        // The container is recognized; the file is simply unreadable to the end.
        assert!(matches!(decode_file(file.path()), Err(DecodeError::Io(_))));
    }

    #[test]
    fn truncated_flac_keeps_decoded_frames() {
        let path = fixture("tone.flac");
        let full = decode_file(&path).expect("full flac decodes");
        let keep = std::fs::metadata(path).expect("flac fixture metadata").len() as usize / 2;
        let file = truncated_fixture("tone.flac", keep);
        let partial = decode_file(file.path()).expect("truncated flac keeps decoded frames");

        assert_eq!(partial.rate, full.rate);
        assert_eq!(partial.channels, full.channels);
        assert!(partial.frames() > 0 && partial.frames() < full.frames());
        assert!(partial.peak() > 0.1);
        assert!(partial.samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn truncated_ogg_keeps_decoded_frames() {
        let path = fixture("tone-long.ogg");
        let full = decode_file(&path).expect("full ogg decodes");
        assert_eq!(full.rate, 44_100);
        assert_eq!(full.channels, 1);
        assert!((2.5..3.5).contains(&full.duration_secs()));

        // Keep complete audio pages, then cut into the next one.
        let keep = std::fs::metadata(path).expect("ogg fixture metadata").len() as usize * 3 / 4;
        let file = truncated_fixture("tone-long.ogg", keep);
        let partial = decode_file(file.path()).expect("truncated ogg keeps decoded frames");
        assert!(partial.frames() > 0 && partial.frames() < full.frames());
        assert!(partial.samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = fixture("does-not-exist.mp3");
        assert!(matches!(decode_file(&path), Err(DecodeError::Io(_))));
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        let file = TempFile(
            std::env::temp_dir().join(format!("audio-player-garbage-{}.wav", std::process::id())),
        );
        std::fs::write(file.path(), b"not audio at all").expect("write garbage");
        assert!(decode_file(file.path()).is_err());
    }

    #[test]
    fn truncated_stream_decodes_what_is_there() {
        // The first half of a real mp3: the header still probes, symphonia's mp3
        // reader reports the end of the stream early, and everything decoded so
        // far must be returned instead of an error.
        let data = std::fs::read(fixture("tone.mp3")).expect("read mp3 fixture");
        let file = TempFile(
            std::env::temp_dir()
                .join(format!("audio-player-truncated-{}.mp3", std::process::id())),
        );
        std::fs::write(file.path(), &data[..data.len() / 2]).expect("write truncated fixture");

        let decoded = decode_file(file.path()).expect("a truncated stream still yields audio");
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.rate, 44_100);
        assert!(
            decoded.frames() > 0 && decoded.frames() < 44_100,
            "expected a partial stream, got {} frames",
            decoded.frames()
        );
    }
}
