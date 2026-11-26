//! Files copied on another computer, brought to this Mac without passing
//! through the transfers folder.
//!
//! A small copy is fetched at once and put on the clipboard as ordinary file
//! URLs, so every program pastes it without a wait. A large one is held until
//! it is pasted here: the paste is what fetches it, as a drop does a drag.
//! Pasted in Finder it goes straight into the folder, where a placeholder
//! shows it arriving; pasted elsewhere it goes on the clipboard, to be pasted
//! again once it is all here.
//!
//! Not as a list whose files are asked for when something pastes. That was
//! tried: the pasteboard asks once, on the main thread, the first time
//! anything reads the URL, and remote-desktop programs and input methods read
//! every new clipboard at once. A 2 GB copy was fetched the moment it was made
//! elsewhere, with this app's main thread and that reader waiting on it for
//! four minutes; and a reader that is turned away is never asked again.

use crate::drag_buffer::FileBuffer;
use crate::drag_place::Placeholder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) struct OfferFile {
    pub transfer_id: String,
    pub name: String,
    pub size: u64,
    pub directory: bool,
}

type Done = Box<dyn FnOnce(Vec<PathBuf>) + Send>;

struct Session {
    by_transfer_id: HashMap<String, Arc<FileBuffer>>,
    order: Vec<Arc<FileBuffer>>,
    fetch: Box<dyn Fn() + Send + Sync>,
    fetch_started: AtomicBool,
    /// Puts the files on the clipboard once they are here.
    done: Mutex<Option<Done>>,
    /// Held until pasted, and the clipboard as it was when the copy was
    /// offered: once something else is copied here, a paste means that.
    held: AtomicBool,
    offered_at: Option<u64>,
    /// What stands in the folder the copy was pasted into: one for each file
    /// or folder copied, with how much of it has arrived.
    shown: Mutex<Vec<(String, u64, Placeholder)>>,
    /// Pasted into a folder, and the clipboard's no longer: a newer copy, which
    /// takes the clipboard's place, leaves it to finish.
    into_folder: AtomicBool,
}

/// The copy the clipboard or the next paste is to have, and those on their
/// way into the folders they were pasted into.
static SESSIONS: Mutex<Vec<Arc<Session>>> = Mutex::new(Vec::new());

fn session_of(transfer_id: &str) -> Option<Arc<Session>> {
    SESSIONS.lock().ok()?.iter().find(|session| session.by_transfer_id.contains_key(transfer_id)).cloned()
}

fn held_session() -> Option<Arc<Session>> {
    SESSIONS.lock().ok()?.iter().find(|session| session.held.load(Ordering::Relaxed)).cloned()
}

/// Done with `session`: later transfers under its ids are not this copy's.
/// False when it had been given up already.
fn forget(session: &Arc<Session>) -> bool {
    SESSIONS.lock().is_ok_and(|mut sessions| {
        let before = sessions.len();
        sessions.retain(|kept| !Arc::ptr_eq(kept, session));
        sessions.len() != before
    })
}

/// What a file is part of in the folder it is pasted into: itself, or the
/// folder that was copied with it inside (as `drag_buffer::save_all` has it).
fn top_of(name: &str) -> &str {
    Path::new(name)
        .components()
        .next()
        .and_then(|first| first.as_os_str().to_str())
        .unwrap_or(name)
}

impl Session {
    fn cancel(&self) {
        for buffer in &self.order {
            buffer.abort();
        }
        self.unshow();
    }

    fn unshow(&self) {
        if let Ok(mut shown) = self.shown.lock() {
            shown.clear();
        }
    }

    /// Ask for the files, wait for them, and leave them in a folder of
    /// their own.
    fn save(&self) -> Result<Vec<PathBuf>, String> {
        if !self.fetch_started.swap(true, Ordering::Relaxed) {
            log::info!("fetching the copied files: {} item(s)", self.order.len());
            (self.fetch)();
        }
        crate::drag_buffer::save_all(&self.order).map_err(|error| error.to_string())
    }
}

// --- called by the transfer path -------------------------------------------

/// True while `transfer_id` is a file of the copy being fetched, so its bytes
/// come here instead of landing in the transfers folder.
pub(crate) fn session_wants(transfer_id: &str) -> bool {
    session_of(transfer_id).is_some()
}

/// Whether `transfer_id` is arriving in a folder it was pasted into, where a
/// placeholder shows it: no pop-up says so as well.
pub(crate) fn pasted_into_folder(transfer_id: &str) -> bool {
    session_of(transfer_id).is_some_and(|session| session.shown.lock().is_ok_and(|shown| !shown.is_empty()))
}

pub(crate) fn feed_chunk(transfer_id: &str, data: &[u8]) -> bool {
    let Some(session) = session_of(transfer_id) else {
        return false;
    };
    let Some(buffer) = session.by_transfer_id.get(transfer_id) else {
        return false;
    };
    if !buffer.append(data) {
        return false;
    }
    if let Ok(mut shown) = session.shown.lock() {
        let top = top_of(&buffer.name);
        if let Some((_, arrived, placeholder)) = shown.iter_mut().find(|(name, _, _)| name == top) {
            *arrived += data.len() as u64;
            placeholder.arrived(*arrived);
        }
    }
    true
}

pub(crate) fn finish_file(transfer_id: &str) -> bool {
    session_of(transfer_id)
        .and_then(|session| session.by_transfer_id.get(transfer_id).map(|buffer| buffer.finish()))
        .unwrap_or(false)
}

/// The sender gave up on one file, could not be asked for them, or this Mac
/// no longer wants the copy: nothing of it can be pasted whole.
pub(crate) fn abort_file(transfer_id: &str) -> bool {
    let Some(session) = session_of(transfer_id) else {
        return false;
    };
    forget(&session);
    session.cancel();
    true
}

// --- called when an offer arrives ------------------------------------------

fn start(
    files: Vec<OfferFile>,
    fetch: Box<dyn Fn() + Send + Sync>,
    done: Done,
    held: bool,
) -> Option<Arc<Session>> {
    if !crate::drag_buffer::offer_acceptable(files.iter().map(|file| (file.name.as_str(), file.size, file.directory))) {
        return None;
    }
    let mut order = Vec::with_capacity(files.len());
    let mut by_transfer_id = HashMap::with_capacity(files.len());
    for file in files {
        let buffer = Arc::new(FileBuffer::new(file.name, file.size, file.directory).ok()?);
        by_transfer_id.insert(file.transfer_id, Arc::clone(&buffer));
        order.push(buffer);
    }
    let session = Arc::new(Session {
        by_transfer_id,
        order,
        fetch,
        fetch_started: AtomicBool::new(false),
        done: Mutex::new(Some(done)),
        held: AtomicBool::new(held),
        offered_at: crate::clipboard::change_count(),
        shown: Mutex::new(Vec::new()),
        into_folder: AtomicBool::new(false),
    });
    // A newer copy replaces whatever an earlier one left for the clipboard:
    // held for a paste, or on its way there. Not one being put into a folder:
    // copying the same file again took a paste away half done.
    let mut sessions = SESSIONS.lock().ok()?;
    sessions.retain(|earlier| {
        let stays = earlier.into_folder.load(Ordering::Relaxed);
        if !stays {
            earlier.cancel();
        }
        stays
    });
    sessions.push(Arc::clone(&session));
    Some(session)
}

/// Fetch the copy, then put it into `folder`, or on the clipboard when there
/// is none. `pasted_in` is the process a paste asked for it in, with no folder
/// to put it into: the paste was kept back, and is made once the copy is on
/// the clipboard.
fn run(session: Arc<Session>, folder: Option<PathBuf>, pasted_in: Option<i32>) {
    std::thread::spawn(move || match session.save() {
        Ok(paths) if forget(&session) => {
            match folder {
                Some(folder) => {
                    for path in &paths {
                        crate::drag_place::place(path, &folder);
                    }
                    // Where they were fetched to is empty now. Only an empty
                    // folder goes: one that could not be moved stays in it.
                    if let Some(fetched_to) = paths.first().and_then(|path| path.parent()) {
                        let _ = std::fs::remove_dir(fetched_to);
                    }
                    // A placeholder goes once what it stood for is there.
                    session.unshow();
                }
                None => {
                    let before = crate::clipboard::change_count();
                    if let Some(done) = session.done.lock().ok().and_then(|mut done| done.take()) {
                        done(paths);
                    }
                    // Not if the clipboard was not written, which would paste
                    // what it held before; nor into whatever has come to the
                    // front since, which did not ask for it.
                    if pasted_in.is_some()
                        && crate::clipboard::change_count() != before
                        && crate::input::macos_appkit::frontmost_pid() == pasted_in
                    {
                        log::info!("the copy held for pasting is here: pasting it where it was asked for");
                        crate::input::paste_received_clipboard();
                    }
                }
            }
        }
        Ok(_) => {}
        Err(error) => {
            log::warn!("the copied files could not be fetched: {error}");
            forget(&session);
            session.unshow();
        }
    });
}

/// Fetch a copy now and hand `done` where its files were saved; they are then
/// written as ordinary files, which every program pastes without a wait.
pub(crate) fn fetch_now(files: Vec<OfferFile>, fetch: Box<dyn Fn() + Send + Sync>, done: Done) -> bool {
    start(files, fetch, done, false).map(|session| run(session, None, None)).is_some()
}

/// Keep a large copy for the paste that wants it (`take_held`). Copied
/// elsewhere to be pasted elsewhere, it never crosses the network.
pub(crate) fn hold(files: Vec<OfferFile>, fetch: Box<dyn Fn() + Send + Sync>, done: Done) -> bool {
    start(files, fetch, done, true).is_some()
}

// --- called when something is pasted here -----------------------------------

/// A held copy, taken by the paste that fetches it.
pub(crate) struct Held(Arc<Session>);

/// Whether a copy is waiting for a paste here.
pub(crate) fn holding() -> bool {
    held_session().is_some()
}

/// The held copy a paste here now means, if there is one. It is no longer
/// that once something else has been copied on this Mac, and is then let go.
pub(crate) fn take_held() -> Option<Held> {
    let session = held_session()?;
    if crate::clipboard::change_count() != session.offered_at {
        log::info!("something else was copied here since: the copy held for pasting is let go");
        forget(&session);
        session.cancel();
        return None;
    }
    session.held.swap(false, Ordering::Relaxed).then_some(Held(session))
}

impl Held {
    /// Fetch the copy: into `folder`, where each file or folder copied shows
    /// as a placeholder until it is there, or on the clipboard when there is
    /// no folder.
    pub(crate) fn fetch(self, folder: Option<PathBuf>) {
        let session = self.0;
        match &folder {
            Some(folder) => {
                let mut sizes: Vec<(&str, u64)> = Vec::new();
                for buffer in &session.order {
                    let top = top_of(&buffer.name);
                    match sizes.iter_mut().find(|(name, _)| *name == top) {
                        Some((_, size)) => *size += buffer.size,
                        None => sizes.push((top, buffer.size)),
                    }
                }
                let shown = sizes
                    .into_iter()
                    .filter_map(|(top, size)| Some((top.to_string(), 0, Placeholder::new(folder, top, size)?)))
                    .collect();
                if let Ok(mut slot) = session.shown.lock() {
                    *slot = shown;
                }
                session.into_folder.store(true, Ordering::Relaxed);
                log::info!("the copy held for pasting goes into {}", folder.display());
            }
            None => log::info!("the copy held for pasting is fetched to the clipboard"),
        }
        let pasted_in = if folder.is_none() { crate::input::macos_appkit::frontmost_pid() } else { None };
        run(session, folder, pasted_in);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_stands_for_itself_and_one_in_a_copied_folder_for_that_folder() {
        assert_eq!(top_of("report.pdf"), "report.pdf");
        assert_eq!(top_of("Photos/2026/a.jpg"), "Photos");
    }

    #[test]
    fn a_newer_copy_replaces_one_for_the_clipboard_but_not_one_going_into_a_folder() {
        let offer = |id: &str| {
            let file = OfferFile { transfer_id: id.into(), name: "a.bin".into(), size: 1, directory: false };
            start(vec![file], Box::new(|| {}), Box::new(|_| {}), true).expect("the offer is taken")
        };
        let pasted = offer("offer-pasted");
        pasted.held.store(false, Ordering::Relaxed);
        pasted.into_folder.store(true, Ordering::Relaxed);
        offer("offer-held");
        offer("offer-newer");
        assert!(session_wants("offer-pasted"), "being put into a folder, it goes on");
        assert!(!session_wants("offer-held"), "nobody pasted it, and a newer copy took its place");
        assert!(session_wants("offer-newer"));
        // Given up from here, or by its sender: the others are not touched.
        assert!(abort_file("offer-pasted"));
        assert!(!session_wants("offer-pasted") && session_wants("offer-newer"));
        assert!(abort_file("offer-newer"));
    }
}
