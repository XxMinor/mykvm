use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// How long fetched files are kept: a chat window reads its attachment when
/// the message is sent, which can be long after the paste.
const SAVED_FILES_KEPT: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether a list of offered files (name, size, is a folder) can be taken:
/// not empty, not too many, and every name a path that stays inside the
/// folder the copy is saved to.
pub(crate) fn offer_acceptable<'a>(files: impl ExactSizeIterator<Item = (&'a str, u64, bool)>) -> bool {
    let count = files.len();
    count > 0
        && count <= crate::folder_transfer::MAX_ENTRIES
        && files.into_iter().all(|(name, size, directory)| {
            name.encode_utf16().count() < 260
                && crate::folder_transfer::validate_relative_path(std::path::Path::new(name)).is_ok()
                && !(directory && size != 0)
        })
}

/// Wait for every file and leave them in a folder of their own, under the
/// names they were listed by. Returns the top-level entries: what was copied.
pub(crate) fn save_all(files: &[std::sync::Arc<FileBuffer>]) -> std::io::Result<Vec<std::path::PathBuf>> {
    let root = std::env::temp_dir().join("MyKVM Clipboard");
    for old in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        let age = old.metadata().and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok());
        if age.is_some_and(|age| age > SAVED_FILES_KEPT) {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let folder = root.join(format!("{stamp}-{}", std::process::id()));
    let mut copied = Vec::new();
    for file in files {
        // Checked when the offer was taken (`offer_acceptable`): relative,
        // and never climbing out of the folder.
        let name = std::path::Path::new(&file.name);
        file.save_to(&folder.join(name))?;
        if let Some(first) = name.components().next() {
            let top = folder.join(first);
            if !copied.contains(&top) {
                copied.push(top);
            }
        }
    }
    log::info!("the copied files were saved: {} item(s) in {}", copied.len(), folder.display());
    Ok(copied)
}

pub(crate) struct FileBuffer {
    pub name: String,
    pub size: u64,
    pub directory: bool,
    state: Mutex<State>,
    ready: Condvar,
}

struct State {
    file: Option<tempfile::NamedTempFile>,
    received: u64,
    complete: bool,
    aborted: bool,
}

impl FileBuffer {
    pub fn new(name: String, size: u64, directory: bool) -> std::io::Result<Self> {
        Ok(Self {
            name,
            size,
            directory,
            state: Mutex::new(State {
                // Made when the first bytes arrive: a copied folder is listed
                // as thousands of these before anything is pasted.
                file: None,
                received: 0,
                complete: directory,
                aborted: false,
            }),
            ready: Condvar::new(),
        })
    }

    pub fn append(&self, bytes: &[u8]) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.aborted
            || state.complete
            || state.received.saturating_add(bytes.len() as u64) > self.size
        {
            state.aborted = true;
            self.ready.notify_all();
            return false;
        }
        let offset = state.received;
        let result = if self.directory {
            Err(std::io::Error::other("directory stream"))
        } else {
            if state.file.is_none() {
                state.file = tempfile::NamedTempFile::new().ok();
            }
            state
                .file
                .as_mut()
                .ok_or_else(|| std::io::Error::other("no spool file"))
                .and_then(|file| {
                    file.as_file_mut().seek(SeekFrom::Start(offset))?;
                    file.as_file_mut().write_all(bytes)
                })
        };
        if let Err(error) = result {
            // Was silent: a full or unwritable temp folder only showed as a
            // transfer that stopped.
            log::warn!("{} could not be spooled: {error}", self.name);
            state.aborted = true;
            self.ready.notify_all();
            return false;
        }
        state.received += bytes.len() as u64;
        self.ready.notify_all();
        true
    }

    pub fn finish(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.aborted || state.received != self.size {
            state.aborted = true;
            self.ready.notify_all();
            return false;
        }
        state.complete = true;
        self.ready.notify_all();
        true
    }

    pub fn abort(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.aborted = true;
        }
        self.ready.notify_all();
    }

    /// Wait for the whole file, then leave a copy of it at `path`.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let stuck = |what| std::io::Error::other(what);
        let mut state = self.state.lock().map_err(|_| stuck("spool lock"))?;
        while !state.complete {
            if state.aborted {
                return Err(stuck("the transfer was cancelled"));
            }
            let before = state.received;
            let (next, timeout) = self
                .ready
                .wait_timeout(state, Duration::from_secs(30))
                .map_err(|_| stuck("spool lock"))?;
            state = next;
            if timeout.timed_out() && state.received == before && !state.complete {
                return Err(stuck("the transfer stalled"));
            }
        }
        if state.aborted {
            return Err(stuck("the transfer was cancelled"));
        }
        if self.directory {
            return std::fs::create_dir_all(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut copy = std::fs::File::create(path)?;
        // An empty file never got a spool.
        if let Some(spool) = state.file.as_mut() {
            spool.as_file_mut().seek(SeekFrom::Start(0))?;
            std::io::copy(spool.as_file_mut(), &mut copy)?;
        }
        Ok(())
    }

    // The two below serve the Windows drag stream.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn received(&self) -> u64 {
        self.state.lock().map(|s| s.received).unwrap_or(0)
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn read_at(&self, pos: u64, out: &mut [u8]) -> Result<usize, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        loop {
            if state.aborted {
                return Err(());
            }
            if pos >= self.size || out.is_empty() {
                return Ok(0);
            }
            if pos < state.received {
                let count = out.len().min((state.received - pos) as usize);
                let file = state.file.as_mut().ok_or(())?.as_file_mut();
                file.seek(SeekFrom::Start(pos)).map_err(|_| ())?;
                file.read_exact(&mut out[..count]).map_err(|_| ())?;
                return Ok(count);
            }
            if state.complete {
                return Ok(0);
            }
            let (next, timeout) = self
                .ready
                .wait_timeout(state, Duration::from_secs(30))
                .map_err(|_| ())?;
            state = next;
            if timeout.timed_out() && state.received <= pos && !state.complete {
                return Err(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_is_refused_when_a_name_would_leave_its_folder() {
        let acceptable = |files: &[(&str, u64, bool)]| offer_acceptable(files.iter().copied());
        assert!(acceptable(&[("a.txt", 3, false), ("album/b.txt", 1, false), ("album", 0, true)]));
        assert!(!acceptable(&[]));
        assert!(!acceptable(&[("../escape.txt", 1, false)]));
        assert!(!acceptable(&[("/etc/passwd", 1, false)]));
        // A folder is listed without a size of its own.
        assert!(!acceptable(&[("album", 5, true)]));
    }
    use std::sync::Arc;

    #[test]
    fn reads_and_appends_share_a_disk_spool_without_changing_offsets() {
        let buffer = FileBuffer::new("example.txt".into(), 6, false).unwrap();
        assert!(buffer.append(b"abc"));
        let mut out = [0; 3];
        assert_eq!(buffer.read_at(0, &mut out), Ok(3));
        assert_eq!(&out, b"abc");
        assert!(buffer.append(b"def"));
        assert!(buffer.finish());
        assert_eq!(buffer.read_at(3, &mut out), Ok(3));
        assert_eq!(&out, b"def");
        assert_eq!(buffer.read_at(6, &mut out), Ok(0));
        assert_eq!(buffer.received(), 6);
    }

    #[test]
    fn abort_wakes_a_waiting_reader_and_refuses_later_chunks() {
        let buffer = Arc::new(FileBuffer::new("pending.bin".into(), 2, false).unwrap());
        let read = Arc::clone(&buffer);
        let worker = std::thread::spawn(move || read.read_at(0, &mut [0; 2]));
        buffer.abort();
        assert_eq!(worker.join().unwrap(), Err(()));
        assert!(!buffer.append(b"ab"));
    }

    #[test]
    fn a_copy_is_saved_once_the_whole_file_has_arrived() {
        let dir = tempfile::tempdir().unwrap();
        let buffer = Arc::new(FileBuffer::new("report.txt".into(), 6, false).unwrap());
        let feed = Arc::clone(&buffer);
        let worker = std::thread::spawn(move || {
            assert!(feed.append(b"abc"));
            std::thread::sleep(Duration::from_millis(30));
            assert!(feed.append(b"def") && feed.finish());
        });
        let path = dir.path().join("deep/report.txt");
        buffer.save_to(&path).unwrap();
        worker.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");

        let empty = FileBuffer::new("empty.txt".into(), 0, false).unwrap();
        assert!(empty.finish());
        empty.save_to(&dir.path().join("empty.txt")).unwrap();
        assert_eq!(std::fs::read(dir.path().join("empty.txt")).unwrap(), b"");
        FileBuffer::new("album/".into(), 0, true).unwrap().save_to(&dir.path().join("album")).unwrap();
        assert!(dir.path().join("album").is_dir());

        let cancelled = FileBuffer::new("gone.bin".into(), 2, false).unwrap();
        cancelled.abort();
        assert!(cancelled.save_to(&dir.path().join("gone.bin")).is_err());
        assert!(!dir.path().join("gone.bin").exists());
    }

    #[test]
    fn truncated_or_oversized_transfers_cannot_look_complete() {
        let buffer = FileBuffer::new("short.bin".into(), 2, false).unwrap();
        assert!(buffer.append(b"a"));
        assert!(!buffer.finish());
        let buffer = FileBuffer::new("large.bin".into(), 2, false).unwrap();
        assert!(!buffer.append(b"abc"));
    }
}
