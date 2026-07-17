//! Native OLE drag-drop on the controlled Windows client.
//!
//! When a file drag on the controlling Mac crosses onto this machine, the Mac
//! sends a drag-start control message plus the file bytes; this module runs a
//! real `DoDragDrop` session so Windows renders the native drag image and any
//! drop target (an Explorer folder, WeChat, a mail client, …) accepts the drop.
//! The virtual files are exposed through an `IDataObject`; each file's bytes are
//! served by an `IStream` backed by a buffer the transfer path fills, so the
//! drop target reads each file as it streams in.
//!
//! DROP TIMING is decided by our own `IDropSource` from flags this module sets
//! (`signal_drop` / `cancel_session`), not from the physical button — the drag
//! is driven by injected input, so there is no real button to watch. A synthetic
//! left-button down/up pair brackets the session so `DoDragDrop` treats it as a
//! real drag and so the terminal button-up wakes `QueryContinueDrag`.
//!
//! NOTE: this cannot be exercised on the macOS build host. It type-checks for
//! the Windows target (cargo xwin) but its runtime behavior — the modal
//! DoDragDrop loop driven by injected input and the streaming IStream read by
//! the drop target — needs verification on real Windows hardware.
#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_ABORT, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED,
    S_OK, STG_E_ACCESSDENIED,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl,
    IStream, IStream_Impl, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM,
    STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
    TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, DROPEFFECT,
    DROPEFFECT_COPY,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
    FD_FILESIZE, FILEDESCRIPTORW,
};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT,
};

/// Files whose transfer feeds a drag session. `transfer_id` matches the id the
/// controller stamps on the file-transfer packets it streams for this drag.
pub struct DragFileMeta {
    pub transfer_id: String,
    pub name: String,
    pub size: u64,
}

// One in-flight drag at a time (there is one cursor), so a single global slot.
static ACTIVE_SESSION: OnceLock<Mutex<Option<Arc<DragSession>>>> = OnceLock::new();

fn session_slot() -> &'static Mutex<Option<Arc<DragSession>>> {
    ACTIVE_SESSION.get_or_init(|| Mutex::new(None))
}

fn active_session() -> Option<Arc<DragSession>> {
    session_slot().lock().ok().and_then(|slot| slot.clone())
}

struct DragSession {
    order: Vec<Arc<FileBuffer>>,
    by_transfer_id: HashMap<String, Arc<FileBuffer>>,
    // Drop/cancel decided here and read by the IDropSource.
    released: Mutex<bool>,
    cancelled: Mutex<bool>,
}

impl DragSession {
    fn is_released(&self) -> bool {
        self.released.lock().map(|v| *v).unwrap_or(true)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.lock().map(|v| *v).unwrap_or(true)
    }
}

/// Growable buffer backing one file's `IStream`. The transfer thread appends
/// bytes and marks completion; the drop target's stream read blocks here until
/// enough bytes have arrived (or the session is aborted).
struct FileBuffer {
    name: String,
    size: u64,
    state: Mutex<FileBufferState>,
    cond: Condvar,
}

struct FileBufferState {
    data: Vec<u8>,
    complete: bool,
    aborted: bool,
}

impl FileBuffer {
    fn new(name: String, size: u64) -> Self {
        Self {
            name,
            size,
            state: Mutex::new(FileBufferState {
                data: Vec::new(),
                complete: false,
                aborted: false,
            }),
            cond: Condvar::new(),
        }
    }

    fn append(&self, bytes: &[u8]) {
        if let Ok(mut state) = self.state.lock() {
            state.data.extend_from_slice(bytes);
        }
        self.cond.notify_all();
    }

    fn finish(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.complete = true;
        }
        self.cond.notify_all();
    }

    fn abort(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.aborted = true;
        }
        self.cond.notify_all();
    }

    /// Copy up to `out.len()` bytes starting at `pos`, blocking until that many
    /// bytes exist, the file is complete, or the session aborts. Returns the
    /// number of bytes copied (0 = end of stream). `Err` means aborted/stalled.
    fn read_at(&self, pos: u64, out: &mut [u8]) -> Result<usize, ()> {
        let Ok(mut state) = self.state.lock() else {
            return Err(());
        };
        loop {
            if state.aborted {
                return Err(());
            }
            if pos >= self.size {
                return Ok(0);
            }
            let available = state.data.len() as u64;
            if pos < available {
                let start = pos as usize;
                let n = out.len().min((available - pos) as usize);
                out[..n].copy_from_slice(&state.data[start..start + n]);
                return Ok(n);
            }
            // No bytes yet at this position.
            if state.complete {
                return Ok(0);
            }
            // Bounded wait so a wedged transfer can never hang the drop target.
            let (next, timeout) = self
                .cond
                .wait_timeout(state, Duration::from_secs(30))
                .map_err(|_| ())?;
            state = next;
            if timeout.timed_out() && state.data.len() as u64 <= pos && !state.complete {
                return Err(());
            }
        }
    }
}

// --- public API called by the transfer/receive path -----------------------

/// True while `transfer_id` belongs to the active drag session, so the transfer
/// handler feeds its bytes here instead of writing them to disk.
pub fn session_wants(transfer_id: &str) -> bool {
    active_session()
        .map(|session| session.by_transfer_id.contains_key(transfer_id))
        .unwrap_or(false)
}

pub fn feed_chunk(transfer_id: &str, data: &[u8]) -> bool {
    let Some(session) = active_session() else {
        return false;
    };
    let Some(buffer) = session.by_transfer_id.get(transfer_id) else {
        return false;
    };
    buffer.append(data);
    true
}

pub fn finish_file(transfer_id: &str) -> bool {
    let Some(session) = active_session() else {
        return false;
    };
    let Some(buffer) = session.by_transfer_id.get(transfer_id) else {
        return false;
    };
    buffer.finish();
    true
}

/// The controller released the drag over this machine: perform the drop.
pub fn signal_drop() {
    if let Some(session) = active_session() {
        if let Ok(mut released) = session.released.lock() {
            *released = true;
        }
    }
    // The synthetic button-up both balances the start-of-drag down and wakes
    // DoDragDrop's QueryContinueDrag so it observes the released flag.
    inject_left_button(false);
}

/// The controller left without dropping (or capture stopped): abort the drag.
pub fn cancel_session() {
    if let Some(session) = active_session() {
        if let Ok(mut cancelled) = session.cancelled.lock() {
            *cancelled = true;
        }
        for buffer in &session.order {
            buffer.abort();
        }
    }
    inject_left_button(false);
}

/// Begin a drag session and spawn the DoDragDrop thread. Returns false if a
/// drag is already in flight or `files` is empty.
pub fn start_drag_session(files: Vec<DragFileMeta>) -> bool {
    if files.is_empty() {
        return false;
    }
    let Ok(mut slot) = session_slot().lock() else {
        return false;
    };
    if slot.is_some() {
        return false;
    }

    let mut order = Vec::with_capacity(files.len());
    let mut by_transfer_id = HashMap::with_capacity(files.len());
    for meta in files {
        let buffer = Arc::new(FileBuffer::new(meta.name, meta.size));
        by_transfer_id.insert(meta.transfer_id, Arc::clone(&buffer));
        order.push(buffer);
    }
    let session = Arc::new(DragSession {
        order,
        by_transfer_id,
        released: Mutex::new(false),
        cancelled: Mutex::new(false),
    });
    *slot = Some(Arc::clone(&session));
    drop(slot);

    // Hold a synthetic left button so DoDragDrop treats this as a live drag.
    inject_left_button(true);

    std::thread::spawn(move || {
        run_drag_thread(session);
    });
    true
}

fn clear_session() {
    if let Ok(mut slot) = session_slot().lock() {
        *slot = None;
    }
}

// --- the DoDragDrop thread -------------------------------------------------

fn run_drag_thread(session: Arc<DragSession>) {
    unsafe {
        // DoDragDrop requires an OLE-initialized STA thread.
        let _ = OleInitialize(None);

        let streams: Vec<IStream> = session
            .order
            .iter()
            .map(|buffer| FileReadStream::new(Arc::clone(buffer)))
            .collect();

        let data_object: IDataObject = DragDataObject {
            files: session.order.clone(),
            streams,
        }
        .into();
        let drop_source: IDropSource = DragDropSource {
            session: Arc::clone(&session),
        }
        .into();

        let mut effect = DROPEFFECT::default();
        let _ = DoDragDrop(&data_object, &drop_source, DROPEFFECT_COPY, &mut effect);

        // Whatever the outcome, release the synthetic button and clear state.
        inject_left_button(false);
        for buffer in &session.order {
            buffer.abort();
        }
        clear_session();
        OleUninitialize();
    }
}

// --- clipboard formats -----------------------------------------------------

struct DragFormats {
    file_descriptor: u16,
    file_contents: u16,
}

fn drag_formats() -> &'static DragFormats {
    static FORMATS: OnceLock<DragFormats> = OnceLock::new();
    FORMATS.get_or_init(|| unsafe {
        DragFormats {
            file_descriptor: RegisterClipboardFormatW(w("FileGroupDescriptorW")) as u16,
            file_contents: RegisterClipboardFormatW(w("FileContents")) as u16,
        }
    })
}

// PCWSTR from a &str literal — leaks a small NUL-terminated UTF-16 buffer once
// per distinct string; only called for the two fixed clipboard-format names.
fn w(value: &str) -> PCWSTR {
    let mut units: Vec<u16> = value.encode_utf16().collect();
    units.push(0);
    let boxed = units.into_boxed_slice();
    let ptr = boxed.as_ptr();
    std::mem::forget(boxed);
    PCWSTR(ptr)
}

// --- IStream over a FileBuffer --------------------------------------------

#[implement(IStream)]
struct FileReadStream {
    buffer: Arc<FileBuffer>,
    pos: Mutex<u64>,
}

impl FileReadStream {
    fn new(buffer: Arc<FileBuffer>) -> IStream {
        FileReadStream {
            buffer,
            pos: Mutex::new(0),
        }
        .into()
    }
}

impl ISequentialStream_Impl for FileReadStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> windows::core::HRESULT {
        let mut pos = match self.pos.lock() {
            Ok(pos) => pos,
            Err(_) => return E_NOTIMPL,
        };
        let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        match self.buffer.read_at(*pos, out) {
            Ok(n) => {
                *pos += n as u64;
                if !pcbread.is_null() {
                    unsafe { *pcbread = n as u32 };
                }
                S_OK
            }
            Err(()) => {
                if !pcbread.is_null() {
                    unsafe { *pcbread = 0 };
                }
                E_ABORT
            }
        }
    }

    fn Write(
        &self,
        _pv: *const core::ffi::c_void,
        _cb: u32,
        _pcbwritten: *mut u32,
    ) -> windows::core::HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileReadStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().map_err(|_| windows::core::Error::from(E_NOTIMPL))?;
        let base = match dworigin {
            STREAM_SEEK_SET => 0i64,
            STREAM_SEEK_CUR => *pos as i64,
            STREAM_SEEK_END => self.buffer.size as i64,
            _ => return Err(windows::core::Error::from(E_NOTIMPL)),
        };
        let next = (base + dlibmove).max(0) as u64;
        *pos = next;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = next };
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(windows::core::Error::from(STG_E_ACCESSDENIED))
    }

    fn CopyTo(
        &self,
        pstm: windows::core::Ref<'_, IStream>,
        cb: u64,
        pcbread: *mut u64,
        pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        let target = pstm.ok()?;
        let mut remaining = cb;
        let mut total_read = 0u64;
        let mut total_written = 0u64;
        let mut chunk = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(chunk.len() as u64) as usize;
            let pos = *self.pos.lock().map_err(|_| windows::core::Error::from(E_NOTIMPL))?;
            let n = match self.buffer.read_at(pos, &mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => n,
                Err(()) => return Err(windows::core::Error::from(E_ABORT)),
            };
            if let Ok(mut p) = self.pos.lock() {
                *p += n as u64;
            }
            total_read += n as u64;
            let mut written = 0u32;
            unsafe {
                target
                    .Write(chunk.as_ptr() as *const _, n as u32, Some(&mut written))
                    .ok()?;
            }
            total_written += written as u64;
            remaining -= n as u64;
        }
        if !pcbread.is_null() {
            unsafe { *pcbread = total_read };
        }
        if !pcbwritten.is_null() {
            unsafe { *pcbwritten = total_written };
        }
        Ok(())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(windows::core::Error::from(E_NOTIMPL));
        }
        let mut stat = STATSTG::default();
        stat.r#type = STGTY_STREAM.0 as u32;
        stat.cbSize = self.buffer.size;
        unsafe { *pstatstg = stat };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        let pos = *self.pos.lock().map_err(|_| windows::core::Error::from(E_NOTIMPL))?;
        let stream: IStream = FileReadStream {
            buffer: Arc::clone(&self.buffer),
            pos: Mutex::new(pos),
        }
        .into();
        Ok(stream)
    }
}

// --- IDataObject exposing the virtual files --------------------------------

#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DragDataObject {
    files: Vec<Arc<FileBuffer>>,
    streams: Vec<IStream>,
}

impl DragDataObject_Impl {
    fn supported(&self, format: &FORMATETC) -> bool {
        let formats = drag_formats();
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return false;
        }
        if format.cfFormat == formats.file_descriptor {
            format.tymed & TYMED_HGLOBAL.0 as u32 != 0
        } else if format.cfFormat == formats.file_contents {
            format.tymed & TYMED_ISTREAM.0 as u32 != 0
        } else {
            false
        }
    }
}

impl IDataObject_Impl for DragDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { pformatetcin.as_ref() }
            .ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
        let formats = drag_formats();
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return Err(windows::core::Error::from(DV_E_FORMATETC));
        }

        if format.cfFormat == formats.file_descriptor {
            if format.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                return Err(windows::core::Error::from(DV_E_TYMED));
            }
            let hglobal = build_file_group_descriptor(&self.files)?;
            return Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hglobal },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }

        if format.cfFormat == formats.file_contents {
            if format.tymed & TYMED_ISTREAM.0 as u32 == 0 {
                return Err(windows::core::Error::from(DV_E_TYMED));
            }
            let index = format.lindex;
            if index < 0 || index as usize >= self.streams.len() {
                return Err(windows::core::Error::from(DV_E_LINDEX));
            }
            let stream = self.streams[index as usize].clone();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }

        Err(windows::core::Error::from(DV_E_FORMATETC))
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
        match unsafe { pformatetc.as_ref() } {
            Some(format) if self.supported(format) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        if !pformatetcout.is_null() {
            unsafe {
                (*pformatetcout).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        // Drop targets may push a preferred-effect blob; accept and ignore it.
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        const DATADIR_GET: u32 = 1;
        if dwdirection != DATADIR_GET {
            return Err(windows::core::Error::from(E_NOTIMPL));
        }
        let formats = drag_formats();
        let entries = [
            FORMATETC {
                cfFormat: formats.file_descriptor,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            },
            FORMATETC {
                cfFormat: formats.file_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_ISTREAM.0 as u32,
            },
        ];
        unsafe { SHCreateStdEnumFmtEtc(&entries) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: windows::core::Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(windows::core::Error::from(
            OLE_E_ADVISENOTSUPPORTED,
        ))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(windows::core::Error::from(
            OLE_E_ADVISENOTSUPPORTED,
        ))
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(windows::core::Error::from(
            OLE_E_ADVISENOTSUPPORTED,
        ))
    }
}

impl IDataObjectAsyncCapability_Impl for DragDataObject_Impl {
    fn SetAsyncMode(&self, _fdoopasync: windows::core::BOOL) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<windows::core::BOOL> {
        // Always async: our stream reads block on the network, so the drop
        // target must extract on a worker thread, not its UI thread.
        Ok(true.into())
    }

    fn StartOperation(
        &self,
        _pbcreserved: windows::core::Ref<'_, IBindCtx>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<windows::core::BOOL> {
        Ok(true.into())
    }

    fn EndOperation(
        &self,
        _hresult: windows::core::HRESULT,
        _pbcreserved: windows::core::Ref<'_, IBindCtx>,
        _dweffects: u32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

fn build_file_group_descriptor(files: &[Arc<FileBuffer>]) -> windows::core::Result<HGLOBAL> {
    let count = files.len();
    let header = std::mem::size_of::<u32>();
    let each = std::mem::size_of::<FILEDESCRIPTORW>();
    // FILEGROUPDESCRIPTORW already contains one descriptor; add the rest.
    let bytes = header + each * count;

    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes)? };
    let ptr = unsafe { GlobalLock(hglobal) } as *mut u8;
    if ptr.is_null() {
        return Err(windows::core::Error::from(E_NOTIMPL));
    }
    unsafe {
        std::ptr::write(ptr as *mut u32, count as u32);
        let descriptors = ptr.add(header) as *mut FILEDESCRIPTORW;
        for (i, file) in files.iter().enumerate() {
            let mut descriptor = FILEDESCRIPTORW {
                dwFlags: FD_FILESIZE.0 as u32,
                nFileSizeHigh: (file.size >> 32) as u32,
                nFileSizeLow: (file.size & 0xFFFF_FFFF) as u32,
                ..Default::default()
            };
            let name: Vec<u16> = file.name.encode_utf16().take(259).collect();
            for (j, unit) in name.iter().enumerate() {
                descriptor.cFileName[j] = *unit;
            }
            std::ptr::write(descriptors.add(i), descriptor);
        }
        let _ = GlobalUnlock(hglobal);
    }
    Ok(hglobal)
}

// --- IDropSource: decides drop/cancel from our flags -----------------------

#[implement(IDropSource)]
struct DragDropSource {
    session: Arc<DragSession>,
}

impl IDropSource_Impl for DragDropSource_Impl {
    fn QueryContinueDrag(
        &self,
        _fescapepressed: windows::core::BOOL,
        _grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if self.session.is_cancelled() {
            return DRAGDROP_S_CANCEL;
        }
        if self.session.is_released() {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> windows::core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// --- synthetic left button (brackets the drag) -----------------------------

fn inject_left_button(down: bool) {
    let flag = if down {
        MOUSEEVENTF_LEFTDOWN
    } else {
        MOUSEEVENTF_LEFTUP
    };
    let mut input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: flag,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &mut input, std::mem::size_of::<INPUT>() as i32);
    }
}
