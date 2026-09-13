//! Port of `td/utils/buffer.h` + `td/utils/buffer.cpp`: tdlib's buffer layer.
//!
//! The same single-writer/multi-reader discipline as in C++ is kept: a
//! `BufferRaw` is one allocation shared by exactly one writer (via
//! [`BufferWriterPtr`]) and any number of readers (via [`BufferReaderPtr`]).
//! The writer exclusively owns `[0, begin)` and `[end, data_size)`, readers
//! only ever read `[begin, end)`, and `begin` is frozen once the first reader
//! exists — exactly the C++ contract, enforced by `CHECK`s in debug builds.

use std::cell::{RefCell, UnsafeCell};
use std::ops::Range;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MIN_BUFFER_SIZE: usize = 512;
const TLS_BUFFER_SIZE: usize = 4096 * 4;

/// Rounds `size` up to a multiple of 8 (`(size + 7) & -8`).
fn round8(size: usize) -> usize {
    (size + 7) & !7
}

static BUFFER_MEM: AtomicUsize = AtomicUsize::new(0);

/// `td::BufferAllocator::get_buffer_mem()`: live memory held by raw buffers.
pub fn get_buffer_mem() -> usize {
    BUFFER_MEM.load(Ordering::Relaxed)
}

/// `td::BufferRaw`: a fixed-capacity allocation shared between one writer and
/// many readers.
///
/// # Safety
///
/// The memory discipline above is enforced by construction: `begin` and `end`
/// are atomics published by the writer, and the data area is an
/// [`UnsafeCell`] so the writer can hand out `&mut` windows that no reader
/// may concurrently access.
pub(crate) struct BufferRaw {
    data: UnsafeCell<Box<[u8]>>,
    /// First byte of the used region. Written only while no reader exists.
    begin: AtomicUsize,
    /// End of the used region. Written by the writer, read by readers.
    end: AtomicUsize,
    has_writer: AtomicBool,
    was_reader: AtomicBool,
    /// Total bytes charged to [`BUFFER_MEM`], `max(size_of::<Self>(), offset + data_size)`.
    alloc_size: usize,
}

impl BufferRaw {
    fn new(size: usize) -> Arc<Self> {
        let size = round8(size);
        let offset = size_of::<Self>();
        let alloc_size = offset.saturating_add(size).max(offset);
        BUFFER_MEM.fetch_add(alloc_size, Ordering::Relaxed);
        Arc::new(Self {
            data: UnsafeCell::new(vec![0u8; size].into_boxed_slice()),
            begin: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            has_writer: AtomicBool::new(true),
            was_reader: AtomicBool::new(false),
            alloc_size,
        })
    }

    fn data_size(&self) -> usize {
        // SAFETY: the data pointer is only dereferenced here, never through
        // the UnsafeCell's interior while a conflicting borrow could exist.
        unsafe { &*self.data.get() }.len()
    }

    /// `&[u8]` over `range` of the data area — reader side.
    ///
    /// # Safety
    ///
    /// `range` must lie within `[begin, end)` and no writer must hold an
    /// `&mut` over an overlapping range (the C++ discipline).
    unsafe fn slice_range(&self, range: Range<usize>) -> &[u8] {
        let data = unsafe { &*self.data.get() };
        &data[range]
    }

    /// `&mut [u8]` over `range` of the data area — writer side.
    ///
    /// # Safety
    ///
    /// `range` must lie within `[0, begin)` or `[end, data_size)` and no
    /// reader may access it (the C++ discipline).
    #[allow(clippy::mut_from_ref)]
    unsafe fn slice_range_mut(&self, range: Range<usize>) -> &mut [u8] {
        let data = unsafe { &mut *self.data.get() };
        &mut data[range]
    }
}

impl Drop for BufferRaw {
    fn drop(&mut self) {
        BUFFER_MEM.fetch_sub(self.alloc_size, Ordering::Relaxed);
    }
}

// SAFETY: `BufferRaw` follows the single-writer/multi-reader discipline of
// the C++ class: interior `&mut` windows are handed out only to the unique
// writer, while shared access reads the atomics and the frozen `[begin, end)`
// data. Without this impl, `Arc<BufferRaw>` could not cross threads at all.
unsafe impl Sync for BufferRaw {}

/// `td::BufferAllocator::WriterPtr`: owns the writer side of a raw buffer and
/// publishes `has_writer = false` when dropped.
pub struct BufferWriterPtr {
    raw: Option<Arc<BufferRaw>>,
}

impl BufferWriterPtr {
    fn new(raw: Arc<BufferRaw>) -> Self {
        Self { raw: Some(raw) }
    }

    pub(crate) fn raw(&self) -> Option<&Arc<BufferRaw>> {
        self.raw.as_ref()
    }
}

impl Drop for BufferWriterPtr {
    fn drop(&mut self) {
        if let Some(raw) = &self.raw {
            raw.has_writer.store(false, Ordering::Release);
        }
    }
}

/// `td::BufferAllocator::ReaderPtr`: owns a reader reference on a raw buffer.
#[derive(Clone)]
pub struct BufferReaderPtr {
    raw: Option<Arc<BufferRaw>>,
}

impl BufferReaderPtr {
    fn new(raw: Arc<BufferRaw>) -> Self {
        Self { raw: Some(raw) }
    }

    fn null() -> Self {
        Self { raw: None }
    }

    fn raw(&self) -> Option<&Arc<BufferRaw>> {
        self.raw.as_ref()
    }
}

thread_local! {
    /// `td::BufferAllocator::buffer_raw_tls`: the per-thread 16 KiB slab that
    /// small readers are carved from.
    static TLS_BUFFER_RAW: RefCell<Option<Arc<BufferRaw>>> = const { RefCell::new(None) };
}

/// `td::BufferAllocator::create_writer(size)`: at least 512 bytes.
pub fn create_writer(size: usize) -> BufferWriterPtr {
    create_writer_exact(size.max(MIN_BUFFER_SIZE))
}

fn create_writer_exact(size: usize) -> BufferWriterPtr {
    BufferWriterPtr::new(BufferRaw::new(size))
}

/// `td::BufferAllocator::create_writer(size, prepend, append)`: reserves
/// `prepend` bytes before and `append` after a used region of `size` bytes.
pub fn create_writer_with_reserve(size: usize, prepend: usize, append: usize) -> BufferWriterPtr {
    let ptr = create_writer(size + prepend + append);
    let raw = ptr.raw().expect("fresh writer is non-null");
    raw.begin.fetch_add(prepend, Ordering::Release);
    raw.end.fetch_add(prepend + size, Ordering::Release);
    ptr
}

/// `td::BufferAllocator::create_reader(size)`: a reader over `size` bytes.
/// Small readers share the thread-local slab, large ones get a fresh buffer.
pub fn create_reader(size: usize) -> BufferReaderPtr {
    if size < MIN_BUFFER_SIZE {
        create_reader_fast(size)
    } else {
        let raw = BufferRaw::new(size);
        raw.end.fetch_add(round8(size), Ordering::Release);
        BufferReaderPtr::new(raw)
    }
}

fn create_reader_fast(size: usize) -> BufferReaderPtr {
    let size = round8(size);
    TLS_BUFFER_RAW.with(|tls| {
        let mut tls = tls.borrow_mut();
        let raw = match tls.as_ref() {
            Some(raw) if raw.data_size() - raw.end.load(Ordering::Relaxed) >= size => {
                Arc::clone(raw)
            }
            _ => {
                let raw = BufferRaw::new(TLS_BUFFER_SIZE);
                *tls = Some(Arc::clone(&raw));
                raw
            }
        };
        raw.end.fetch_add(size, Ordering::Relaxed);
        BufferReaderPtr::new(raw)
    })
}

/// `td::BufferAllocator::create_reader(WriterPtr)`: freezes `begin`.
pub fn create_reader_from_writer(ptr: &BufferWriterPtr) -> BufferReaderPtr {
    let raw = Arc::clone(ptr.raw().expect("reader of a null writer"));
    raw.was_reader.store(true, Ordering::Release);
    BufferReaderPtr::new(raw)
}

/// `td::BufferAllocator::create_reader(ReaderPtr)`.
pub fn create_reader_from_reader(ptr: &BufferReaderPtr) -> BufferReaderPtr {
    BufferReaderPtr::new(Arc::clone(ptr.raw().expect("reader of a null reader")))
}

/// `td::BufferSlice`: a counted window `[begin, end)` into a raw buffer.
pub struct BufferSlice {
    buffer: BufferReaderPtr,
    begin: usize,
    end: usize,
}

impl BufferSlice {
    /// Default-constructed null slice.
    pub fn null() -> Self {
        Self {
            buffer: BufferReaderPtr::null(),
            begin: 0,
            end: 0,
        }
    }

    /// `BufferSlice(BufferReaderPtr)`: an empty window synced with the writer.
    pub fn from_reader(buffer: BufferReaderPtr) -> Self {
        let Some(raw) = buffer.raw() else {
            return Self {
                buffer,
                begin: 0,
                end: 0,
            };
        };
        let begin = raw.begin.load(Ordering::Acquire);
        let mut this = Self {
            buffer,
            begin,
            end: begin,
        };
        this.sync_with_writer();
        this
    }

    /// `BufferSlice(BufferReaderPtr, begin, end)`.
    pub fn from_reader_range(buffer: BufferReaderPtr, begin: usize, end: usize) -> Self {
        Self { buffer, begin, end }
    }

    /// `BufferSlice(size_t size)`: `size` bytes at the tail of a fresh reader.
    pub fn with_size(size: usize) -> Self {
        let buffer = create_reader(size);
        let raw = buffer.raw().expect("fresh reader is non-null");
        let end = raw.end.load(Ordering::Relaxed);
        let begin = end - round8(size);
        Self {
            buffer,
            begin,
            end: begin + size,
        }
    }

    /// `BufferSlice(Slice slice)`.
    pub fn from_slice_bytes(slice: &[u8]) -> Self {
        let mut this = Self::with_size(slice.len());
        this.as_mutable_slice().copy_from_slice(slice);
        this
    }

    /// `BufferSlice::copy()`: deep copy.
    pub fn copy(&self) -> Self {
        if self.is_null() {
            return Self {
                buffer: BufferReaderPtr::null(),
                begin: self.begin,
                end: self.end,
            };
        }
        Self::from_slice_bytes(self.as_slice())
    }

    pub fn as_slice(&self) -> &[u8] {
        let Some(raw) = self.buffer.raw() else {
            return &[];
        };
        // SAFETY: `[begin, end)` is within the reader's readable region and
        // the writer cannot hand out `&mut` to it.
        unsafe { raw.slice_range(self.begin..self.end) }
    }

    pub fn as_mutable_slice(&mut self) -> &mut [u8] {
        let Some(raw) = self.buffer.raw() else {
            return &mut [];
        };
        // SAFETY: mirror of `as_slice`; the C++ class hands out `&mut` over
        // its own window the same way (used by copy_from before publication).
        unsafe { raw.slice_range_mut(self.begin..self.end) }
    }

    pub fn prepare_read(&self) -> &[u8] {
        self.as_slice()
    }

    /// `BufferSlice::after(offset)`.
    pub fn after(&self, offset: usize) -> &[u8] {
        &self.as_slice()[offset.min(self.size())..]
    }

    /// `BufferSlice::confirm_read(size)`: returns whether the window is spent.
    pub fn confirm_read(&mut self, size: usize) -> bool {
        self.begin += size;
        assert!(self.begin <= self.end, "confirm_read past end");
        self.begin == self.end
    }

    /// `BufferSlice::truncate(limit)`.
    pub fn truncate(&mut self, limit: usize) {
        if self.size() > limit {
            self.end = self.begin + limit;
        }
    }

    /// `BufferSlice::from_slice(slice)`: re-windows over `slice`, which must
    /// point into this slice's buffer.
    pub fn from_slice(&self, slice: &[u8]) -> Self {
        let raw = self.buffer.raw().expect("from_slice on a null BufferSlice");
        let data = raw.data_size();
        let data_start = unsafe { &*raw.data.get() }.as_ptr() as usize;
        let slice_start = slice.as_ptr() as usize;
        assert!(slice_start >= data_start);
        let begin = slice_start - data_start;
        let end = begin + slice.len();
        let raw_begin = raw.begin.load(Ordering::Acquire);
        let raw_end = raw.end.load(Ordering::Acquire);
        assert!(
            raw_begin <= begin && begin <= end && end <= raw_end,
            "from_slice out of range"
        );
        assert!(end <= data);
        Self {
            buffer: create_reader_from_reader(&self.buffer),
            begin,
            end,
        }
    }

    pub fn data(&self) -> &[u8] {
        self.as_slice()
    }

    pub fn empty(&self) -> bool {
        self.size() == 0
    }

    pub fn is_null(&self) -> bool {
        self.buffer.raw().is_none()
    }

    pub fn size(&self) -> usize {
        if self.is_null() {
            0
        } else {
            self.end - self.begin
        }
    }

    pub fn length(&self) -> usize {
        self.size()
    }

    /// `BufferSlice::sync_with_writer()`: extends `end` to the writer's
    /// current end; returns the number of new bytes.
    pub fn sync_with_writer(&mut self) -> usize {
        assert!(!self.is_null(), "sync_with_writer on a null BufferSlice");
        let raw = self.buffer.raw().expect("checked non-null above");
        let old_end = self.end;
        self.end = raw.end.load(Ordering::Acquire);
        self.end - old_end
    }

    /// `BufferSlice::is_writer_alive()`.
    pub fn is_writer_alive(&self) -> bool {
        let raw = self
            .buffer
            .raw()
            .expect("is_writer_alive on a null BufferSlice");
        raw.has_writer.load(Ordering::Acquire)
    }

    /// `BufferSlice::clear()`.
    pub fn clear(&mut self) {
        self.begin = 0;
        self.end = 0;
        self.buffer = BufferReaderPtr::null();
    }
}

impl Default for BufferSlice {
    fn default() -> Self {
        Self::null()
    }
}

/// `BufferSlice::clone()`: shares the underlying buffer.
impl Clone for BufferSlice {
    fn clone(&self) -> Self {
        if self.is_null() {
            return Self {
                buffer: BufferReaderPtr::null(),
                begin: self.begin,
                end: self.end,
            };
        }
        Self {
            buffer: create_reader_from_reader(&self.buffer),
            begin: self.begin,
            end: self.end,
        }
    }
}

impl From<&[u8]> for BufferSlice {
    fn from(slice: &[u8]) -> Self {
        Self::from_slice_bytes(slice)
    }
}

/// `td::BufferWriter`: the writer side of a raw buffer.
pub struct BufferWriter {
    buffer: BufferWriterPtr,
}

impl Default for BufferWriter {
    /// `BufferWriter()`: a null writer.
    fn default() -> Self {
        Self::null()
    }
}

impl BufferWriter {
    pub fn new(size: usize) -> Self {
        Self {
            buffer: create_writer(size),
        }
    }

    /// `BufferWriter(size, prepend, append)`.
    pub fn with_reserve(size: usize, prepend: usize, append: usize) -> Self {
        Self {
            buffer: create_writer_with_reserve(size, prepend, append),
        }
    }

    /// `BufferWriter(Slice, prepend, append)`.
    pub fn from_slice_with_reserve(slice: &[u8], prepend: usize, append: usize) -> Self {
        let mut writer = Self::with_reserve(slice.len(), prepend, append);
        writer.as_mutable_slice().copy_from_slice(slice);
        writer
    }

    /// A null writer (no allocation).
    pub fn null() -> Self {
        Self {
            buffer: BufferWriterPtr { raw: None },
        }
    }

    pub fn as_buffer_slice(&self) -> BufferSlice {
        match &self.buffer.raw {
            Some(_) => BufferSlice::from_reader(create_reader_from_writer(&self.buffer)),
            None => BufferSlice::null(),
        }
    }

    pub fn is_null(&self) -> bool {
        self.buffer.raw.is_none()
    }

    pub fn empty(&self) -> bool {
        self.size() == 0
    }

    pub fn size(&self) -> usize {
        let Some(raw) = &self.buffer.raw else {
            return 0;
        };
        raw.end.load(Ordering::Relaxed) - raw.begin.load(Ordering::Acquire)
    }

    pub fn as_slice(&self) -> &[u8] {
        let Some(raw) = &self.buffer.raw else {
            return &[];
        };
        let begin = raw.begin.load(Ordering::Acquire);
        let end = raw.end.load(Ordering::Relaxed);
        // SAFETY: the used region is readable while this writer is alive.
        unsafe { raw.slice_range(begin..end) }
    }

    pub fn as_mutable_slice(&mut self) -> &mut [u8] {
        let Some(raw) = &self.buffer.raw else {
            return &mut [];
        };
        let begin = raw.begin.load(Ordering::Acquire);
        let end = raw.end.load(Ordering::Relaxed);
        // SAFETY: the writer exclusively owns the used region for writing.
        unsafe { raw.slice_range_mut(begin..end) }
    }

    /// `BufferWriter::prepare_prepend()`: the reserved region before `begin`.
    /// Invalidated once a reader exists (`was_reader_`).
    pub fn prepare_prepend(&mut self) -> &mut [u8] {
        let Some(raw) = &self.buffer.raw else {
            return &mut [];
        };
        assert!(
            !raw.was_reader.load(Ordering::Acquire),
            "prepare_prepend after a reader was created"
        );
        let begin = raw.begin.load(Ordering::Acquire);
        // SAFETY: `[0, begin)` is writer-exclusive before any reader exists.
        unsafe { raw.slice_range_mut(0..begin) }
    }

    /// `BufferWriter::prepare_append()`: the reserved region after `end`.
    pub fn prepare_append(&mut self) -> &mut [u8] {
        let Some(raw) = &self.buffer.raw else {
            return &mut [];
        };
        let end = raw.end.load(Ordering::Relaxed);
        let data_size = raw.data_size();
        // SAFETY: `[end, data_size)` is writer-exclusive.
        unsafe { raw.slice_range_mut(end..data_size) }
    }

    /// `BufferWriter::confirm_append(size)`.
    pub fn confirm_append(&mut self, size: usize) {
        let Some(raw) = &self.buffer.raw else {
            assert_eq!(size, 0, "confirm_append on a null writer");
            return;
        };
        let new_end = raw.end.load(Ordering::Relaxed) + size;
        assert!(new_end <= raw.data_size(), "confirm_append past capacity");
        raw.end.store(new_end, Ordering::Release);
    }

    /// `BufferWriter::confirm_prepend(size)`.
    pub fn confirm_prepend(&mut self, size: usize) {
        let Some(raw) = &self.buffer.raw else {
            assert_eq!(size, 0, "confirm_prepend on a null writer");
            return;
        };
        let begin = raw.begin.load(Ordering::Acquire);
        assert!(begin >= size, "confirm_prepend past capacity");
        raw.begin.store(begin - size, Ordering::Release);
    }
}

pub(crate) struct ChainBufferNode {
    slice: BufferSlice,
    sync_flag: bool,
    /// Owned by the writer until published; afterwards an owning reference
    /// on the successor, freed by refcounting like `ReaderPtr next_` in C++.
    next: ChainBufferNodeReaderPtr,
    ref_cnt: AtomicUsize,
    has_writer: AtomicBool,
}

// SAFETY: `ChainBufferNode` follows the same single-writer/multi-reader
// discipline as `BufferRaw`; `next` is either writer-exclusive or frozen and
// published through the refcount (acquire/release). Nodes move between
// threads with their owning ptrs.
unsafe impl Send for ChainBufferNode {}
unsafe impl Sync for ChainBufferNode {}

impl ChainBufferNode {
    fn new(slice: BufferSlice, sync_flag: bool) -> Self {
        Self {
            slice,
            sync_flag,
            next: ChainBufferNodeReaderPtr::null(),
            ref_cnt: AtomicUsize::new(0),
            has_writer: AtomicBool::new(false),
        }
    }

    fn make_writer_ptr(ptr: *mut ChainBufferNode) -> ChainBufferNodeWriterPtr {
        // SAFETY: `ptr` is a fresh allocation with refcount 0.
        unsafe {
            (*ptr).ref_cnt.store(1, Ordering::Relaxed);
            (*ptr).has_writer.store(true, Ordering::Relaxed);
        }
        ChainBufferNodeWriterPtr(ptr)
    }

    fn make_reader_ptr(ptr: *mut ChainBufferNode) -> ChainBufferNodeReaderPtr {
        // SAFETY: `ptr` is a live node with a stable refcount.
        unsafe { (*ptr).ref_cnt.fetch_add(1, Ordering::AcqRel) };
        ChainBufferNodeReaderPtr(ptr)
    }

    fn has_writer(&self) -> bool {
        self.has_writer.load(Ordering::Acquire)
    }

    fn unique(&self) -> bool {
        self.ref_cnt.load(Ordering::Acquire) == 1
    }

    /// `td::ChainBufferNode::clear_nonrecursive`: frees a unique suffix of the
    /// chain iteratively so dropping a long chain cannot overflow the stack.
    ///
    /// # Safety
    ///
    /// `next` must be the owning handle moved out of a node whose refcount
    /// just reached zero.
    unsafe fn clear_nonrecursive(mut next: ChainBufferNodeReaderPtr) {
        while let Some(raw) = next.raw() {
            // SAFETY: `raw` is live while the `next` handle holds a reference.
            if !unsafe { (*raw).unique() } {
                // The node is shared; dropping our one handle here is a
                // single refcount level, never a recursive chain.
                break;
            }
            // Move the successor handle out so freeing `raw` below cannot
            // recurse through its `next` (the C++ `ptr = std::move(ptr->next_)`).
            let deeper =
                unsafe { std::mem::replace(&mut (*raw).next, ChainBufferNodeReaderPtr::null()) };
            next = deeper;
            // Replacing `next` drops the old handle: dec_ref_cnt frees `raw`
            // iteratively, with its `next` already nulled.
        }
    }

    /// # Safety
    ///
    /// `ptr` must be a live node.
    unsafe fn dec_ref_cnt(ptr: *mut ChainBufferNode) {
        // SAFETY: the caller holds one reference to a live node.
        let left = unsafe { (*ptr).ref_cnt.fetch_sub(1, Ordering::AcqRel) };
        if left == 1 {
            // SAFETY: `ptr` is uniquely owned now; move its successor handle
            // out so the node's drop (via Box) cannot recurse.
            let next =
                unsafe { std::mem::replace(&mut (*ptr).next, ChainBufferNodeReaderPtr::null()) };
            // SAFETY: `ptr` was allocated by Box and all references are gone.
            unsafe { drop(Box::from_raw(ptr)) };
            unsafe { Self::clear_nonrecursive(next) };
        }
    }
}

/// `td::ChainBufferNode::WriterPtr`.
pub struct ChainBufferNodeWriterPtr(*mut ChainBufferNode);

// SAFETY: writer handles follow the single-writer discipline; the node is
// only freed through `dec_ref_cnt` when no handle remains.
unsafe impl Send for ChainBufferNodeWriterPtr {}

impl ChainBufferNodeWriterPtr {
    fn raw(&self) -> Option<*mut ChainBufferNode> {
        if self.0.is_null() { None } else { Some(self.0) }
    }
}

impl Drop for ChainBufferNodeWriterPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a live node owned by this writer handle.
            unsafe {
                (*self.0).has_writer.store(false, Ordering::Release);
                ChainBufferNode::dec_ref_cnt(self.0);
            }
        }
    }
}

/// `td::ChainBufferNode::ReaderPtr`.
pub struct ChainBufferNodeReaderPtr(*mut ChainBufferNode);

// SAFETY: reader handles only traverse the frozen, refcounted chain.
unsafe impl Send for ChainBufferNodeReaderPtr {}

impl ChainBufferNodeReaderPtr {
    fn null() -> Self {
        Self(ptr::null_mut())
    }

    fn raw(&self) -> Option<*mut ChainBufferNode> {
        if self.0.is_null() { None } else { Some(self.0) }
    }
}

impl Drop for ChainBufferNodeReaderPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a live node held by this reader handle.
            unsafe { ChainBufferNode::dec_ref_cnt(self.0) };
        }
    }
}

/// `td::ChainBufferNodeAllocator::create`.
fn create_chain_node(slice: BufferSlice, sync_flag: bool) -> ChainBufferNodeWriterPtr {
    ChainBufferNode::make_writer_ptr(Box::into_raw(Box::new(ChainBufferNode::new(
        slice, sync_flag,
    ))))
}

/// `td::ChainBufferNodeAllocator::clone`.
fn clone_chain_node(ptr: &ChainBufferNodeReaderPtr) -> ChainBufferNodeReaderPtr {
    match ptr.raw() {
        Some(raw) => ChainBufferNode::make_reader_ptr(raw),
        None => ChainBufferNodeReaderPtr::null(),
    }
}

/// `td::ChainBufferIterator`.
pub struct ChainBufferIterator {
    head: ChainBufferNodeReaderPtr,
    reader: BufferSlice,
    need_sync: bool,
    offset: usize,
}

impl ChainBufferIterator {
    pub fn null() -> Self {
        Self {
            head: ChainBufferNodeReaderPtr::null(),
            reader: BufferSlice::null(),
            need_sync: false,
            offset: 0,
        }
    }

    fn from_head(head: ChainBufferNodeReaderPtr) -> Self {
        let mut this = Self {
            head,
            reader: BufferSlice::null(),
            need_sync: false,
            offset: 0,
        };
        this.load_head();
        this
    }

    fn from_parts(
        head: ChainBufferNodeReaderPtr,
        reader: BufferSlice,
        need_sync: bool,
        offset: usize,
    ) -> Self {
        Self {
            head,
            reader,
            need_sync,
            offset,
        }
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn clear(&mut self) {
        *self = Self::null();
    }

    fn load_head(&mut self) {
        // SAFETY: `head` is live while this iterator holds its reader ptr.
        let node = unsafe { &*self.head.raw().expect("load_head on a null head") };
        self.reader = node.slice.clone();
        self.need_sync = node.sync_flag;
    }

    /// `td::ChainBufferIterator::prepare_read()`.
    pub fn prepare_read(&mut self) -> &[u8] {
        if self.head.raw().is_none() {
            return &[];
        }
        loop {
            // Probe without binding a reference that would have to live for
            // the whole function (its type is fixed by the `return`s below).
            if !self.reader.prepare_read().is_empty() {
                return self.reader.prepare_read();
            }
            // SAFETY: `self.head` holds a live node.
            let has_writer = unsafe { (*self.head.raw().expect("non-null above")).has_writer() };
            if self.need_sync {
                self.reader.sync_with_writer();
                if !self.reader.prepare_read().is_empty() {
                    return self.reader.prepare_read();
                }
            }
            if has_writer {
                return &[];
            }
            let head = self.head.raw().expect("non-null above");
            // SAFETY: the writer is gone, so `next` is frozen and readable;
            // clone the owning handle like `ChainBufferNodeAllocator::clone`.
            let next_handle = clone_chain_node(unsafe { &(*head).next });
            if next_handle.raw().is_none() {
                self.head = ChainBufferNodeReaderPtr::null();
                return &[];
            }
            self.head = next_handle;
            self.load_head();
        }
    }

    /// `td::ChainBufferIterator::read_as_buffer_slice(limit)`: reads only from
    /// the head slice.
    pub fn read_as_buffer_slice(&mut self, limit: usize) -> BufferSlice {
        self.prepare_read();
        let mut res = self.reader.clone();
        res.truncate(limit);
        self.confirm_read(res.size());
        res
    }

    pub fn head(&self) -> &BufferSlice {
        &self.reader
    }

    pub fn confirm_read(&mut self, size: usize) {
        self.offset += size;
        self.reader.confirm_read(size);
    }

    /// `td::ChainBufferIterator::advance_till_end()`.
    pub fn advance_till_end(&mut self) {
        loop {
            let ready = self.prepare_read();
            if ready.is_empty() {
                break;
            }
            let size = ready.len();
            self.confirm_read(size);
        }
    }

    /// `td::ChainBufferIterator::advance(offset, dest)`: skips up to `offset`
    /// bytes, copying them into `dest` when given. Copies through raw
    /// pointers so the read window borrow ends before `confirm_read` mutates
    /// the iterator — the C++ code does the same with plain pointers.
    pub fn advance(&mut self, offset: usize, dest: &mut [u8]) -> usize {
        let mut offset = offset;
        let mut skipped = 0;
        let mut dest = dest;
        while offset != 0 {
            let (ready_ptr, ready_len) = {
                let ready = self.prepare_read();
                if ready.is_empty() {
                    break;
                }
                let len = ready.len().min(offset);
                (ready.as_ptr(), len)
            };
            offset -= ready_len;
            skipped += ready_len;

            let to_dest_size = ready_len.min(dest.len());
            if to_dest_size != 0 {
                // SAFETY: both ranges are valid for `to_dest_size` bytes and
                // disjoint; the raw buffer stays alive through `self`.
                unsafe {
                    ptr::copy_nonoverlapping(ready_ptr, dest.as_mut_ptr(), to_dest_size);
                }
                dest = &mut dest[to_dest_size..];
            }

            self.confirm_read(ready_len);
        }
        skipped
    }
}

/// `td::ChainBufferIterator::clone()`: shares the node chain and the window.
impl Clone for ChainBufferIterator {
    fn clone(&self) -> Self {
        let head = match self.head.raw() {
            Some(_) => clone_chain_node(&self.head),
            None => ChainBufferNodeReaderPtr::null(),
        };
        Self::from_parts(head, self.reader.clone(), self.need_sync, self.offset)
    }
}

/// `td::ChainBufferReader`: a readable window over a node chain.
pub struct ChainBufferReader {
    begin: ChainBufferIterator,
    end: ChainBufferIterator,
    sync_flag: bool,
}

impl ChainBufferReader {
    pub fn null() -> Self {
        Self {
            begin: ChainBufferIterator::null(),
            end: ChainBufferIterator::null(),
            sync_flag: true,
        }
    }

    /// `ChainBufferReader(head)`: reads to the current end of the chain.
    pub fn from_head(head: ChainBufferNodeReaderPtr) -> Self {
        let begin = match head.raw() {
            Some(_) => clone_chain_node(&head),
            None => ChainBufferNodeReaderPtr::null(),
        };
        let mut end = ChainBufferIterator::from_head(head);
        end.advance_till_end();
        Self {
            begin: ChainBufferIterator::from_head(begin),
            end,
            sync_flag: true,
        }
    }

    /// `ChainBufferReader(begin, end, sync_flag)` — used by
    /// `ByteFlowInplaceBase` to window the parent's chain.
    pub fn from_iterators(
        begin: ChainBufferIterator,
        end: ChainBufferIterator,
        sync_flag: bool,
    ) -> Self {
        Self {
            begin,
            end,
            sync_flag,
        }
    }

    /// `ChainBufferReader(head, size)`.
    pub fn from_head_with_size(head: ChainBufferNodeReaderPtr, size: usize) -> Self {
        let begin = match head.raw() {
            Some(_) => clone_chain_node(&head),
            None => ChainBufferNodeReaderPtr::null(),
        };
        let mut end = ChainBufferIterator::from_head(head);
        let advanced = end.advance(size, &mut []);
        assert_eq!(advanced, size, "chain reader shorter than size");
        Self {
            begin: ChainBufferIterator::from_head(begin),
            end,
            sync_flag: true,
        }
    }

    pub fn prepare_read(&mut self) -> &[u8] {
        let size = self.size();
        let res = self.begin.prepare_read();
        &res[..res.len().min(size)]
    }

    pub fn confirm_read(&mut self, size: usize) {
        assert!(size <= self.size(), "confirm_read past end");
        self.begin.confirm_read(size);
    }

    /// `td::ChainBufferReader::advance(offset, dest)`.
    pub fn advance(&mut self, offset: usize, dest: &mut [u8]) -> usize {
        assert!(offset <= self.size(), "advance past end");
        self.begin.advance(offset, dest)
    }

    pub fn size(&self) -> usize {
        self.end.offset() - self.begin.offset()
    }

    pub fn empty(&self) -> bool {
        self.size() == 0
    }

    /// `td::ChainBufferReader::sync_with_writer()`.
    pub fn sync_with_writer(&mut self) {
        if self.sync_flag {
            self.end.advance_till_end();
        }
    }

    /// `td::ChainBufferReader::advance_end(size)`.
    pub fn advance_end(&mut self, size: usize) {
        self.end.advance(size, &mut []);
    }

    pub fn begin(&self) -> &ChainBufferIterator {
        &self.begin
    }

    pub fn end(&self) -> &ChainBufferIterator {
        &self.end
    }

    /// `td::ChainBufferReader::cut_head(pos)`: returns `[begin, pos)`.
    pub fn cut_head(&mut self, pos: ChainBufferIterator) -> ChainBufferReader {
        let tmp = self.begin.clone();
        self.begin = pos.clone();
        ChainBufferReader::from_iterators(tmp, pos, false)
    }

    /// `td::ChainBufferReader::cut_head(offset)`.
    pub fn cut_head_offset(&mut self, offset: usize) -> ChainBufferReader {
        assert!(offset <= self.size(), "cut_head past end");
        let mut it = self.begin.clone();
        it.advance(offset, &mut []);
        self.cut_head(it)
    }

    /// `td::ChainBufferReader::move_as_buffer_slice()`.
    pub fn move_as_buffer_slice(&mut self) -> BufferSlice {
        let size = self.size();
        let res = if self.begin.head().size() >= size {
            self.begin.read_as_buffer_slice(size)
        } else {
            let mut res = BufferSlice::with_size(size);
            let advanced = self.advance(size, res.as_mutable_slice());
            debug_assert_eq!(advanced, size);
            res
        };
        *self = ChainBufferReader::null();
        res
    }

    /// `td::ChainBufferReader::read_as_buffer_slice(limit)`.
    pub fn read_as_buffer_slice(&mut self, limit: usize) -> BufferSlice {
        self.begin.read_as_buffer_slice(limit.min(self.size()))
    }
}

/// `td::ChainBufferReader::clone()`: shares the node chain and window.
impl Clone for ChainBufferReader {
    fn clone(&self) -> Self {
        Self::from_iterators(self.begin.clone(), self.end.clone(), self.sync_flag)
    }
}

/// `td::ChainBufferWriter`: an append-only chain of buffer nodes.
pub struct ChainBufferWriter {
    writer: BufferWriter,
    tail: ChainBufferNodeWriterPtr,
    head: ChainBufferNodeReaderPtr,
}

impl ChainBufferWriter {
    pub fn new() -> Self {
        let writer = BufferWriter::new(0);
        let tail = create_chain_node(writer.as_buffer_slice(), true);
        let head = match tail.raw() {
            Some(raw) => ChainBufferNode::make_reader_ptr(raw),
            None => ChainBufferNodeReaderPtr::null(),
        };
        Self { writer, tail, head }
    }

    pub fn prepare_append(&mut self, hint: usize) -> &mut [u8] {
        assert!(!self.empty(), "prepare_append on an empty writer");
        if !self.prepare_append_inplace().is_empty() {
            return self.prepare_append_inplace();
        }
        self.prepare_append_alloc(hint)
    }

    pub fn prepare_append_at_least(&mut self, size: usize) -> &mut [u8] {
        assert!(!self.empty(), "prepare_append_at_least on an empty writer");
        if self.prepare_append_inplace().len() >= size {
            return self.prepare_append_inplace();
        }
        self.prepare_append_alloc(size)
    }

    pub fn prepare_append_inplace(&mut self) -> &mut [u8] {
        assert!(!self.empty(), "prepare_append_inplace on an empty writer");
        self.writer.prepare_append()
    }

    fn prepare_append_alloc(&mut self, hint: usize) -> &mut [u8] {
        assert!(!self.empty(), "prepare_append_alloc on an empty writer");
        let hint = if hint < (1 << 10) { 1 << 12 } else { hint };
        let new_writer = BufferWriter::new(hint);
        let new_tail = create_chain_node(new_writer.as_buffer_slice(), true);
        // SAFETY: `tail` is writer-owned (no reader created while it is the
        // live tail), so writing `next` is exclusive. The stored handle owns
        // one reference, exactly like `next_ = clone(new_tail)` in C++.
        let new_tail_raw = new_tail.raw().expect("fresh tail is non-null");
        let tail_raw = self.tail.raw().expect("non-empty writer has a tail");
        unsafe { (*tail_raw).next = ChainBufferNode::make_reader_ptr(new_tail_raw) };
        self.writer = new_writer;
        self.tail = new_tail;
        self.writer.prepare_append()
    }

    pub fn confirm_append(&mut self, size: usize) {
        assert!(!self.empty(), "confirm_append on an empty writer");
        self.writer.confirm_append(size);
    }

    /// `td::ChainBufferWriter::append(Slice, hint)`.
    pub fn append(&mut self, mut slice: &[u8], hint: usize) {
        while !slice.is_empty() {
            let ready = self.prepare_append(slice.len().max(hint));
            let shift = ready.len().min(slice.len());
            ready[..shift].copy_from_slice(&slice[..shift]);
            self.confirm_append(shift);
            slice = &slice[shift..];
        }
    }

    /// `td::ChainBufferWriter::append(BufferSlice)`.
    pub fn append_buffer_slice(&mut self, slice: BufferSlice) {
        let ready = self.prepare_append_inplace();
        if slice.size() < (1 << 8) || ready.len() >= slice.size() {
            self.append(slice.as_slice(), 0);
            return;
        }
        let new_tail = create_chain_node(slice, false);
        // SAFETY: `tail` is writer-owned, as in `prepare_append_alloc`; the
        // stored handle owns one reference, like `next_ = clone(new_tail)`.
        let new_tail_raw = new_tail.raw().expect("fresh tail is non-null");
        let tail_raw = self.tail.raw().expect("non-empty writer has a tail");
        unsafe {
            (*tail_raw).next = ChainBufferNode::make_reader_ptr(new_tail_raw);
        }
        self.writer = BufferWriter::null();
        self.tail = new_tail;
    }

    /// `td::ChainBufferWriter::append(ChainBufferReader)`.
    pub fn append_reader(&mut self, mut reader: ChainBufferReader) {
        while !reader.empty() {
            let slice = reader.read_as_buffer_slice(usize::MAX);
            self.append_buffer_slice(slice);
        }
    }

    /// `td::ChainBufferWriter::extract_reader()`.
    pub fn extract_reader(&mut self) -> ChainBufferReader {
        let head = match self.head.raw() {
            Some(_) => clone_chain_node(&self.head),
            None => panic!("extract_reader on an empty writer"),
        };
        ChainBufferReader::from_head(head)
    }

    fn empty(&self) -> bool {
        self.tail.raw().is_none()
    }
}

impl Default for ChainBufferWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::BufferBuilder`: gathers prepends/appends over one writer buffer.
#[derive(Default)]
pub struct BufferBuilder {
    buffer_writer: BufferWriter,
    to_append: Vec<BufferSlice>,
    to_prepend: Vec<BufferSlice>,
}

impl BufferBuilder {
    /// `BufferBuilder(Slice, prepend, append)`.
    pub fn from_slice_with_reserve(slice: &[u8], prepend_size: usize, append_size: usize) -> Self {
        Self {
            buffer_writer: BufferWriter::from_slice_with_reserve(slice, prepend_size, append_size),
            to_append: Vec::new(),
            to_prepend: Vec::new(),
        }
    }

    pub fn append(&mut self, slice: &[u8]) {
        if self.append_inplace(slice) {
            return;
        }
        self.append_slow(BufferSlice::from_slice_bytes(slice));
    }

    pub fn append_buffer_slice(&mut self, slice: BufferSlice) {
        if self.append_inplace(slice.as_slice()) {
            return;
        }
        self.append_slow(slice);
    }

    pub fn prepend(&mut self, slice: &[u8]) {
        if self.prepend_inplace(slice) {
            return;
        }
        self.prepend_slow(BufferSlice::from_slice_bytes(slice));
    }

    pub fn prepend_buffer_slice(&mut self, slice: BufferSlice) {
        if self.prepend_inplace(slice.as_slice()) {
            return;
        }
        self.prepend_slow(slice);
    }

    /// Iterates the concatenated contents in order (prepends first).
    pub fn for_each(&self, mut f: impl FnMut(&[u8])) {
        for slice in self.to_prepend.iter().rev() {
            f(slice.as_slice());
        }
        if !self.buffer_writer.empty() {
            f(self.buffer_writer.as_slice());
        }
        for slice in &self.to_append {
            f(slice.as_slice());
        }
    }

    pub fn size(&self) -> usize {
        let mut total = 0;
        self.for_each(|slice| total += slice.len());
        total
    }

    /// `td::BufferBuilder::extract()`.
    pub fn extract(&mut self) -> BufferSlice {
        if self.to_append.is_empty() && self.to_prepend.is_empty() {
            return self.buffer_writer.as_buffer_slice();
        }
        let total_size = self.size();
        let mut writer = BufferWriter::with_reserve(0, 0, total_size);
        self.for_each(|slice| {
            writer.prepare_append()[..slice.len()].copy_from_slice(slice);
            writer.confirm_append(slice.len());
        });
        *self = Self::default();
        writer.as_buffer_slice()
    }

    fn append_inplace(&mut self, slice: &[u8]) -> bool {
        if !self.to_append.is_empty() {
            return false;
        }
        let dest = self.buffer_writer.prepare_append();
        if dest.len() < slice.len() {
            return false;
        }
        dest[..slice.len()].copy_from_slice(slice);
        self.buffer_writer.confirm_append(slice.len());
        true
    }

    fn append_slow(&mut self, slice: BufferSlice) {
        self.to_append.push(slice);
    }

    fn prepend_inplace(&mut self, slice: &[u8]) -> bool {
        if !self.to_prepend.is_empty() {
            return false;
        }
        let dest = self.buffer_writer.prepare_prepend();
        if dest.len() < slice.len() {
            return false;
        }
        let start = dest.len() - slice.len();
        dest[start..].copy_from_slice(slice);
        self.buffer_writer.confirm_prepend(slice.len());
        true
    }

    fn prepend_slow(&mut self, slice: BufferSlice) {
        self.to_prepend.push(slice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{rand_split_with, rand_string_with};

    /// Deterministic stand-in for `td::Random` (the real port lands in S21).
    fn lcg(seed: u32) -> impl FnMut(i32, i32) -> i32 {
        let mut state = seed;
        move |from, to| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            from + (state as i32).rem_euclid(to - from + 1)
        }
    }

    #[test]
    fn slice_roundtrip_and_windows() {
        let mut slice = BufferSlice::from_slice_bytes(b"hello world");
        assert_eq!(slice.size(), 11);
        assert_eq!(slice.as_slice(), b"hello world");
        assert_eq!(slice.after(6), b"world");
        assert_eq!(slice.prepare_read(), b"hello world");
        assert!(!slice.confirm_read(5));
        assert_eq!(slice.as_slice(), b" world");
        assert!(slice.confirm_read(6));
        assert!(slice.empty());

        let mut slice = BufferSlice::from_slice_bytes(b"0123456789");
        slice.truncate(4);
        assert_eq!(slice.as_slice(), b"0123");
        slice.truncate(100);
        assert_eq!(slice.as_slice(), b"0123");

        let slice = BufferSlice::from_slice_bytes(b"abcdef");
        let window = slice.from_slice(&slice.as_slice()[2..5]);
        assert_eq!(window.as_slice(), b"cde");
        assert_eq!(window.size(), 3);
    }

    #[test]
    fn clone_shares_and_copy_duplicates() {
        let slice = BufferSlice::from_slice_bytes(b"payload");
        let cloned = slice.clone();
        assert_eq!(cloned.as_slice(), b"payload");
        assert_eq!(slice.size(), cloned.size());
        let copied = slice.copy();
        assert_eq!(copied.as_slice(), b"payload");
        assert_eq!(copied.size(), slice.size());
    }

    #[test]
    fn writer_reserve_append_prepend() {
        let mut writer = BufferWriter::with_reserve(3, 2, 4);
        writer.as_mutable_slice().copy_from_slice(b"mid");
        writer.prepare_prepend()[..2].copy_from_slice(b"ab");
        writer.confirm_prepend(2);
        writer.prepare_append()[..4].copy_from_slice(b"tail");
        writer.confirm_append(4);
        assert_eq!(writer.as_slice(), b"abmidtail");
        assert_eq!(writer.size(), 9);
        assert_eq!(writer.as_buffer_slice().as_slice(), b"abmidtail");
    }

    #[test]
    fn writer_slice_tracks_confirmations() {
        let mut writer = BufferWriter::from_slice_with_reserve(b"hello", 0, 100);
        let mut slice = writer.as_buffer_slice();
        assert!(slice.is_writer_alive());
        assert_eq!(slice.as_slice(), b"hello");
        assert_eq!(slice.sync_with_writer(), 0);
        writer.prepare_append()[..6].copy_from_slice(b" world");
        writer.confirm_append(6);
        assert_eq!(slice.sync_with_writer(), 6);
        assert_eq!(slice.as_slice(), b"hello world");
        drop(writer);
        assert!(!slice.is_writer_alive());
    }

    #[test]
    fn small_readers_share_the_slab_and_large_get_own_buffer() {
        let a = BufferSlice::with_size(3);
        let b = BufferSlice::with_size(3);
        a.as_slice();
        b.as_slice();
        let large = BufferSlice::with_size(1 << 20);
        assert_eq!(large.size(), 1 << 20);
        assert!(get_buffer_mem() >= (1 << 20) + TLS_BUFFER_SIZE);
        drop(a);
        drop(b);
        drop(large);
    }

    #[test]
    fn null_slices_are_empty() {
        let slice = BufferSlice::null();
        assert!(slice.is_null());
        assert!(slice.empty());
        assert_eq!(slice.size(), 0);
        assert_eq!(slice.as_slice(), &[] as &[u8]);
        let cloned = slice.clone();
        assert!(cloned.is_null());
    }

    #[test]
    fn chain_writer_reader_roundtrip() {
        let mut writer = ChainBufferWriter::new();
        writer.append(b"one ", 0);
        writer.append(b"two ", 0);
        writer.append(b"three", 0);

        // All appends fit the first node, so one prepare_read sees it all,
        // exactly as the C++ reader would.
        let mut reader = writer.extract_reader();
        assert_eq!(reader.size(), 13);
        assert_eq!(reader.prepare_read(), b"one two three");
        reader.confirm_read(13);
        assert!(reader.empty());
    }

    #[test]
    fn chain_reader_cut_head_and_advance() {
        let mut writer = ChainBufferWriter::new();
        writer.append(b"abcdefghij", 0);
        let mut reader = writer.extract_reader();

        let head = reader.cut_head_offset(3);
        assert_eq!(head.size(), 3);
        let mut dest = [0u8; 3];
        let mut head = head;
        assert_eq!(head.advance(3, &mut dest), 3);
        assert_eq!(&dest, b"abc");
        assert_eq!(reader.size(), 7);
        assert_eq!(reader.prepare_read(), b"defghij");
    }

    #[test]
    fn chain_reader_move_as_buffer_slice() {
        let mut writer = ChainBufferWriter::new();
        writer.append(b"hello world", 0);
        let mut reader = writer.extract_reader();
        let slice = reader.move_as_buffer_slice();
        assert_eq!(slice.as_slice(), b"hello world");
        assert!(reader.empty());
    }

    #[test]
    fn chain_writer_grows_past_one_node() {
        let mut writer = ChainBufferWriter::new();
        let big = vec![b'x'; 10000];
        writer.append(&big, 0);
        writer.append(b"end", 0);
        let mut reader = writer.extract_reader();
        assert_eq!(reader.size(), 10003);
        let mut buf = vec![0u8; reader.size()];
        assert_eq!(reader.advance(reader.size(), &mut buf), 10003);
        assert_eq!(&buf[..3], b"xxx");
        assert_eq!(&buf[10000..], b"end");
    }

    #[test]
    fn chain_reader_syncs_with_writer() {
        let mut writer = ChainBufferWriter::new();
        writer.append(b"first ", 0);
        let mut reader = writer.extract_reader();
        assert_eq!(reader.size(), 6);
        writer.append(b"second", 0);
        assert_eq!(reader.size(), 6, "not synced yet");
        reader.sync_with_writer();
        assert_eq!(reader.size(), 12);
        // The begin window was never advanced, so syncing it yields the whole
        // range; after confirming the first part, the tail becomes visible.
        assert_eq!(reader.prepare_read(), b"first second");
        reader.confirm_read(6);
        assert_eq!(reader.prepare_read(), b"second");
    }

    #[test]
    fn buffer_builder_prepend_append() {
        let mut builder = BufferBuilder::default();
        builder.append(b"b");
        builder.prepend(b"a");
        builder.append(b"c");
        assert_eq!(builder.extract().as_slice(), b"abc");

        let mut builder = BufferBuilder::from_slice_with_reserve(b"hello", 0, 0);
        assert_eq!(builder.extract().as_slice(), b"hello");

        let mut builder = BufferBuilder::from_slice_with_reserve(b"hello", 1, 1);
        builder.prepend(b"A ");
        builder.append(b" B");
        assert_eq!(builder.extract().as_slice(), b"A hello B");
    }

    #[test]
    fn buffer_builder_slow_paths() {
        // `pre`/`post` fit the 3-byte reserves (in-place), then the extra
        // prepend goes to the slow vector while the extra append still fits
        // the append reserve.
        let mut builder = BufferBuilder::from_slice_with_reserve(b"core", 3, 3);
        builder.prepend(b"pre");
        builder.append(b"post");
        builder.prepend_buffer_slice(BufferSlice::from_slice_bytes(b"<"));
        builder.append_buffer_slice(BufferSlice::from_slice_bytes(b">"));
        assert_eq!(builder.size(), 13);
        assert_eq!(builder.extract().as_slice(), b"<precorepost>");
    }

    /// Port of `TEST(Buffer, buffer_builder)` from `tdutils/test/buffer.cpp`,
    /// with the injected-rng random part.
    pub(crate) fn buffer_builder_test(mut rnd: impl FnMut(i32, i32) -> i32) {
        {
            let mut builder = BufferBuilder::default();
            builder.append(b"b");
            builder.prepend(b"a");
            builder.append(b"c");
            assert_eq!(builder.extract().as_slice(), b"abc");
        }
        {
            let mut builder = BufferBuilder::from_slice_with_reserve(b"hello", 0, 0);
            assert_eq!(builder.extract().as_slice(), b"hello");
        }
        {
            let mut builder = BufferBuilder::from_slice_with_reserve(b"hello", 1, 1);
            builder.prepend(b"A ");
            builder.append(b" B");
            assert_eq!(builder.extract().as_slice(), b"A hello B");
        }
        {
            let str = rand_string_with(&mut rnd, b'a' as i32, b'z' as i32, 10000);
            let split_str = rand_split_with(&mut rnd, &str);

            let l = rnd(0, split_str.len() as i32 - 1) as usize;
            let mut r = l;
            let mut l = l;
            let mut builder = BufferBuilder::from_slice_with_reserve(&split_str[l], 123, 1000);
            while l != 0 || r != split_str.len() - 1 {
                if l == 0 || (rnd(0, 1) == 1 && r != split_str.len() - 1) {
                    r += 1;
                    if rnd(0, 1) == 1 {
                        builder.append(&split_str[r]);
                    } else {
                        builder.append_buffer_slice(BufferSlice::from_slice_bytes(&split_str[r]));
                    }
                } else {
                    l -= 1;
                    if rnd(0, 1) == 1 {
                        builder.prepend(&split_str[l]);
                    } else {
                        builder.prepend_buffer_slice(BufferSlice::from_slice_bytes(&split_str[l]));
                    }
                }
            }
            assert_eq!(builder.extract().as_slice(), str);
        }
    }

    crate::td_test!(Buffer, buffer_builder, {
        // The macro body lives in the nested `Buffer::buffer_builder` module.
        crate::buffer::tests::buffer_builder_test(crate::buffer::tests::lcg(0x12345678));
    });

    #[test]
    fn ported_buffer_builder_test_passes() {
        buffer_builder_test(lcg(0x12345678));
        buffer_builder_test(lcg(0xdeadbeef));
    }

    #[test]
    fn buffer_builder_test_is_registered() {
        let runner = crate::tests::default_runner().lock().unwrap();
        assert!(runner.names().contains(&"Test_Buffer_buffer_builder"));
    }
}
