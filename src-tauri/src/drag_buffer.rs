use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
                file: if directory {
                    None
                } else {
                    Some(tempfile::NamedTempFile::new()?)
                },
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
        let result = state
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("directory stream"))
            .and_then(|file| {
                file.as_file_mut().seek(SeekFrom::Start(offset))?;
                file.as_file_mut().write_all(bytes)
            });
        if result.is_err() {
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

    pub fn received(&self) -> u64 {
        self.state.lock().map(|s| s.received).unwrap_or(0)
    }

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
    fn truncated_or_oversized_transfers_cannot_look_complete() {
        let buffer = FileBuffer::new("short.bin".into(), 2, false).unwrap();
        assert!(buffer.append(b"a"));
        assert!(!buffer.finish());
        let buffer = FileBuffer::new("large.bin".into(), 2, false).unwrap();
        assert!(!buffer.append(b"abc"));
    }
}
