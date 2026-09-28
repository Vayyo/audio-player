//! Fixed-buffer PCM spectrum and projected ASCII torus for the UI thread.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use crate::core::player::PlaybackState;

const WIDTH: usize = 60;
const HEIGHT: usize = 22;
const CELLS: usize = WIDTH * HEIGHT;
const MAJOR: usize = 80;
const MINOR: usize = 28;
const BANDS: usize = 12;
const WINDOW: usize = 2048;
const SHADES: &[u8] = b" .:-=+*#%@";
const CONTOURS: usize = 3;
const BANDS_PER_CONTOUR: usize = BANDS / CONTOURS;
const COLOR_COLUMNS: usize = 6;


#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ColorMode {
    BlackWhite,
    Solid,
    Rgb,
}

/// An owned, reusable frame of monospaced ASCII; update it on the UI thread.
pub(super) struct AsciiDonut {
    text: String,
    phase: f32,
    ring_phase: f32,
    ring_speed: f32,
    last_state: PlaybackState,
    stop_bands: [f32; BANDS],
    stop_elapsed: f32,
    stop_gain: f32,
    bands: [f32; BANDS],
    major: [(f32, f32); MAJOR],
    minor: [(f32, f32); MINOR],
    hann: [f32; WINDOW],
}

impl Default for AsciiDonut {
    fn default() -> Self {
        let mut donut = Self {
            text: String::with_capacity(HEIGHT * (WIDTH + 1)),
            phase: 0.0,
            ring_phase: 0.0,
            ring_speed: 0.0,
            last_state: PlaybackState::Stopped,
            stop_bands: [0.0; BANDS],
            stop_elapsed: 0.0,
            stop_gain: 1.0,
            bands: [0.0; BANDS],
            major: std::array::from_fn(|i| (TAU * i as f32 / MAJOR as f32).sin_cos()),
            minor: std::array::from_fn(|i| (TAU * i as f32 / MINOR as f32).sin_cos()),
            hann: std::array::from_fn(|i| {
                0.5 - 0.5 * (TAU * i as f32 / (WINDOW - 1) as f32).cos()
            }),
        };
        donut.project();
        donut
    }
}

impl AsciiDonut {
    /// `samples` are interleaved decoded PCM frames in output-device format.
    /// `position_frames` is the transport position in that same PCM buffer.
    pub(super) fn update(
        &mut self,
        samples: &[f32],
        channels: usize,
        sample_rate: u32,
        position_frames: u64,
        state: PlaybackState,
        dt: Duration,
    ) {
        // A suspended window or a debugger must not cause a large animation jump.
        const SPEED: f32 = 1.7;
        const EASE_SECONDS: f32 = 0.4;
        let elapsed = dt.as_secs_f32().min(0.1);
        self.phase = (self.phase + elapsed * SPEED).rem_euclid(TAU);
        let previous_speed = self.ring_speed;
        let speed_change = SPEED * elapsed / EASE_SECONDS;
        self.ring_speed = if state == PlaybackState::Playing {
            (previous_speed + speed_change).min(SPEED)
        } else {
            (previous_speed - speed_change).max(0.0)
        };
        let entering_stop = state == PlaybackState::Stopped && self.last_state != state;
        if !entering_stop {
            self.ring_phase = (self.ring_phase + (previous_speed + self.ring_speed) * 0.5 * elapsed)
                .rem_euclid(TAU);
        }
        match state {
            PlaybackState::Playing => {
                self.stop_gain = 1.0;
                self.analyze(samples, channels, sample_rate, position_frames);
            }
            PlaybackState::Paused => {} // Retain measured energy while rotation eases to rest.
            PlaybackState::Stopped => {
                if entering_stop {
                    self.stop_bands = self.bands;
                    self.stop_elapsed = 0.0;
                    self.stop_gain = 1.0;
                } else {
                    self.stop_elapsed = (self.stop_elapsed + elapsed).min(EASE_SECONDS);
                    let progress = self.stop_elapsed / EASE_SECONDS;
                    self.stop_gain = 1.0 - progress * progress * (3.0 - 2.0 * progress);
                    self.bands = self.stop_bands.map(|energy| energy * self.stop_gain);
                }
            }
        }
        self.last_state = state;
        self.project();
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// The text stays one 60×22 projection; only RGB varies across it and with phase.
    pub(super) fn colored_layout(
        &self,
        font: eframe::egui::FontId,
        mode: ColorMode,
        hue: f32,
    ) -> eframe::egui::text::LayoutJob {
        use eframe::egui::{ecolor::Hsva, text::{LayoutJob, TextFormat}};

        let mut job = LayoutJob::default();
        if mode != ColorMode::Rgb {
            let color = match mode {
                ColorMode::BlackWhite => eframe::egui::Color32::WHITE,
                ColorMode::Solid => Hsva::new(hue, 0.78, 1.0, 1.0).into(),
                ColorMode::Rgb => unreachable!(),
            };
            job.append(self.text(), 0.0, TextFormat {
                font_id: font,
                color,
                ..Default::default()
            });
            return job;
        }
        for row in 0..HEIGHT {
            let line = row * (WIDTH + 1);
            for column in (0..WIDTH).step_by(WIDTH / COLOR_COLUMNS) {
                let end = column + WIDTH / COLOR_COLUMNS;
                let spatial_hue = (column as f32 / WIDTH as f32 * 0.82
                    + row as f32 / HEIGHT as f32 * 0.23
                    + self.phase / TAU + hue).fract();
                let color = Hsva::new(spatial_hue, 0.78, 1.0, 1.0).into();
                job.append(
                    &self.text()[line + column..line + end + usize::from(end == WIDTH)],
                    0.0,
                    TextFormat { font_id: font.clone(), color, ..Default::default() },
                );
            }
        }
        job
    }

    /// Three closed contours; each reads only its own four measured frequency bands.
    pub(super) fn draw_equalizer(
        &self,
        painter: &eframe::egui::Painter,
        center: eframe::egui::Pos2,
        radius: f32,
        mode: ColorMode,
        hue: f32,
    ) {
        const SEGMENTS: usize = 120;
        let colors = Self::contour_colors(mode, hue);
        for (contour, color) in colors.into_iter().enumerate() {
            let stroke = eframe::egui::Stroke::new(1.35, color);
            let mut previous = self.equalizer_point(center, radius, contour, 0.0);
            for segment in 1..=SEGMENTS {
                let angle = TAU * segment as f32 / SEGMENTS as f32;
                let next = self.equalizer_point(center, radius, contour, angle);
                painter.line_segment([previous, next], stroke);
                previous = next;
            }
        }
    }

    fn contour_colors(mode: ColorMode, hue: f32) -> [eframe::egui::Color32; CONTOURS] {
        use eframe::egui::ecolor::Hsva;

        // All three strokes share one hue; fixed descending value leaves the
        // bright ASCII surface in front, regardless of selected color mode.
        let values = [0.68, 0.53, 0.40];
        match mode {
            ColorMode::BlackWhite => values.map(|value| eframe::egui::Color32::from_gray((value * 255.0) as u8)),
            ColorMode::Solid | ColorMode::Rgb =>
                values.map(|value| Hsva::new(hue, 0.78, value, 1.0).into()),
        }
    }

    fn equalizer_point(
        &self,
        center: eframe::egui::Pos2,
        radius: f32,
        contour: usize,
        angle: f32,
    ) -> eframe::egui::Pos2 {
        let band = angle.rem_euclid(TAU) * (BANDS_PER_CONTOUR as f32 / TAU);
        let left = band as usize % BANDS_PER_CONTOUR;
        let fraction = band.fract();
        let blend = fraction * fraction * (3.0 - 2.0 * fraction);
        let start = contour * BANDS_PER_CONTOUR;
        let measured = if self.last_state == PlaybackState::Stopped || self.stop_gain < 1.0 {
            &self.stop_bands
        } else {
            &self.bands
        };
        let energy = measured[start + left] * (1.0 - blend)
            + measured[start + (left + 1) % BANDS_PER_CONTOUR] * blend;
        // Modulate *measured* energy, not the neutral radius: a sustained tone
        // remains visibly wavy, the crest travels with phase, silence stays round.
        // The full excursion remains inside the display even at 800×550.
        let displacement = if self.stop_gain == 0.0 {
            0.0
        } else {
            let crest = 0.56 + 0.44 * (angle * 4.0 - self.ring_phase * 3.0).cos();
            (1.0 - (-130.0 * energy).exp()) * (radius * 0.075).min(24.0) * crest * self.stop_gain
        };
        let distance = radius * (1.045 + contour as f32 * 0.085) + displacement;
        let (sin, cos) = angle.sin_cos();
        eframe::egui::pos2(center.x + distance * sin, center.y - distance * cos)
    }


    fn analyze(&mut self, samples: &[f32], channels: usize, rate: u32, position: u64) {
        self.bands.fill(0.0);
        if channels == 0 || rate == 0 {
            return;
        }
        let frames = samples.len() / channels;
        let end = position.try_into().unwrap_or(usize::MAX).min(frames);
        let count = end.min(WINDOW);
        if count < 16 {
            return;
        }

        let low = 55.0_f32;
        let high = (rate as f32 * 0.45).min(9000.0);
        if high <= low {
            return;
        }
        let ratio = (high / low).powf(1.0 / BANDS as f32);
        for (band, level) in self.bands.iter_mut().enumerate() {
            let edge = low * ratio.powi(band as i32);
            let next = edge * ratio;
            // A longer Hann window rejects energy spilling into adjacent bands.
            // Three probes retain tones between centers and near both band edges.
            let width = next - edge;
            let n = ((6.0 * rate as f32 / width) as usize).clamp(64, WINDOW).min(count);
            if n < 16 {
                continue;
            }
            let coefficients = [0.15, 0.5, 0.85].map(|fraction| {
                2.0 * (TAU * (edge + width * fraction) / rate as f32).cos()
            });
            let mut power_sum = [0.0_f32; 3];
            let mut weight = 0.0_f32;
            // Average channel powers, not samples: opposite-phase speakers still
            // carry the same tone even though their arithmetic mean is zero.
            for channel in 0..channels {
                let mut prev = [0.0_f32; 3];
                let mut prev2 = [0.0_f32; 3];
                for i in 0..n {
                    let window = self.hann[i * (WINDOW - 1) / (n - 1)];
                    if channel == 0 {
                        weight += window;
                    }
                    let sample = samples[(end - n + i) * channels + channel];
                    let sample = if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
                    for probe in 0..3 {
                        let current = sample * window + coefficients[probe] * prev[probe] - prev2[probe];
                        prev2[probe] = prev[probe];
                        prev[probe] = current;
                    }
                }
                for probe in 0..3 {
                    let coefficient = coefficients[probe];
                    power_sum[probe] += (prev[probe] * prev[probe] + prev2[probe] * prev2[probe]
                        - coefficient * prev[probe] * prev2[probe]).max(0.0);
                }
            }
            *level = (2.0 * (power_sum[0].max(power_sum[1]).max(power_sum[2]) / channels as f32).sqrt() / weight).clamp(0.0, 1.0);
        }
    }

    fn project(&mut self) {
        let mut pixels = [b' '; CELLS];
        let mut depth = [f32::NEG_INFINITY; CELLS];
        let (spin_sin, spin_cos) = self.phase.sin_cos();
        let (tilt_sin, tilt_cos) = (PI * 0.21).sin_cos();
        for (u, &(sin_u, cos_u)) in self.major.iter().enumerate() {
            let energy = self.bands[u * BANDS / MAJOR];
            let radius = 0.62 + 0.24 * energy;
            for &(sin_v, cos_v) in &self.minor {
                let ring = 1.48 + radius * cos_v;
                let x0 = ring * cos_u;
                let y0 = ring * sin_u;
                let z0 = radius * sin_v;
                let x = x0 * spin_cos + z0 * spin_sin;
                let z = z0 * spin_cos - x0 * spin_sin;
                let y = y0 * tilt_cos - z * tilt_sin;
                let z = y0 * tilt_sin + z * tilt_cos;
                // At 11px monospace, one glyph is roughly half as wide as a row
                // is tall; compensate in projection rather than stretching text.
                let col = (WIDTH as f32 * 0.5 + x * 7.6).round() as isize;
                let row = (HEIGHT as f32 * 0.5 - y * 4.0).round() as isize;
                if !(0..WIDTH as isize).contains(&col) || !(0..HEIGHT as isize).contains(&row) {
                    continue;
                }
                let at = row as usize * WIDTH + col as usize;
                if z <= depth[at] {
                    continue;
                }
                depth[at] = z;
                let nx0 = cos_v * cos_u;
                let ny0 = cos_v * sin_u;
                let nz0 = sin_v;
                let nx = nx0 * spin_cos + nz0 * spin_sin;
                let nz = nz0 * spin_cos - nx0 * spin_sin;
                let ny = ny0 * tilt_cos - nz * tilt_sin;
                let nz = ny0 * tilt_sin + nz * tilt_cos;
                let light = (nx * -0.38 + ny * 0.48 + nz * 0.8).max(0.0);
                let brightness = (0.22 + light * 0.57 + energy * 0.36).min(1.0);
                pixels[at] = SHADES[(brightness * (SHADES.len() - 1) as f32) as usize];
            }
        }
        self.text.clear();
        for line in pixels.as_chunks::<WIDTH>().0 {
            for &glyph in line {
                self.text.push(char::from(glyph));
            }
            self.text.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32) -> Vec<f32> {
        (0..4096)
            .map(|i| 0.85 * (TAU * hz * i as f32 / 48_000.0).sin())
            .collect()
    }

    fn dominant(bands: &[f32; BANDS]) -> usize {
        bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0
    }

    #[test]
    fn real_frequencies_select_different_sectors_and_silence_stays_neutral() {
        let mut donut = AsciiDonut::default();
        let silence = [0.0; 2048];
        donut.update(&silence, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        assert!(donut.bands.iter().all(|&v| v == 0.0));
        let neutral = donut.text().to_owned();
        donut.update(&tone(220.0), 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let low = dominant(&donut.bands);
        assert!(donut.bands[low] > 0.3, "low tone bands: {:?}", donut.bands);
        let low_text = donut.text().to_owned();
        donut.update(&tone(4000.0), 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let high = dominant(&donut.bands);
        assert!(high > low + 3, "low {low}, high {high}: {:?}", donut.bands);
        assert!(donut.bands[high] > 0.15, "high tone bands: {:?}", donut.bands);
        assert_ne!(neutral, low_text);
        assert_ne!(low_text, donut.text());
    }

    #[test]
    fn spectrum_uses_only_reached_frames_including_short_history() {
        let mut donut = AsciiDonut::default();
        let mut samples = vec![0.0; 128];
        samples.extend(tone(4000.0));
        for position in [0, 15, 16, 64, 128] {
            donut.update(&samples, 1, 48_000, position, PlaybackState::Playing, Duration::ZERO);
            assert!(donut.bands.iter().all(|&v| v == 0.0), "position {position}: {:?}", donut.bands);
        }
        donut.update(&samples, 1, 48_000, 192, PlaybackState::Playing, Duration::ZERO);
        let high = dominant(&donut.bands);
        assert!(high >= 7 && donut.bands[high] > 0.15, "short history: {:?}", donut.bands);
        donut.update(&samples, 1, 48_000, 64, PlaybackState::Playing, Duration::ZERO);
        assert!(donut.bands.iter().all(|&v| v == 0.0), "seeking back previews future PCM");

        let short = tone(4000.0);
        donut.update(&short[..96], 1, 48_000, 96, PlaybackState::Playing, Duration::ZERO);
        assert!(donut.bands[dominant(&donut.bands)] > 0.15, "short clip: {:?}", donut.bands);
    }

    #[test]
    fn anti_phase_stereo_retains_low_frequency_energy() {
        let mono = tone(220.0);
        let stereo: Vec<f32> = mono.iter().flat_map(|&sample| [sample, -sample]).collect();
        let mut donut = AsciiDonut::default();
        donut.update(&mono, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let low = dominant(&donut.bands);
        let mono_level = donut.bands[low];
        assert!(mono_level > 0.3, "mono bands: {:?}", donut.bands);
        donut.update(&stereo, 2, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        assert_eq!(dominant(&donut.bands), low, "stereo bands: {:?}", donut.bands);
        assert!((donut.bands[low] - mono_level).abs() < 1e-4, "stereo bands: {:?}", donut.bands);
    }

    #[test]
    fn pause_retains_spectrum_and_stop_eases_to_neutral_while_ascii_rotates() {
        let mut donut = AsciiDonut::default();
        let samples = tone(220.0);
        donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, Duration::from_millis(90));
        let playing_text = donut.text().to_owned();
        let measured = donut.bands;
        assert!(measured.iter().any(|&v| v > 0.3));

        donut.update(&[], 0, 0, u64::MAX, PlaybackState::Paused, Duration::from_millis(90));
        assert_eq!(donut.bands, measured);
        assert_ne!(donut.text(), playing_text);
        let paused_text = donut.text().to_owned();
        let phase = donut.phase;
        donut.update(&[], 0, 0, 0, PlaybackState::Stopped, Duration::from_millis(90));
        assert_eq!(donut.bands, measured);
        assert!(donut.phase > phase);
        assert_ne!(donut.text(), paused_text);
        for _ in 0..5 {
            donut.update(&[], 0, 0, 0, PlaybackState::Stopped, Duration::from_millis(90));
        }
        assert!(donut.bands.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn contours_ease_to_freeze_and_flatten_then_resume_with_pcm() {
        let mut donut = AsciiDonut::default();
        let samples: Vec<f32> = tone(220.0).into_iter().map(|sample| sample * (0.02 / 0.85)).collect();
        let center = eframe::egui::pos2(146.0, 146.0);
        let radius = 111.0;
        let vertices = |donut: &AsciiDonut| -> [[eframe::egui::Pos2; 120]; CONTOURS] {
            std::array::from_fn(|contour| {
                std::array::from_fn(|segment| {
                    donut.equalizer_point(center, radius, contour, TAU * segment as f32 / 120.0)
                })
            })
        };
        let deformation = |donut: &AsciiDonut, contour: usize| -> f32 {
            (0..120).map(|segment| {
                let point = donut.equalizer_point(center, radius, contour, TAU * segment as f32 / 120.0);
                point.distance(center) - radius * (1.045 + contour as f32 * 0.085)
            }).fold(0.0_f32, f32::max)
        };
        let tick = Duration::from_millis(90);

        for _ in 0..6 {
            donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, tick);
        }
        let active = dominant(&donut.bands) / BANDS_PER_CONTOUR;
        assert!(deformation(&donut, active) > 1.0);
        let measured = donut.bands;
        let playing_vertices = vertices(&donut);
        let playing_text = donut.text().to_owned();
        let playing_speed = donut.ring_speed;
        donut.update(&[], 0, 0, u64::MAX, PlaybackState::Paused, tick);
        assert_ne!(vertices(&donut), playing_vertices);
        assert_eq!(donut.bands, measured);
        assert!(donut.ring_speed < playing_speed);
        assert_ne!(donut.text(), playing_text);

        let mut prior_speed = donut.ring_speed;
        let mut prior_phase = donut.ring_phase;
        for _ in 0..4 {
            donut.update(&[], 0, 0, 0, PlaybackState::Paused, tick);
            assert!(donut.ring_speed < prior_speed);
            assert!(donut.ring_phase > prior_phase);
            assert_eq!(donut.bands, measured);
            prior_speed = donut.ring_speed;
            prior_phase = donut.ring_phase;
        }
        assert_eq!(donut.ring_speed, 0.0);
        let frozen = vertices(&donut);
        assert_ne!(frozen, playing_vertices);
        for _ in 0..3 {
            donut.update(&[], 0, 0, 0, PlaybackState::Paused, tick);
            assert_eq!(vertices(&donut), frozen);
        }

        donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, tick);
        assert!(donut.ring_speed > 0.0 && donut.ring_speed < playing_speed);
        assert!(vertices(&donut)[active].iter().zip(&frozen[active])
            .any(|(next, before)| next.distance(*before) > 0.01));
        for _ in 0..5 {
            donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, tick);
        }

        let before_stop = vertices(&donut);
        let initial_deformation = deformation(&donut, active);
        let speed_before_stop = donut.ring_speed;
        donut.update(&[], 0, 0, 0, PlaybackState::Stopped, tick);
        assert_eq!(vertices(&donut), before_stop);
        assert_eq!(donut.bands, measured);
        assert!(donut.ring_speed < speed_before_stop);
        let mut prior_deformation = initial_deformation;
        let mut prior_speed = donut.ring_speed;
        for frame in 0..5 {
            donut.update(&[], 0, 0, 0, PlaybackState::Stopped, tick);
            let next_deformation = deformation(&donut, active);
            assert!(next_deformation < prior_deformation);
            if frame == 0 {
                assert!(next_deformation > initial_deformation * 0.7);
            }
            assert!(donut.ring_speed < prior_speed || (prior_speed == 0.0 && donut.ring_speed == 0.0));
            prior_deformation = next_deformation;
            prior_speed = donut.ring_speed;
        }
        assert_eq!(donut.ring_speed, 0.0);
        assert!(donut.bands.iter().all(|&energy| energy == 0.0));
        let circles = vertices(&donut);
        for (contour, points) in circles.iter().enumerate() {
            let neutral_radius = radius * (1.045 + contour as f32 * 0.085);
            assert!(points.iter().all(|point| (point.distance(center) - neutral_radius).abs() < 0.001));
        }
        donut.update(&[], 0, 0, 0, PlaybackState::Stopped, tick);
        assert_eq!(vertices(&donut), circles);
    }

    #[test]
    fn disjoint_frequency_contours_follow_only_their_measured_pcm_bands() {
        let mut donut = AsciiDonut::default();
        let center = eframe::egui::pos2(146.0, 146.0);
        let radius = 111.0; // 800×550 window after frame, sidebar, and title.
        let base = |contour| radius * (1.045 + contour as f32 * 0.085);

        for contour in 0..CONTOURS {
            for segment in 0..=120 {
                let angle = TAU * segment as f32 / 120.0;
                let point = donut.equalizer_point(center, radius, contour, angle);
                assert!((center.distance(point) - base(contour)).abs() < 0.001);
                assert!(point.x >= 0.0 && point.x <= 292.0);
                assert!(point.y >= 0.0 && point.y <= 292.0);
            }
            assert!(donut.equalizer_point(center, radius, contour, 0.0)
                .distance(donut.equalizer_point(center, radius, contour, TAU)) < 0.001);
        }

        for (hz, contour) in [(220.0, 0), (800.0, 1), (4000.0, 2)] {
            donut.update(&tone(hz), 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
            let band = dominant(&donut.bands);
            assert_eq!(band / BANDS_PER_CONTOUR, contour, "{hz} Hz: {:?}", donut.bands);
            let angle = TAU * (band % BANDS_PER_CONTOUR) as f32 / BANDS_PER_CONTOUR as f32;
            let peak = center.distance(donut.equalizer_point(center, radius, contour, angle));
            assert!(peak > base(contour) + 5.0, "{hz} Hz: {peak}");
            for other in 0..CONTOURS {
                if other != contour {
                    let other_peak = (0..120).map(|segment| {
                        let angle = TAU * segment as f32 / 120.0;
                        center.distance(donut.equalizer_point(center, radius, other, angle))
                    }).fold(0.0_f32, f32::max);
                    assert!(other_peak < base(other) + 2.0, "{hz} Hz bleeds into contour {other}: peak {}, bands {:?}", other_peak - base(other), donut.bands);
                }
            }
            for other in 0..CONTOURS {
                for segment in 0..120 {
                    let point = donut.equalizer_point(center, radius, other, TAU * segment as f32 / 120.0);
                    assert!(point.x >= 0.0 && point.x <= 292.0);
                    assert!(point.y >= 0.0 && point.y <= 292.0);
                }
            }
            donut.update(&[], 0, 0, 0, PlaybackState::Paused, Duration::ZERO);
            assert!((center.distance(donut.equalizer_point(center, radius, contour, angle)) - peak).abs() < 0.001);
            donut.update(&[], 0, 0, 0, PlaybackState::Stopped, Duration::ZERO);
            for _ in 0..4 {
                donut.update(&[], 0, 0, 0, PlaybackState::Stopped, Duration::from_millis(100));
            }
            assert!((center.distance(donut.equalizer_point(center, radius, contour, angle)) - base(contour)).abs() < 0.001);
        }
    }

    #[test]
    fn sustained_pcm_has_moving_five_pixel_contour_crests() {
        let center = eframe::egui::pos2(146.0, 146.0);
        let radius = 111.0;
        for (hz, contour) in [(220.0, 0), (4000.0, 2)] {
            let mut donut = AsciiDonut::default();
            let samples: Vec<f32> = tone(hz).into_iter().map(|sample| sample * (0.125 / 0.85)).collect();
            donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
            let band = dominant(&donut.bands);
            assert_eq!(band / BANDS_PER_CONTOUR, contour, "{hz} Hz");
            let angle = TAU * (band % BANDS_PER_CONTOUR) as f32 / BANDS_PER_CONTOUR as f32;
            let initial_crest = center.distance(donut.equalizer_point(center, radius, contour, angle));
            let (mut nearest, mut farthest) = (f32::MAX, 0.0_f32);
            for segment in 0..120 {
                let distance = center.distance(donut.equalizer_point(
                    center, radius, contour, TAU * segment as f32 / 120.0,
                ));
                nearest = nearest.min(distance);
                farthest = farthest.max(distance);
            }
            assert!(farthest - nearest >= 5.0, "{hz} Hz crest-to-trough: {}", farthest - nearest);
            for _ in 0..8 {
                donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, Duration::from_millis(90));
            }
            let moved = center.distance(donut.equalizer_point(center, radius, contour, angle));
            assert!(initial_crest - moved >= 3.0, "{hz} Hz crest did not travel: {initial_crest} -> {moved}");
        }
    }

    #[test]
    fn sustained_bass_remains_wavy_in_large_windows() {
        let mut donut = AsciiDonut::default();
        let samples: Vec<f32> = tone(220.0).into_iter().map(|sample| sample * (0.125 / 0.85)).collect();
        donut.update(&samples, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let center = eframe::egui::pos2(380.0, 380.0);
        let (mut nearest, mut farthest) = (f32::MAX, 0.0_f32);
        for segment in 0..120 {
            let distance = center.distance(donut.equalizer_point(center, 280.0, 0, TAU * segment as f32 / 120.0));
            nearest = nearest.min(distance);
            farthest = farthest.max(distance);
        }
        assert!(farthest - nearest >= 14.0, "large-window crest-to-trough: {}", farthest - nearest);
    }

    #[test]
    fn quiet_mid_tone_survives_loud_bass_without_driving_other_contours() {
        let bass = tone(220.0);
        let mid = tone(800.0);
        let mixed: Vec<_> = bass.iter().zip(&mid).map(|(low, middle)| low + middle * 0.025).collect();
        let mut donut = AsciiDonut::default();
        let center = eframe::egui::pos2(146.0, 146.0);
        let radius = 111.0;
        let mid_peak = |donut: &AsciiDonut| (0..120).map(|segment| {
            center.distance(donut.equalizer_point(center, radius, 1, TAU * segment as f32 / 120.0))
        }).fold(0.0_f32, f32::max);

        donut.update(&bass, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let bass_only = mid_peak(&donut);
        donut.update(&mixed, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
        let mixed_mid = mid_peak(&donut);
        assert!(mixed_mid >= radius * 1.13 + 5.0, "quiet mid vanished: {mixed_mid}, bands {:?}", donut.bands);
        assert!(mixed_mid - bass_only >= 4.0, "bass leaked into mid contour: {bass_only} -> {mixed_mid}");
        let bass_angle = TAU * 3.0 / BANDS_PER_CONTOUR as f32;
        assert!(center.distance(donut.equalizer_point(center, radius, 0, bass_angle)) >= radius * 1.045 + 5.0);
    }

    #[test]
    fn quiet_pcm_moves_the_matching_contour_by_five_pixels() {
        let mut donut = AsciiDonut::default();
        let center = eframe::egui::pos2(150.0, 150.0);
        for (hz, contour) in [(220.0, 0), (4000.0, 2)] {
            let quiet: Vec<f32> = tone(hz).into_iter().map(|sample| sample * 0.025).collect();
            donut.update(&quiet, 1, 48_000, 2048, PlaybackState::Playing, Duration::ZERO);
            let band = dominant(&donut.bands);
            assert_eq!(band / BANDS_PER_CONTOUR, contour);
            let angle = TAU * (band % BANDS_PER_CONTOUR) as f32 / BANDS_PER_CONTOUR as f32;
            let displacement = center.distance(donut.equalizer_point(center, 120.0, contour, angle))
                - 120.0 * (1.045 + contour as f32 * 0.085);
            assert!(displacement >= 5.0, "{hz} Hz: {displacement}");
        }
    }

    #[test]
    fn black_white_ascii_is_white_with_strictly_darker_grayscale_rings() {
        let donut = AsciiDonut::default();
        let font = eframe::egui::FontId::monospace(11.0);
        let layout = donut.colored_layout(font.clone(), ColorMode::BlackWhite, 0.0);
        assert_eq!(layout.text, donut.text());
        assert_eq!(layout.sections.len(), 1);
        assert_eq!(layout.sections[0].format.color, eframe::egui::Color32::WHITE);
        let rings = AsciiDonut::contour_colors(ColorMode::BlackWhite, 0.3);
        assert!(rings.iter().all(|ring| ring.r() == ring.g() && ring.g() == ring.b()));
        assert!(255 > rings[0].r() && rings[0].r() > rings[1].r() && rings[1].r() > rings[2].r());
        assert_eq!(rings, AsciiDonut::contour_colors(ColorMode::BlackWhite, 0.8));
        assert_eq!(layout.sections[0].format.color,
            donut.colored_layout(font, ColorMode::BlackWhite, 0.8).sections[0].format.color);
    }

    #[test]
    fn solid_ascii_and_rings_share_selected_hue_and_descend_in_brightness() {
        let donut = AsciiDonut::default();
        let font = eframe::egui::FontId::monospace(11.0);
        let layout = donut.colored_layout(font.clone(), ColorMode::Solid, 0.0);
        let shifted = donut.colored_layout(font, ColorMode::Solid, 0.3);
        assert_eq!(layout.text, donut.text());
        assert_eq!(layout.sections.len(), 1);
        assert_eq!(shifted.sections.len(), 1);
        assert_ne!(layout.sections[0].format.color, shifted.sections[0].format.color);
        for (hue, ascii) in [(0.0, layout.sections[0].format.color), (0.3, shifted.sections[0].format.color)] {
            let rings = AsciiDonut::contour_colors(ColorMode::Solid, hue);
            let brightness = |color: eframe::egui::Color32| color.r().max(color.g()).max(color.b());
            assert!(brightness(ascii) > brightness(rings[0]));
            assert!(brightness(rings[0]) > brightness(rings[1]));
            assert!(brightness(rings[1]) > brightness(rings[2]));
            for color in [ascii, rings[0], rings[1], rings[2]] {
                let hsv = eframe::egui::ecolor::Hsva::from(color);
                assert!((hsv.h - hue).abs() < 0.015 || (hue == 0.0 && hsv.h > 0.985), "{hsv:?}");
            }
        }
    }

    #[test]
    fn rgb_spans_hues_shifts_with_tone_and_moves_while_mono_colors_remain_fixed() {
        let mut donut = AsciiDonut::default();
        let font = eframe::egui::FontId::monospace(11.0);
        let rainbow = donut.colored_layout(font.clone(), ColorMode::Rgb, 0.0);
        let colored: Vec<_> = rainbow.sections.iter().filter(|section| {
            rainbow.text[section.byte_range.start.0..section.byte_range.end.0]
                .bytes().any(|glyph| glyph != b' ' && glyph != b'\n')
        }).map(|section| section.format.color).collect();
        assert!(colored.len() > 2);
        assert!(colored.iter().any(|&color| color != colored[0]));
        let shifted = donut.colored_layout(font.clone(), ColorMode::Rgb, 0.3);
        assert_eq!(rainbow.text, shifted.text);
        assert_ne!(rainbow.sections[0].format.color, shifted.sections[0].format.color);
        let rings = AsciiDonut::contour_colors(ColorMode::Rgb, 0.0);
        let shifted_rings = AsciiDonut::contour_colors(ColorMode::Rgb, 0.3);
        assert!(rings.iter().zip(shifted_rings).all(|(a, b)| *a != b));
        for (hue, colors) in [(0.0, rings), (0.3, shifted_rings)] {
            let brightness = |color: eframe::egui::Color32| color.r().max(color.g()).max(color.b());
            assert!(brightness(colors[0]) > brightness(colors[1]));
            assert!(brightness(colors[1]) > brightness(colors[2]));
            for color in colors {
                let hsv = eframe::egui::ecolor::Hsva::from(color);
                assert!((hsv.h - hue).abs() < 0.015 || (hue == 0.0 && hsv.h > 0.985), "{hsv:?}");
            }
        }
        let mono = [
            (ColorMode::BlackWhite, donut.colored_layout(font.clone(), ColorMode::BlackWhite, 0.3).sections[0].format.color),
            (ColorMode::Solid, donut.colored_layout(font.clone(), ColorMode::Solid, 0.3).sections[0].format.color),
        ];
        donut.update(&[], 0, 0, 0, PlaybackState::Stopped, Duration::from_millis(90));
        let after = donut.colored_layout(font.clone(), ColorMode::Rgb, 0.0);
        assert_ne!(rainbow.sections[0].format.color, after.sections[0].format.color);
        for (mode, color) in mono {
            let layout = donut.colored_layout(font.clone(), mode, 0.3);
            assert_eq!(layout.sections.len(), 1);
            assert_eq!(color, layout.sections[0].format.color);
        }
        donut.update(&[], 0, 0, 0, PlaybackState::Paused, Duration::from_millis(90));
        assert_ne!(after.sections[0].format.color,
            donut.colored_layout(font, ColorMode::Rgb, 0.0).sections[0].format.color);
    }

    #[test]
    fn projected_front_view_is_round_in_eleven_pixel_monospace_cells() {
        let donut = AsciiDonut::default();
        let (mut left, mut right, mut top, mut bottom) = (WIDTH, 0, HEIGHT, 0);
        for (row, line) in donut.text().lines().enumerate() {
            for (col, glyph) in line.bytes().enumerate() {
                if glyph != b' ' {
                    left = left.min(col);
                    right = right.max(col);
                    top = top.min(row);
                    bottom = bottom.max(row);
                }
            }
        }
        let ratio = (right + 1 - left) as f32 * 6.6 / ((bottom + 1 - top) as f32 * 13.0);
        assert!((0.8..1.25).contains(&ratio), "pixel-space aspect ratio: {ratio}");
        assert!((195.0..=225.0).contains(&((right + 1 - left) as f32 * 6.6)));
    }

    #[test]
    fn incomplete_and_non_finite_pcm_never_produce_false_energy() {
        let mut donut = AsciiDonut::default();
        for (samples, channels, rate) in [
            (&[][..], 1, 48_000),
            (&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY][..], 2, 48_000),
            (&[1.0; 32][..], 0, 48_000),
            (&[1.0; 32][..], 1, 0),
        ] {
            donut.update(samples, channels, rate, u64::MAX, PlaybackState::Playing, Duration::MAX);
            assert!(donut.bands.iter().all(|&v| v == 0.0));
            assert_eq!(donut.text().lines().count(), HEIGHT);
            assert!(donut.text().lines().all(|row| row.len() == WIDTH));
        }
        let bad = [f32::NAN; 2049];
        donut.update(&bad, 2, 48_000, u64::MAX, PlaybackState::Playing, Duration::ZERO);
        assert!(donut.bands.iter().all(|&v| v == 0.0));
    }
}
