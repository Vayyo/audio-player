//! Ordered music paths and playback navigation, independent of the audio engine.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

pub struct MusicQueue {
    paths: Vec<PathBuf>,
    current_index: Option<usize>,
    folder: Option<PathBuf>,
}

impl MusicQueue {
    pub fn single(path: PathBuf) -> Self {
        Self {
            paths: vec![path],
            current_index: Some(0),
            folder: None,
        }
    }

    pub fn from_folder(folder: &Path) -> io::Result<Self> {
        let mut paths = Vec::new();
        scan_folder(folder, &mut paths)?;
        paths.sort_unstable();
        let current_index = (!paths.is_empty()).then_some(0);
        Ok(Self {
            paths,
            current_index,
            folder: Some(folder.to_path_buf()),
        })
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    pub fn current(&self) -> Option<&Path> {
        self.current_index.and_then(|index| self.path(index))
    }

    pub fn path(&self, index: usize) -> Option<&Path> {
        self.paths.get(index).map(PathBuf::as_path)
    }

    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn set_current(&mut self, index: usize) -> bool {
        if index >= self.paths.len() {
            return false;
        }
        self.current_index = Some(index);
        true
    }

    pub fn next_index(&self, repeat: RepeatMode) -> Option<usize> {
        let index = self.current_index?;
        if index + 1 < self.paths.len() {
            Some(index + 1)
        } else if repeat == RepeatMode::All {
            Some(0)
        } else {
            None
        }
    }

    pub fn previous_index(&self, repeat: RepeatMode) -> Option<usize> {
        let index = self.current_index?;
        if index > 0 {
            Some(index - 1)
        } else if repeat == RepeatMode::All {
            Some(self.paths.len() - 1)
        } else {
            None
        }
    }

    pub fn completed_index(&self, repeat: RepeatMode) -> Option<usize> {
        if repeat == RepeatMode::One {
            self.current_index
        } else {
            self.next_index(repeat)
        }
    }
}

fn scan_folder(folder: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            scan_folder(&path, paths)?;
        } else if supported_audio(&path)
            && (file_type.is_file()
                || file_type.is_symlink() && entry.metadata().is_ok_and(|meta| meta.is_file()))
        {
            paths.push(path);
        }
    }
    Ok(())
}

fn supported_audio(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
        ["mp3", "flac", "wav", "ogg"]
            .iter()
            .any(|supported| extension.eq_ignore_ascii_case(supported))
    })
}

#[cfg(test)]
mod tests {
    use super::{MusicQueue, RepeatMode};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempFolder(PathBuf);

    impl TempFolder {
        fn new() -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rap-queue-{}-{nonce}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create fixture folder");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("create fixture subfolder");
            fs::write(&path, b"fixture").expect("write fixture");
            path
        }
    }

    impl Drop for TempFolder {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("clean fixture folder");
        }
    }

    #[test]
    fn nested_supported_tracks_are_sorted_and_unrelated_formats_ignored() {
        let folder = TempFolder::new();
        let ogg = folder.file("b/nested/last.OGG");
        let mp3 = folder.file("a/first.Mp3");
        let wav = folder.file("b/second.wav");
        let flac = folder.file("b/third.FLAC");
        folder.file("b/ignore.txt");
        folder.file("track.mp3.backup");

        let queue = MusicQueue::from_folder(folder.path()).expect("scan folder");
        assert_eq!(queue.folder(), Some(folder.path()));
        assert_eq!(queue.len(), 4);
        assert!(!queue.is_empty());
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.current(), Some(mp3.as_path()));
        assert_eq!(queue.path(0), Some(mp3.as_path()));
        assert_eq!(queue.path(1), Some(ogg.as_path()));
        assert_eq!(queue.path(2), Some(wav.as_path()));
        assert_eq!(queue.path(3), Some(flac.as_path()));
        assert_eq!(queue.path(4), None);
    }

    #[test]
    fn empty_folder_and_invalid_root() {
        let folder = TempFolder::new();
        let mut queue = MusicQueue::from_folder(folder.path()).expect("scan empty folder");
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.folder(), Some(folder.path()));
        assert_eq!(queue.current_index(), None);
        assert_eq!(queue.current(), None);
        assert_eq!(queue.path(0), None);
        assert!(!queue.set_current(0));
        for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(queue.next_index(repeat), None);
            assert_eq!(queue.previous_index(repeat), None);
            assert_eq!(queue.completed_index(repeat), None);
        }
        let ordinary_file = folder.file("file.mp3");
        assert!(MusicQueue::from_folder(&ordinary_file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directory_cycle_is_not_traversed() {
        let folder = TempFolder::new();
        let track = folder.file("sub/real.mp3");
        std::os::unix::fs::symlink(folder.path(), folder.path().join("sub/loop"))
            .expect("make directory cycle");
        let queue = MusicQueue::from_folder(folder.path()).expect("scan without following cycle");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.current(), Some(track.as_path()));
    }

    #[test]
    fn repeat_transitions_and_manual_navigation() {
        assert_eq!(RepeatMode::Off.cycle(), RepeatMode::All);
        assert_eq!(RepeatMode::All.cycle(), RepeatMode::One);
        assert_eq!(RepeatMode::One.cycle(), RepeatMode::Off);

        let folder = TempFolder::new();
        let first = folder.file("a.mp3");
        let last = folder.file("z.ogg");
        let mut queue = MusicQueue::from_folder(folder.path()).expect("scan two tracks");
        assert_eq!(queue.current(), Some(first.as_path()));
        assert_eq!(queue.previous_index(RepeatMode::Off), None);
        assert_eq!(queue.previous_index(RepeatMode::One), None);
        assert_eq!(queue.previous_index(RepeatMode::All), Some(1));
        for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(queue.next_index(repeat), Some(1));
        }
        assert_eq!(queue.completed_index(RepeatMode::One), Some(0));
        assert_eq!(queue.completed_index(RepeatMode::Off), Some(1));
        assert_eq!(queue.completed_index(RepeatMode::All), Some(1));

        assert!(queue.set_current(1));
        assert_eq!(queue.current(), Some(last.as_path()));
        assert_eq!(queue.current_index(), Some(1));
        assert_eq!(queue.next_index(RepeatMode::Off), None);
        assert_eq!(queue.next_index(RepeatMode::One), None);
        assert_eq!(queue.next_index(RepeatMode::All), Some(0));
        assert_eq!(queue.completed_index(RepeatMode::Off), None);
        assert_eq!(queue.completed_index(RepeatMode::All), Some(0));
        assert_eq!(queue.completed_index(RepeatMode::One), Some(1));
        for repeat in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(queue.previous_index(repeat), Some(0));
        }
        assert!(!queue.set_current(2));
        assert_eq!(queue.current(), Some(last.as_path()));
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn single_track_has_no_manual_navigation_without_all_repeat() {
        let track = PathBuf::from("example.wav");
        let queue = MusicQueue::single(track.clone());
        assert_eq!(queue.folder(), None);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.current(), Some(track.as_path()));
        for repeat in [RepeatMode::Off, RepeatMode::One] {
            assert_eq!(queue.next_index(repeat), None);
            assert_eq!(queue.previous_index(repeat), None);
        }
        assert_eq!(queue.next_index(RepeatMode::All), Some(0));
        assert_eq!(queue.previous_index(RepeatMode::All), Some(0));
        assert_eq!(queue.completed_index(RepeatMode::Off), None);
        assert_eq!(queue.completed_index(RepeatMode::All), Some(0));
        assert_eq!(queue.completed_index(RepeatMode::One), Some(0));
    }
}
