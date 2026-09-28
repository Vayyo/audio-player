//! Playback state machine of the engine: play/pause/stop, position and volume.
//!
//! Everything is headless: the player only tracks where playback is, it does not
//! touch an audio device. Position is expressed in frames from the start of the
//! track, volume is a linear gain in `[0.0, 1.0]`.

use std::fmt;

/// Transport state of the player.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackState {
    /// Not playing, position is at the start of the track.
    Stopped,
    /// Playing; [`Player::advance`] moves the position forward.
    Playing,
    /// Suspended at the current position.
    Paused,
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
        })
    }
}

/// Deterministic transport for one track of `total_frames` frames.
///
/// Transition rules (all transitions are idempotent):
///
/// * [`Player::play`] — `Stopped` and `Paused` become `Playing`; `Playing` stays.
///   The position is preserved except at the end, where Play restarts at `0`.
/// * [`Player::pause`] — `Playing` becomes `Paused` and keeps the position;
///   `Stopped` and `Paused` are unaffected.
/// * [`Player::stop`] — any state becomes `Stopped` and the position resets to `0`.
/// * [`Player::seek`] — sets the position to `min(frame, total_frames)` in every
///   state, without changing it.
/// * [`Player::advance`] — only moves the position while `Playing`, saturating at
///   `total_frames`.
#[derive(Clone, Debug)]
pub struct Player {
    state: PlaybackState,
    position: u64,
    total_frames: u64,
    volume: f32,
}

impl Player {
    /// Create a `Stopped` player for a track of `total_frames` frames, volume `1.0`.
    pub fn new(total_frames: u64) -> Self {
        Self { state: PlaybackState::Stopped, position: 0, total_frames, volume: 1.0 }
    }

    /// Current transport state.
    pub fn state(&self) -> PlaybackState {
        self.state
    }

    /// Current position in frames from the start of the track.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Total length of the track in frames.
    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// Current linear volume in `[0.0, 1.0]`.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Set the volume, clamped into `[0.0, 1.0]`; `NaN` silences the output.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_nan() { 0.0 } else { volume.clamp(0.0, 1.0) };
    }

    /// Start or resume playback, restarting at the beginning when at the end.
    pub fn play(&mut self) {
        if self.position == self.total_frames {
            self.position = 0;
        }
        self.state = PlaybackState::Playing;
    }

    /// Suspend playback at the current position. No-op unless playing.
    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            self.state = PlaybackState::Paused;
        }
    }

    /// Stop playback and rewind to the start of the track.
    pub fn stop(&mut self) {
        self.state = PlaybackState::Stopped;
        self.position = 0;
    }

    /// Move the position to `frame`, clamped into `[0, total_frames]`.
    ///
    /// Seeking beyond the end lands exactly on `total_frames`, seeking to `0`
    /// rewinds to the start, and the state never changes.
    pub fn seek(&mut self, frame: u64) {
        self.position = frame.min(self.total_frames);
    }

    /// Advance the position by `frames` while playing, saturating at `total_frames`.
    pub fn advance(&mut self, frames: u64) {
        if self.state != PlaybackState::Playing {
            return;
        }
        self.position = self.position.saturating_add(frames).min(self.total_frames);
    }

    /// Apply the current volume to interleaved samples in place.
    ///
    /// Volume `1.0` leaves the samples bit-for-bit unchanged, volume `0.0`
    /// produces exact silence, and any other volume scales by the linear gain.
    pub fn apply_volume(&self, samples: &mut [f32]) {
        if self.volume == 1.0 {
            return;
        }
        if self.volume == 0.0 {
            samples.fill(0.0);
            return;
        }
        for sample in samples.iter_mut() {
            *sample *= self.volume;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackState, Player};

    #[test]
    fn new_player_is_stopped_at_start_with_full_volume() {
        let player = Player::new(1_000);

        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.position(), 0);
        assert_eq!(player.total_frames(), 1_000);
        assert_eq!(player.volume(), 1.0);
    }

    #[test]
    fn play_pause_stop_transitions() {
        let mut player = Player::new(1_000);

        player.pause();
        assert_eq!(player.state(), PlaybackState::Stopped, "pause must not start playback");

        player.play();
        assert_eq!(player.state(), PlaybackState::Playing);
        player.play();
        assert_eq!(player.state(), PlaybackState::Playing, "play is idempotent");

        player.advance(10);
        player.pause();
        assert_eq!(player.state(), PlaybackState::Paused);
        assert_eq!(player.position(), 10, "pause keeps the position");
        player.advance(10);
        assert_eq!(player.position(), 10, "paused transport does not advance");

        player.play();
        assert_eq!(player.state(), PlaybackState::Playing, "play resumes from pause");
        assert_eq!(player.position(), 10);

        player.stop();
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.position(), 0, "stop rewinds to the start");

        player.stop();
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.position(), 0);
    }

    #[test]
    fn advance_saturates_at_the_end() {
        let mut player = Player::new(100);
        player.play();

        player.advance(60);
        assert_eq!(player.position(), 60);
        player.advance(u64::MAX);
        assert_eq!(player.position(), 100);
        player.advance(1);
        assert_eq!(player.position(), 100);
    }

    #[test]
    fn seek_is_clamped_and_state_independent() {
        let mut player = Player::new(500);

        player.seek(200);
        assert_eq!(player.position(), 200);
        assert_eq!(player.state(), PlaybackState::Stopped, "seeking does not start playback");

        player.seek(u64::MAX);
        assert_eq!(player.position(), 500, "seeking past the end lands exactly on total_frames");

        player.seek(0);
        assert_eq!(player.position(), 0);

        player.play();
        player.seek(500);
        assert_eq!(player.position(), 500);
        player.seek(501);
        assert_eq!(player.position(), 500, "seek beyond the end stays clamped while playing");
        player.seek(0);
        assert_eq!(player.position(), 0, "rewinding while playing is allowed");
        assert_eq!(player.state(), PlaybackState::Playing);

        player.pause();
        player.seek(7_000);
        assert_eq!(player.position(), 500);
        assert_eq!(player.state(), PlaybackState::Paused);
    }

    #[test]
    fn play_at_endpoint_restarts_but_seek_and_pause_preserve_endpoint() {
        let mut player = Player::new(4);
        for state in [PlaybackState::Stopped, PlaybackState::Paused, PlaybackState::Playing] {
            player.stop();
            if state != PlaybackState::Stopped {
                player.play();
                if state == PlaybackState::Paused {
                    player.pause();
                }
            }
            player.seek(u64::MAX);
            assert_eq!(player.position(), 4);
            assert_eq!(player.state(), state);
            player.play();
            assert_eq!(player.state(), PlaybackState::Playing);
            assert_eq!(player.position(), 0);
            player.advance(1);
            assert_eq!(player.position(), 1, "first Play must make PCM available again");
        }
    }

    #[test]
    fn seek_in_empty_track_stays_at_zero() {
        let mut player = Player::new(0);

        player.seek(50);
        assert_eq!(player.position(), 0);
        player.play();
        player.advance(50);
        assert_eq!(player.position(), 0);
    }

    #[test]
    fn volume_is_clamped() {
        let mut player = Player::new(10);

        player.set_volume(0.5);
        assert_eq!(player.volume(), 0.5);
        player.set_volume(2.0);
        assert_eq!(player.volume(), 1.0);
        player.set_volume(-3.0);
        assert_eq!(player.volume(), 0.0);
        player.set_volume(1.0);
        assert_eq!(player.volume(), 1.0);
        player.set_volume(f32::NAN);
        assert_eq!(player.volume(), 0.0, "NaN volume silences the output");
    }

    #[test]
    fn volume_zero_produces_exact_silence() {
        let mut player = Player::new(10);
        player.set_volume(0.0);

        let mut samples = [0.5_f32, -0.25, 1.0, -1.0, 0.0];
        player.apply_volume(&mut samples);
        assert_eq!(samples, [0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn volume_one_leaves_samples_bitwise_unchanged() {
        let player = Player::new(10);
        assert_eq!(player.volume(), 1.0);

        let original = [0.5_f32, -0.25, 1.0, -1.0, 0.0, f32::MIN_POSITIVE];
        let mut samples = original;
        player.apply_volume(&mut samples);

        for (before, after) in original.iter().zip(samples.iter()) {
            assert_eq!(before.to_bits(), after.to_bits(), "sample changed at volume 1.0");
        }
    }

    #[test]
    fn volume_scales_samples_linearly() {
        let mut player = Player::new(10);
        player.set_volume(0.5);

        let mut samples = [1.0_f32, -1.0, 0.5, 0.0];
        player.apply_volume(&mut samples);
        assert_eq!(samples, [0.5, -0.5, 0.25, 0.0]);
    }
}
