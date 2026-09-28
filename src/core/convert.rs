//! Conversion of decoded PCM to the parameters of the output device.
//!
//! Sample values stay normalized `f32` until the very last step, where they are
//! packed into the device's sample format. Nothing here panics: every length is
//! computed with checked arithmetic and every index is accessed through `get`.

use std::error::Error;
use std::fmt;

use symphonia::core::audio::conv::FromSample;
use symphonia::core::audio::sample::{Sample, SampleFormat};

/// Interleaved `f32` PCM together with its current layout.
#[derive(Clone, Debug)]
pub struct PcmBuffer {
    /// Sample rate in Hz.
    pub rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Channel-interleaved samples, `frames * channels` long.
    pub samples: Vec<f32>,
}

impl PcmBuffer {
    /// Wrap already interleaved samples. The buffer is not validated here;
    /// [`PcmBuffer::frames`] derives the frame count by integer division.
    pub fn new(rate: u32, channels: u16, samples: Vec<f32>) -> Self {
        Self { rate, channels, samples }
    }

    /// Number of whole frames; `0` if the channel count is zero.
    pub fn frames(&self) -> u64 {
        if self.channels == 0 {
            return 0;
        }
        (self.samples.len() / usize::from(self.channels)) as u64
    }
}

/// Parameters of the output stream that PCM has to be converted to.
#[derive(Clone, Copy, Debug)]
pub struct DeviceFormat {
    /// Device rate in Hz.
    pub rate: u32,
    /// Device channel count.
    pub channels: u16,
    /// Device sample format.
    pub sample_format: SampleFormat,
}

/// Historical S1 test format; the live output configuration is queried from CPAL.
pub const MVP_DEVICE: DeviceFormat =
    DeviceFormat { rate: 96_000, channels: 2, sample_format: SampleFormat::F32 };

/// Failure modes of [`convert`] and [`convert_to_sample_format`].
#[derive(Debug)]
pub enum ConvertError {
    /// The source or target rate is zero, so the resampling ratio is undefined.
    ZeroRate,
    /// The source or target channel count is zero, so there is nothing to mix.
    ZeroChannels,
    /// The resulting sample count does not fit into `usize`.
    TooManySamples,
    /// The engine does not pack this sample format.
    UnsupportedSampleFormat(SampleFormat),
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroRate => f.write_str("sample rate is zero"),
            Self::ZeroChannels => f.write_str("channel count is zero"),
            Self::TooManySamples => f.write_str("converted sample count does not fit into usize"),
            Self::UnsupportedSampleFormat(format) => {
                write!(f, "sample format {format:?} is not supported")
            }
        }
    }
}

impl Error for ConvertError {}

/// Convert `src` to the rate and channel count of `target`.
///
/// Sample values stay interleaved `f32`; use [`convert_to_sample_format`] for
/// the device's sample format.
///
/// # Channel rule
///
/// * Equal channel counts: the samples are copied unchanged.
/// * More target channels: source channels are repeated cyclically, so
///   `out[frame][c] = in[frame][c % src_channels]` (mono to stereo duplicates
///   the single channel into both speakers).
/// * Fewer target channels: the source channels are split into
///   `target_channels` contiguous groups of nearly equal size and each group is
///   averaged. With `S` source and `D` target channels group `c` covers the
///   source channels `c * S / D .. (c + 1) * S / D` (integer division), which is
///   never empty because `S >= D`.
///
/// # Rate rule
///
/// The output has `round(src_frames * target_rate / src_rate)` frames. Output
/// frame `i` is read from the source position `pos = i * src_rate / target_rate`,
/// linearly interpolated between the frames `floor(pos)` and `floor(pos) + 1`
/// (as `f64`, clamped to the last source frame).
pub fn convert(src: &PcmBuffer, target: &DeviceFormat) -> Result<PcmBuffer, ConvertError> {
    // Either side may be zero, so the message must not blame the source.
    if src.rate == 0 || target.rate == 0 {
        return Err(ConvertError::ZeroRate);
    }
    if src.channels == 0 || target.channels == 0 {
        return Err(ConvertError::ZeroChannels);
    }
    if src.rate == target.rate {
        return mix_channels(src, target.channels);
    }
    if src.channels == target.channels {
        return resample(src, target.rate);
    }
    let mixed = mix_channels(src, target.channels)?;
    resample(&mixed, target.rate)
}

/// Apply the channel rule documented on [`convert`].
fn mix_channels(src: &PcmBuffer, target_channels: u16) -> Result<PcmBuffer, ConvertError> {
    let src_channels = usize::from(src.channels);
    let dst_channels = usize::from(target_channels);

    if src_channels == dst_channels {
        return Ok(PcmBuffer::new(src.rate, target_channels, src.samples.clone()));
    }

    let capacity = usize::try_from(src.frames())
        .ok()
        .and_then(|frames| frames.checked_mul(dst_channels))
        .ok_or(ConvertError::TooManySamples)?;

    let mut samples: Vec<f32> = Vec::new();
    samples.try_reserve_exact(capacity).map_err(|_| ConvertError::TooManySamples)?;

    for frame in src.samples.chunks_exact(src_channels) {
        if dst_channels > src_channels {
            // Cyclic repetition: every target channel borrows a source channel.
            for channel in 0..dst_channels {
                samples.push(frame[channel % src_channels]);
            }
        } else {
            // src_channels >= dst_channels here, so each group holds at least one channel.
            for channel in 0..dst_channels {
                let start = channel * src_channels / dst_channels;
                let end = (channel + 1) * src_channels / dst_channels;
                let group = &frame[start..end];
                samples.push(group.iter().copied().sum::<f32>() / group.len() as f32);
            }
        }
    }

    Ok(PcmBuffer::new(src.rate, target_channels, samples))
}

/// Apply the rate rule documented on [`convert`].
fn resample(src: &PcmBuffer, target_rate: u32) -> Result<PcmBuffer, ConvertError> {
    if src.rate == target_rate {
        return Ok(PcmBuffer::new(target_rate, src.channels, src.samples.clone()));
    }

    let src_frames = src.frames();
    if src_frames == 0 {
        return Ok(PcmBuffer::new(target_rate, src.channels, Vec::new()));
    }

    let channels = usize::from(src.channels);
    // Round-half-up in u128: no overflow and no floating point rounding error here.
    let numerator = u128::from(src_frames) * u128::from(target_rate) + u128::from(src.rate) / 2;
    let dst_frames =
        u64::try_from(numerator / u128::from(src.rate)).map_err(|_| ConvertError::TooManySamples)?;
    let dst_len = usize::try_from(dst_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(channels))
        .ok_or(ConvertError::TooManySamples)?;

    // Index of the last valid source frame; `src_frames >= 1` at this point.
    let last_frame = usize::try_from(src_frames - 1).map_err(|_| ConvertError::TooManySamples)?;
    let step = f64::from(src.rate) / f64::from(target_rate);

    let mut samples: Vec<f32> = Vec::new();
    samples.try_reserve_exact(dst_len).map_err(|_| ConvertError::TooManySamples)?;

    let output_frames = usize::try_from(dst_frames).map_err(|_| ConvertError::TooManySamples)?;
    for output in 0..output_frames {
        // Both rates are non-zero, so `position` is finite and never negative.
        let position = output as f64 * step;
        let left = position.floor();
        let left_frame = (left as usize).min(last_frame);
        let right_frame = (left_frame + 1).min(last_frame);
        let fraction = (position - left) as f32;

        // `get` keeps this panic-free even for buffers whose length is not a
        // multiple of the channel count; missing samples read as silence.
        let left_base = left_frame.saturating_mul(channels);
        let right_base = right_frame.saturating_mul(channels);
        let left_slice = src.samples.get(left_base..left_base.saturating_add(channels));
        let right_slice = src.samples.get(right_base..right_base.saturating_add(channels));

        for channel in 0..channels {
            let a = left_slice.and_then(|slice| slice.get(channel)).copied().unwrap_or(0.0);
            let b = right_slice.and_then(|slice| slice.get(channel)).copied().unwrap_or(0.0);
            samples.push(a + (b - a) * fraction);
        }
    }

    Ok(PcmBuffer::new(target_rate, src.channels, samples))
}

/// Convert normalized `f32` samples into the sample type `T` of the device.
///
/// `NaN` becomes silence and the value is clamped into `[-1.0, 1.0]` before
/// [`FromSample::from_sample`] runs, so integer targets saturate instead of
/// wrapping or panicking. For the `F32` target this is the identity on
/// normalized input.
pub fn convert_samples_to<T>(src: &[f32]) -> Vec<T>
where
    T: Sample + FromSample<f32>,
{
    let mut samples: Vec<T> = Vec::with_capacity(src.len());
    samples.extend(src.iter().map(|&sample| T::from_sample(clamp_unit(sample))));
    samples
}

/// Clamp to the normalized range, mapping `NaN` to silence.
fn clamp_unit(sample: f32) -> f32 {
    if sample.is_nan() { 0.0 } else { sample.clamp(-1.0, 1.0) }
}

/// Interleaved samples packed into a concrete device sample format.
#[derive(Clone, Debug)]
pub enum ConvertedSamples {
    /// 32-bit float samples.
    F32(Vec<f32>),
    /// Signed 16-bit integer samples.
    I16(Vec<i16>),
    /// Signed 32-bit integer samples.
    I32(Vec<i32>),
    /// Unsigned 8-bit integer samples.
    U8(Vec<u8>),
}

impl ConvertedSamples {
    /// Number of samples (not frames) in the buffer.
    pub fn len(&self) -> usize {
        match self {
            Self::F32(samples) => samples.len(),
            Self::I16(samples) => samples.len(),
            Self::I32(samples) => samples.len(),
            Self::U8(samples) => samples.len(),
        }
    }

    /// `true` if there is not a single sample.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Pack interleaved normalized `f32` samples into the device's sample format.
///
/// Only the formats the MVP devices use are packed; anything else returns
/// [`ConvertError::UnsupportedSampleFormat`].
pub fn convert_to_sample_format(
    src: &[f32],
    format: SampleFormat,
) -> Result<ConvertedSamples, ConvertError> {
    match format {
        SampleFormat::F32 => Ok(ConvertedSamples::F32(convert_samples_to(src))),
        SampleFormat::S16 => Ok(ConvertedSamples::I16(convert_samples_to(src))),
        SampleFormat::S32 => Ok(ConvertedSamples::I32(convert_samples_to(src))),
        SampleFormat::U8 => Ok(ConvertedSamples::U8(convert_samples_to(src))),
        other => Err(ConvertError::UnsupportedSampleFormat(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConvertError, ConvertedSamples, DeviceFormat, PcmBuffer, convert, convert_samples_to,
        convert_to_sample_format,
    };
    use std::f64::consts::TAU;
    use symphonia::core::audio::sample::SampleFormat;

    /// One second of a 440 Hz sine at `rate`, mono.
    fn sine(rate: u32) -> PcmBuffer {
        let frames = rate as usize;
        let samples = (0..frames)
            .map(|frame| (TAU * 440.0 * frame as f64 / f64::from(rate)).sin() as f32)
            .collect();
        PcmBuffer::new(rate, 1, samples)
    }

    /// Value of the reference 440 Hz sine at `frame` for a stream of `rate` Hz.
    fn sine_at(frame: usize, rate: u32) -> f32 {
        (TAU * 440.0 * frame as f64 / f64::from(rate)).sin() as f32
    }

    /// Check that every interior frame of `samples` (interleaved, `channels`
    /// channels) still is the 440 Hz sine at `rate`.
    ///
    /// The first and last frames are skipped: they sit on the buffer boundary
    /// where interpolation has no neighbour to read.
    fn assert_sine(samples: &[f32], channels: usize, rate: u32) {
        assert!(channels > 0, "zero channels");
        let frames = samples.len() / channels;
        assert!(frames > 16, "too few frames to check: {frames}");

        for frame in 8..frames - 8 {
            let expected = sine_at(frame, rate);
            for channel in 0..channels {
                let actual = samples[frame * channels + channel];
                assert!(
                    (actual - expected).abs() < 1e-2,
                    "frame {frame} channel {channel}: {actual} != {expected}"
                );
            }
        }
    }

    /// Check that the tail of the output holds the last source frame instead of
    /// extrapolating past the end of the buffer.
    ///
    /// Only meaningful when the target channels repeat the source channels (no
    /// averaging); that is the case for every caller here.
    fn assert_tail_is_clamped(converted: &PcmBuffer, source: &PcmBuffer) {
        let src_channels = usize::from(source.channels);
        let dst_channels = usize::from(converted.channels);
        let last_frame = source.samples.len() - src_channels;

        for channel in 0..dst_channels {
            let last_source = source.samples[last_frame + channel % src_channels];
            let last_output = converted.samples[converted.samples.len() - dst_channels + channel];
            assert!(
                (last_output - last_source).abs() <= f32::EPSILON,
                "tail channel {channel}: {last_output} != {last_source}"
            );
        }
    }

    #[test]
    fn resamples_44100_to_48000() {
        let source = sine(44_100);
        let target = DeviceFormat { rate: 48_000, channels: 1, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");

        assert_eq!(converted.rate, 48_000);
        assert_eq!(converted.channels, 1);
        assert_eq!(converted.frames(), 48_000);
        assert_eq!(converted.samples.len(), 48_000);

        let peak = converted.samples.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()));
        assert!(peak > 0.5, "signal degenerated: peak {peak}");
        assert!(converted.samples.iter().all(|s| s.is_finite()));
        // Linear interpolation keeps the signal inside the source range.
        assert!(peak <= 1.0);

        // The samples are still the 440 Hz sine, now on the 48000 Hz time base:
        // an inverted resampling ratio would keep the frame count but destroy the
        // waveform, so the content itself has to be checked.
        assert_sine(&converted.samples, 1, 48_000);
        assert_tail_is_clamped(&converted, &source);
    }

    #[test]
    fn resamples_48000_to_44100() {
        let source = sine(48_000);
        let target = DeviceFormat { rate: 44_100, channels: 1, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");

        assert_eq!(converted.rate, 44_100);
        assert_eq!(converted.channels, 1);
        assert_eq!(converted.frames(), 44_100);
        assert_eq!(converted.samples.len(), 44_100);
        assert!(converted.samples.iter().all(|s| s.is_finite()));

        assert_sine(&converted.samples, 1, 44_100);
    }

    #[test]
    fn resampling_never_reads_past_the_last_source_frame() {
        // 2 Hz source, 8 Hz target: the last four output positions fall on the
        // last source frame, where there is no right neighbour to interpolate with.
        // The last source frame is 0.5 rather than 0.0 so that reading past the
        // end (silence) is distinguishable from clamping to the last frame.
        let source = PcmBuffer::new(2, 1, vec![0.0, 1.0, 0.5]);
        let target = DeviceFormat { rate: 8, channels: 1, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");

        assert_eq!(converted.frames(), 12);
        let expected: [f32; 12] =
            [0.0, 0.25, 0.5, 0.75, 1.0, 0.875, 0.75, 0.625, 0.5, 0.5, 0.5, 0.5];
        for (index, (actual, expected)) in converted.samples.iter().zip(expected).enumerate() {
            assert!((actual - expected).abs() <= f32::EPSILON, "frame {index}: {actual} != {expected}");
        }
    }

    #[test]
    fn keeps_same_rate_and_channels_unchanged() {
        let source = PcmBuffer::new(44_100, 2, vec![0.25, -0.25, 0.5, -0.5]);
        let target = DeviceFormat { rate: 44_100, channels: 2, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");
        assert_eq!(converted.samples, source.samples);
    }

    #[test]
    fn mono_becomes_stereo_by_duplication() {
        let source = PcmBuffer::new(44_100, 1, vec![0.25, -0.5, 1.0]);
        let target = DeviceFormat { rate: 44_100, channels: 2, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");

        assert_eq!(converted.channels, 2);
        assert_eq!(converted.frames(), 3);
        let (frames, remainder) = converted.samples.as_chunks::<2>();
        assert!(remainder.is_empty(), "odd sample count: {}", converted.samples.len());
        for frame in frames {
            assert_eq!(frame[0], frame[1], "mono is not duplicated: {frame:?}");
        }
        assert_eq!(converted.samples, vec![0.25, 0.25, -0.5, -0.5, 1.0, 1.0]);
    }

    #[test]
    fn stereo_becomes_mono_by_averaging() {
        let source = PcmBuffer::new(44_100, 2, vec![1.0, 0.0, -0.5, 0.5]);
        let target = DeviceFormat { rate: 44_100, channels: 1, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");
        assert_eq!(converted.channels, 1);
        assert_eq!(converted.samples, vec![0.5, 0.0]);
    }

    #[test]
    fn quad_becomes_stereo_by_group_averaging() {
        // 4 channels -> 2 groups: {0,1} and {2,3}.
        let source = PcmBuffer::new(44_100, 4, vec![1.0, 0.0, 0.0, 1.0]);
        let target = DeviceFormat { rate: 44_100, channels: 2, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");
        assert_eq!(converted.samples, vec![0.5, 0.5]);
    }

    #[test]
    fn mono_to_quad_repeats_cyclically() {
        let source = PcmBuffer::new(44_100, 1, vec![0.1, 0.2]);
        let target = DeviceFormat { rate: 44_100, channels: 4, sample_format: SampleFormat::F32 };
        let converted = convert(&source, &target).expect("conversion succeeds");
        assert_eq!(converted.samples, vec![0.1, 0.1, 0.1, 0.1, 0.2, 0.2, 0.2, 0.2]);
    }

    #[test]
    fn full_mvp_conversion_of_a_sine() {
        let source = sine(44_100);
        let converted = convert(&source, &super::MVP_DEVICE).expect("conversion succeeds");
        assert_eq!(converted.rate, 96_000);
        assert_eq!(converted.channels, 2);
        assert_eq!(converted.frames(), 96_000);
        assert_eq!(converted.samples.len(), 192_000);

        // Both device channels must carry the 440 Hz sine on the 96000 Hz time base.
        assert_sine(&converted.samples, 2, 96_000);
        assert_tail_is_clamped(&converted, &source);
    }

    #[test]
    fn f32_to_i16_is_clamped_and_panic_free() {
        let source = [0.0_f32, 0.5, -0.5, 1.0, -1.0, 2.0, -2.0, f32::NAN, f32::INFINITY, -0.25];
        let packed = convert_samples_to::<i16>(&source);

        assert_eq!(packed[0], 0);
        assert_eq!(packed[1], 16_384);
        assert_eq!(packed[2], -16_384);
        assert_eq!(packed[3], i16::MAX);
        assert_eq!(packed[4], i16::MIN);
        assert_eq!(packed[5], i16::MAX);
        assert_eq!(packed[6], i16::MIN);
        assert_eq!(packed[7], 0);
        assert_eq!(packed[8], i16::MAX);
        assert_eq!(packed[9], -8_192);
        assert_eq!(packed.len(), source.len());
    }

    #[test]
    fn f32_to_i32_and_u8_stay_in_range() {
        let source = [1.0_f32, -1.0, 0.0, f32::NAN, 3.0, -3.0];
        let packed = convert_samples_to::<i32>(&source);
        assert_eq!(packed[0], i32::MAX);
        assert_eq!(packed[1], i32::MIN);
        assert_eq!(packed[2], 0);
        assert_eq!(packed[3], 0);
        assert_eq!(packed[4], i32::MAX);
        assert_eq!(packed[5], i32::MIN);

        let packed = convert_samples_to::<u8>(&source);
        assert_eq!(packed[0], u8::MAX);
        assert_eq!(packed[1], 0);
        assert_eq!(packed[2], 128);
    }

    #[test]
    fn dispatches_on_device_sample_format() {
        let source = [1.0_f32, -1.0];
        let packed = convert_to_sample_format(&source, SampleFormat::F32).expect("f32 packs");
        match packed {
            ConvertedSamples::F32(samples) => assert_eq!(samples, vec![1.0, -1.0]),
            other => panic!("unexpected variant {other:?}"),
        }

        let packed = convert_to_sample_format(&source, SampleFormat::S16).expect("i16 packs");
        assert_eq!(packed.len(), 2);
        assert!(!packed.is_empty());
        match packed {
            ConvertedSamples::I16(samples) => assert_eq!(samples, vec![i16::MAX, i16::MIN]),
            other => panic!("unexpected variant {other:?}"),
        }
    }

    #[test]
    fn unsupported_sample_format_is_an_error() {
        let packed = convert_to_sample_format(&[0.5], SampleFormat::F64);
        assert!(matches!(packed, Err(ConvertError::UnsupportedSampleFormat(_))));
    }

    #[test]
    fn degenerate_buffers_do_not_panic() {
        let empty = PcmBuffer::new(44_100, 1, Vec::new());
        let target = DeviceFormat { rate: 96_000, channels: 2, sample_format: SampleFormat::F32 };
        let converted = convert(&empty, &target).expect("empty buffer converts");
        assert_eq!(converted.frames(), 0);
        assert!(converted.samples.is_empty());

        let zero_rate = PcmBuffer::new(0, 1, vec![0.0]);
        assert!(matches!(convert(&zero_rate, &target), Err(ConvertError::ZeroRate)));

        let zero_channels = PcmBuffer::new(44_100, 0, vec![0.0]);
        assert!(matches!(convert(&zero_channels, &target), Err(ConvertError::ZeroChannels)));

        let valid = PcmBuffer::new(44_100, 1, vec![0.0]);
        let zero_device_rate =
            DeviceFormat { rate: 0, channels: 2, sample_format: SampleFormat::F32 };
        assert!(matches!(convert(&valid, &zero_device_rate), Err(ConvertError::ZeroRate)));

        let zero_device_channels =
            DeviceFormat { rate: 96_000, channels: 0, sample_format: SampleFormat::F32 };
        assert!(matches!(
            convert(&valid, &zero_device_channels),
            Err(ConvertError::ZeroChannels)
        ));

        // Samples that do not fill a whole frame must not panic either.
        let ragged = PcmBuffer::new(44_100, 2, vec![0.5, 0.5, 0.5]);
        let converted = convert(&ragged, &target).expect("ragged buffer converts");
        assert_eq!(converted.frames(), 2);
    }
}
