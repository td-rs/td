//! Port of `td/utils/ByteFlow.h` + the `GzipByteFlow` of
//! `td/utils/GzipByteFlow.cpp`: the pull-based streaming pipeline.
//!
//! Nodes form a chain `source >> a >> b >> sink`; each middle node owns an
//! output writer and hands its parent a shared reader clone, so no
//! self-referential borrows are needed. Parents are kept as raw pointers
//! (the C++ design); a pipeline is single-threaded and nodes must outlive
//! their parents' use of them.

use crate::buffer::{ChainBufferReader, ChainBufferWriter};
use crate::gzip::{Gzip, GzipMode, GzipState};
use crate::status::{Result, Status};

/// `td::ByteFlowInterface`.
pub trait ByteFlowInterface {
    fn close_input(&mut self, status: Status);
    fn wakeup(&mut self);
    fn set_parent(&mut self, parent: &mut dyn ByteFlowInterface);
    fn set_input(&mut self, input: ChainBufferReader);
    fn get_need_size(&mut self) -> usize;
    fn get_read_size(&mut self) -> usize;
    fn get_write_size(&mut self) -> usize;
    fn reset_need_size(&mut self) {}
}

/// Chains `from`'s output into `to`'s input (`td::operator>>`).
pub fn connect(from: &mut dyn ByteFlowInterface, to: &mut dyn ByteFlowInterface) {
    from.set_parent(to);
}

/// A null parent pointer. Only the data pointer is ever inspected
/// (`is_null` checks); the vtable of a null parent is never dereferenced.
fn null_flow_parent() -> *mut dyn ByteFlowInterface {
    // SAFETY: transmute of a null thin-pointer pair; the metadata is never
    // used for a null parent.
    unsafe {
        std::mem::transmute::<(*mut (), *mut ()), *mut dyn ByteFlowInterface>((
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ))
    }
}

/// `td::ByteFlowBaseCommon::Watermark`.
#[derive(Clone, Copy, Debug)]
pub struct Watermark {
    pub low: usize,
    pub high: usize,
}

impl Default for Watermark {
    fn default() -> Self {
        Self {
            low: usize::MAX,
            high: 0,
        }
    }
}

/// `td::ByteFlowBaseCommon::Options`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub write_watermark: Watermark,
    pub read_watermark: Watermark,
}

/// `td::ByteFlowBaseCommon` — the watermark/backpressure machinery shared by
/// the regular and in-place flow bases. Subclasses drive it through
/// [`ByteFlowBaseCommon::wakeup_with`] with their processing step.
pub struct ByteFlowBaseCommon<'a> {
    parent: *mut (dyn ByteFlowInterface + 'a),
    input: Option<ChainBufferReader>,
    is_input_active: bool,
    need_size: usize,
    can_read: bool,
    can_write: bool,
    options: Options,
    waiting_flag: bool,
    stop_flag: bool,
}

impl<'a> Default for ByteFlowBaseCommon<'a> {
    fn default() -> Self {
        Self {
            parent: null_flow_parent(),
            input: None,
            is_input_active: true,
            need_size: 0,
            can_read: true,
            can_write: true,
            options: Options::default(),
            waiting_flag: false,
            stop_flag: false,
        }
    }
}

impl<'a> ByteFlowBaseCommon<'a> {
    /// `td::ByteFlowBaseCommon::close_input`.
    pub fn close_input(&mut self, status: Status) {
        if status.is_error() {
            self.finish(status);
        } else {
            self.is_input_active = false;
            let mut noop = || false;
            self.wakeup_with(&mut noop);
        }
    }

    /// `td::ByteFlowBaseCommon::wakeup` — the shared loop; `loop_step` plays
    /// the role of the C++ virtual `loop()`.
    pub fn wakeup_with(&mut self, loop_step: &mut dyn FnMut() -> bool) {
        if self.stop_flag || self.input.is_none() {
            return;
        }
        self.input
            .as_mut()
            .expect("checked above")
            .sync_with_writer();

        if self.waiting_flag {
            if !self.is_input_active {
                self.finish(Status::ok());
            }
            return;
        }
        loop {
            if self.stop_flag {
                break;
            }

            // update can_read
            if self.is_input_active {
                let read_size = self.get_read_size_impl();
                if read_size < self.need_size.min(self.options.read_watermark.low) {
                    self.can_read = false;
                }
                if read_size >= self.need_size.max(self.options.read_watermark.high) {
                    self.can_read = true;
                }
            } else {
                // always can read when input is closed
                self.can_read = true;
            }

            // update can_write
            let write_size = self.get_write_size_impl();
            if write_size > self.options.write_watermark.high {
                self.can_write = false;
            }
            if write_size <= self.options.write_watermark.low {
                self.can_write = true;
            }

            if !self.can_read || !self.can_write {
                break;
            }
            self.need_size = 0;

            if !loop_step() && self.need_size <= self.get_read_size_impl() {
                self.need_size = self.get_read_size_impl() + 1;
            }
        }
        self.on_output_updated();
    }

    pub fn get_need_size_impl(&self) -> usize {
        self.need_size
    }

    pub fn reset_need_size_impl(&mut self) {
        self.need_size = 0;
    }

    pub fn get_read_size_impl(&mut self) -> usize {
        let input = self.input.as_mut().expect("get_read_size without input");
        input.sync_with_writer();
        input.size()
    }

    pub fn get_write_size_impl(&mut self) -> usize {
        // SAFETY: `parent` is set by `set_parent` before the flow runs and
        // stays live until the pipeline is dismantled.
        let parent = unsafe { self.parent.as_mut().expect("get_write_size without parent") };
        parent.get_read_size()
    }

    pub fn set_options(&mut self, options: Options) {
        self.options = options;
    }

    /// `td::ByteFlowBaseCommon::finish`.
    pub fn finish(&mut self, status: Status) {
        self.stop_flag = true;
        self.need_size = 0;
        if !self.parent.is_null() {
            // SAFETY: the parent outlives this node's active use.
            let parent = unsafe { self.parent.as_mut().expect("non-null parent") };
            self.parent = null_flow_parent();
            parent.close_input(status);
        }
    }

    pub fn set_need_size(&mut self, need_size: usize) {
        self.need_size = need_size;
    }

    /// `td::ByteFlowBaseCommon::on_output_updated`.
    pub fn on_output_updated(&mut self) {
        if !self.parent.is_null() {
            // SAFETY: the parent outlives this node's active use.
            unsafe { self.parent.as_mut().expect("non-null parent") }.wakeup();
        }
    }

    /// `td::ByteFlowBaseCommon::consume_input`.
    pub fn consume_input(&mut self) {
        self.waiting_flag = true;
        if !self.is_input_active {
            self.finish(Status::ok());
        }
    }
}

/// `td::ByteFlowBase` — a flow node with its own output writer.
pub struct ByteFlowBase<'a> {
    pub common: ByteFlowBaseCommon<'a>,
    pub output: ChainBufferWriter,
    output_reader: ChainBufferReader,
}

impl<'a> Default for ByteFlowBase<'a> {
    fn default() -> Self {
        let mut output = ChainBufferWriter::new();
        let output_reader = output.extract_reader();
        Self {
            common: ByteFlowBaseCommon::default(),
            output,
            output_reader,
        }
    }
}

impl<'a> ByteFlowBase<'a> {
    pub fn set_input(&mut self, input: ChainBufferReader) {
        self.common.input = Some(input);
    }

    pub fn set_parent(&mut self, parent: &mut (dyn ByteFlowInterface + 'a)) {
        self.common.parent = parent as *mut (dyn ByteFlowInterface + 'a);
        parent.set_input(self.output_reader.clone());
    }
}

/// `td::ByteFlowInplaceBase` — a flow node that rewrites its input in place.
pub struct ByteFlowInplaceBase<'a> {
    pub common: ByteFlowBaseCommon<'a>,
    pub output: ChainBufferReader,
}

impl<'a> Default for ByteFlowInplaceBase<'a> {
    fn default() -> Self {
        Self {
            common: ByteFlowBaseCommon::default(),
            output: ChainBufferReader::null(),
        }
    }
}

impl<'a> ByteFlowInplaceBase<'a> {
    pub fn set_input(&mut self, input: ChainBufferReader) {
        let begin = input.begin().clone();
        let end = input.begin().clone();
        self.output = ChainBufferReader::from_iterators(begin, end, false);
        self.common.input = Some(input);
    }

    pub fn set_parent(&mut self, parent: &mut (dyn ByteFlowInterface + 'a)) {
        self.common.parent = parent as *mut (dyn ByteFlowInterface + 'a);
        parent.set_input(self.output.clone());
    }
}

/// `td::ByteFlowSource` — the pipeline head, fed by an external writer.
pub struct ByteFlowSource<'a> {
    buffer: Option<ChainBufferReader>,
    parent: *mut (dyn ByteFlowInterface + 'a),
}

impl<'a> Default for ByteFlowSource<'a> {
    fn default() -> Self {
        Self {
            buffer: None,
            parent: null_flow_parent(),
        }
    }
}

impl<'a> ByteFlowSource<'a> {
    /// `td::ByteFlowSource(buffer)`.
    pub fn new(buffer: &ChainBufferReader) -> Self {
        Self {
            buffer: Some(buffer.clone()),
            parent: null_flow_parent(),
        }
    }

    pub fn set_parent(&mut self, parent: &mut (dyn ByteFlowInterface + 'a)) {
        assert!(self.parent.is_null(), "source already connected");
        self.parent = parent as *mut (dyn ByteFlowInterface + 'a);
        parent.set_input(
            self.buffer
                .as_ref()
                .expect("source without a buffer")
                .clone(),
        );
    }

    pub fn close_input(&mut self, status: Status) {
        // SAFETY: the parent outlives this node's active use.
        let parent = unsafe { self.parent.as_mut().expect("source without a parent") };
        self.parent = null_flow_parent();
        parent.close_input(status);
    }

    pub fn wakeup(&mut self) {
        if self.parent.is_null() {
            return;
        }
        // SAFETY: the parent outlives this node's active use.
        unsafe { self.parent.as_mut().expect("non-null parent") }.wakeup();
    }

    pub fn get_need_size(&mut self) -> usize {
        if self.parent.is_null() {
            return 0;
        }
        // SAFETY: the parent outlives this node's active use.
        unsafe { self.parent.as_mut().expect("non-null parent") }.get_need_size()
    }

    pub fn get_read_size(&mut self) -> usize {
        panic!("get_read_size on a ByteFlowSource")
    }

    pub fn get_write_size(&mut self) -> usize {
        // SAFETY: the parent outlives this node's active use.
        unsafe { self.parent.as_mut().expect("source without a parent") }.get_read_size()
    }
}

/// `td::ByteFlowSink` — the pipeline tail.
pub struct ByteFlowSink {
    buffer: Option<ChainBufferReader>,
    active: bool,
    status: Status,
}

impl Default for ByteFlowSink {
    fn default() -> Self {
        Self {
            buffer: None,
            active: true,
            status: Status::ok(),
        }
    }
}

impl ByteFlowSink {
    pub fn set_input(&mut self, input: ChainBufferReader) {
        assert!(self.buffer.is_none(), "sink already has an input");
        self.buffer = Some(input);
    }

    pub fn close_input(&mut self, status: Status) {
        assert!(self.active, "close_input on a finished sink");
        self.active = false;
        self.status = status;
        self.buffer
            .as_mut()
            .expect("sink input set before close")
            .sync_with_writer();
    }

    pub fn wakeup(&mut self) {
        self.buffer
            .as_mut()
            .expect("sink input set before wakeup")
            .sync_with_writer();
    }

    pub fn get_read_size(&mut self) -> usize {
        let buffer = self.buffer.as_mut().expect("sink input set");
        buffer.sync_with_writer();
        buffer.size()
    }

    pub fn is_ready(&self) -> bool {
        !self.active
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// `td::ByteFlowSink::result`.
    pub fn result(&mut self) -> &mut ChainBufferReader {
        assert!(self.is_ready() && self.status.is_ok());
        self.buffer.as_mut().expect("sink input set")
    }

    /// `td::ByteFlowSink::get_output`.
    pub fn get_output(&mut self) -> &mut ChainBufferReader {
        self.buffer.as_mut().expect("sink input set")
    }
}

/// `td::ByteFlowMoveSink` — the tail that moves input into an external
/// writer.
pub struct ByteFlowMoveSink<'a> {
    input: Option<ChainBufferReader>,
    output: Option<&'a mut ChainBufferWriter>,
    active: bool,
    status: Status,
}

impl<'a> ByteFlowMoveSink<'a> {
    pub fn new() -> Self {
        Self {
            input: None,
            output: None,
            active: true,
            status: Status::ok(),
        }
    }

    /// `td::ByteFlowMoveSink(output)`.
    pub fn with_output(output: &'a mut ChainBufferWriter) -> Self {
        let mut sink = Self::new();
        sink.set_output(output);
        sink
    }

    pub fn set_input(&mut self, input: ChainBufferReader) {
        assert!(self.input.is_none(), "move sink already has an input");
        self.input = Some(input);
    }

    pub fn close_input(&mut self, status: Status) {
        assert!(self.active, "close_input on a finished sink");
        self.active = false;
        self.status = status;
        self.wakeup();
    }

    pub fn wakeup(&mut self) {
        let input = self.input.as_mut().expect("move sink input set");
        input.sync_with_writer();
        let size = input.size();
        // Consume the appended part so the next wakeup continues where this
        // one stopped (the C++ `append(*input_)` advances the reader too).
        let head = input.cut_head_offset(size);
        let output = self.output.as_mut().expect("move sink output set");
        output.append_reader(head);
    }

    pub fn get_read_size(&mut self) -> usize {
        let input = self.input.as_mut().expect("move sink input set");
        input.sync_with_writer();
        input.size()
    }

    pub fn set_output(&mut self, output: &'a mut ChainBufferWriter) {
        assert!(self.output.is_none(), "move sink already has an output");
        self.output = Some(output);
    }

    pub fn is_ready(&self) -> bool {
        !self.active
    }

    pub fn status(&self) -> &Status {
        &self.status
    }
}

impl Default for ByteFlowMoveSink<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// `td::GzipByteFlow` — a gzip codec as a pipeline node.
pub struct GzipByteFlow<'a> {
    base: ByteFlowBase<'a>,
    gzip: Gzip,
    total_output_size: usize,
    max_output_size: usize,
}

impl<'a> GzipByteFlow<'a> {
    pub fn new() -> Self {
        Self {
            base: ByteFlowBase::default(),
            gzip: Gzip::new(),
            total_output_size: 0,
            max_output_size: usize::MAX,
        }
    }

    /// `td::GzipByteFlow(mode)`.
    pub fn with_mode(mode: GzipMode) -> Self {
        let mut flow = Self::new();
        flow.gzip.init(mode).ensure();
        flow
    }

    pub fn init_decode(&mut self) {
        self.gzip.init_decode().ensure();
    }

    pub fn init_encode(&mut self) {
        self.gzip.init_encode().ensure();
    }

    /// `td::GzipByteFlow::set_max_output_size`.
    pub fn set_max_output_size(&mut self, max_output_size: usize) {
        self.max_output_size = max_output_size;
    }

    pub fn set_options(&mut self, options: Options) {
        self.base.common.set_options(options);
    }

    /// `td::GzipByteFlow::loop` — one pull step, over disjoint field
    /// borrows so the wakeup loop can call it.
    fn step(
        gzip: &mut Gzip,
        output: &mut ChainBufferWriter,
        common: &mut ByteFlowBaseCommon<'_>,
        total_output_size: &mut usize,
        max_output_size: usize,
    ) -> bool {
        if gzip.need_input() {
            let slice = common
                .input
                .as_mut()
                .expect("gzip flow input")
                .prepare_read();
            if slice.is_empty() {
                if !common.is_input_active {
                    gzip.close_input();
                } else {
                    return false;
                }
            } else {
                // The reader window is stable until confirm_read below.
                let slice = common
                    .input
                    .as_mut()
                    .expect("gzip flow input")
                    .prepare_read();
                gzip.set_input(slice);
            }
        }
        if gzip.need_output() {
            let slice = output.prepare_append(0);
            assert!(!slice.is_empty(), "flow output writer exhausted");
            gzip.set_output(slice);
        }
        let state = gzip.run();
        let output_size = gzip.flush_output();
        if output_size != 0 {
            if output_size > max_output_size || *total_output_size > max_output_size - output_size {
                common.finish(Status::error_message("Max output size limit exceeded"));
                return false;
            }
            *total_output_size += output_size;
            output.confirm_append(output_size);
        }

        let input_size = gzip.flush_input();
        if input_size != 0 {
            common
                .input
                .as_mut()
                .expect("gzip flow input")
                .confirm_read(input_size);
        }
        match state {
            Result::Err(status) => {
                common.finish(status);
                false
            }
            Result::Ok(GzipState::Done) => {
                common.consume_input();
                false
            }
            Result::Ok(GzipState::Running) => true,
        }
    }
}

impl<'a> Default for GzipByteFlow<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> ByteFlowInterface for GzipByteFlow<'a> {
    fn close_input(&mut self, status: Status) {
        self.base.common.close_input(status);
    }

    fn wakeup(&mut self) {
        let gzip = &mut self.gzip;
        let output = &mut self.base.output;
        let total = &mut self.total_output_size;
        let max_output_size = self.max_output_size;
        // The step closure and the wakeup loop borrow disjoint fields of
        // `self`; the common part is re-derived from a raw pointer so the
        // closure can capture it while `wakeup_with` mutably borrows it.
        let common_ptr = &mut self.base.common as *mut ByteFlowBaseCommon<'a>;
        // SAFETY: `common_ptr` is valid for the whole wakeup call, and the
        // closure is only invoked from within it.
        let mut step =
            move || unsafe { Self::step(gzip, output, &mut *common_ptr, total, max_output_size) };
        unsafe { &mut *common_ptr }.wakeup_with(&mut step);
    }

    fn set_parent(&mut self, parent: &mut dyn ByteFlowInterface) {
        // SAFETY: the pipeline contract requires every parent to outlive the
        // connection it is set on (the C++ raw-pointer discipline); the
        // transmute only extends the borrow to the node's own lifetime.
        let parent: &mut (dyn ByteFlowInterface + 'a) = unsafe { std::mem::transmute(parent) };
        self.base.set_parent(parent);
    }

    fn set_input(&mut self, input: ChainBufferReader) {
        self.base.set_input(input);
    }

    fn get_need_size(&mut self) -> usize {
        self.base.common.get_need_size_impl()
    }

    fn get_read_size(&mut self) -> usize {
        self.base.common.get_read_size_impl()
    }

    fn get_write_size(&mut self) -> usize {
        self.base.common.get_write_size_impl()
    }

    fn reset_need_size(&mut self) {
        self.base.common.reset_need_size_impl();
    }
}

impl<'a> ByteFlowInterface for ByteFlowSource<'a> {
    fn close_input(&mut self, status: Status) {
        ByteFlowSource::close_input(self, status);
    }

    fn wakeup(&mut self) {
        ByteFlowSource::wakeup(self);
    }

    fn set_parent(&mut self, parent: &mut dyn ByteFlowInterface) {
        // SAFETY: the pipeline contract requires the parent to outlive the
        // connection (the C++ raw-pointer discipline); the transmute only
        // extends the borrow to this node's own lifetime.
        let parent: &mut (dyn ByteFlowInterface + 'a) = unsafe { std::mem::transmute(parent) };
        ByteFlowSource::set_parent(self, parent);
    }

    fn set_input(&mut self, _input: ChainBufferReader) {
        panic!("set_input on a ByteFlowSource");
    }

    fn get_need_size(&mut self) -> usize {
        ByteFlowSource::get_need_size(self)
    }

    fn get_read_size(&mut self) -> usize {
        ByteFlowSource::get_read_size(self)
    }

    fn get_write_size(&mut self) -> usize {
        ByteFlowSource::get_write_size(self)
    }
}

impl ByteFlowInterface for ByteFlowSink {
    fn close_input(&mut self, status: Status) {
        ByteFlowSink::close_input(self, status);
    }

    fn wakeup(&mut self) {
        ByteFlowSink::wakeup(self);
    }

    fn set_parent(&mut self, _parent: &mut dyn ByteFlowInterface) {
        panic!("set_parent on a ByteFlowSink");
    }

    fn set_input(&mut self, input: ChainBufferReader) {
        ByteFlowSink::set_input(self, input);
    }

    fn get_need_size(&mut self) -> usize {
        panic!("get_need_size on a ByteFlowSink")
    }

    fn get_read_size(&mut self) -> usize {
        ByteFlowSink::get_read_size(self)
    }

    fn get_write_size(&mut self) -> usize {
        panic!("get_write_size on a ByteFlowSink")
    }
}

impl<'a> ByteFlowInterface for ByteFlowMoveSink<'a> {
    fn close_input(&mut self, status: Status) {
        ByteFlowMoveSink::close_input(self, status);
    }

    fn wakeup(&mut self) {
        ByteFlowMoveSink::wakeup(self);
    }

    fn set_parent(&mut self, _parent: &mut dyn ByteFlowInterface) {
        panic!("set_parent on a ByteFlowMoveSink");
    }

    fn set_input(&mut self, input: ChainBufferReader) {
        ByteFlowMoveSink::set_input(self, input);
    }

    fn get_need_size(&mut self) -> usize {
        panic!("get_need_size on a ByteFlowMoveSink")
    }

    fn get_read_size(&mut self) -> usize {
        ByteFlowMoveSink::get_read_size(self)
    }

    fn get_write_size(&mut self) -> usize {
        panic!("get_write_size on a ByteFlowMoveSink")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gzip::gzencode;
    use crate::random::{rand_split, rand_string};

    fn run_pipeline(
        parts: &[Vec<u8>],
        flows: &mut [&mut dyn ByteFlowInterface],
        source: &mut ByteFlowSource,
        sink: &mut ByteFlowSink,
        input_writer: &mut ChainBufferWriter,
    ) {
        let mut current: &mut (dyn ByteFlowInterface + '_) = source;
        for flow in flows.iter_mut() {
            current.set_parent(*flow);
            current = *flow;
        }
        current.set_parent(sink);

        assert!(!sink.is_ready());
        for part in parts {
            input_writer.append(part, 0);
            source.wakeup();
        }
        assert!(!sink.is_ready());
        source.close_input(Status::ok());
        assert!(sink.is_ready());
    }

    #[test]
    fn flow_encode_matches_one_shot() {
        let str = rand_string(b'a' as i32, b'z' as i32, 1_000_000);
        let parts = rand_split(&str);

        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let mut gzip_flow = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut sink = ByteFlowSink::default();

        run_pipeline(
            &parts,
            &mut [&mut gzip_flow],
            &mut source,
            &mut sink,
            &mut input_writer,
        );

        assert!(sink.status().is_ok(), "{}", sink.status());
        let res = sink.result().move_as_buffer_slice().as_slice().to_vec();
        assert!(!res.is_empty());
        assert_eq!(gzencode(&str, 2.0).as_slice(), res);
    }

    #[test]
    fn flow_error_on_truncated_stream() {
        let str = rand_string(b'a' as i32, b'z' as i32, 1_000_000);
        let mut zip = gzencode(&str, 0.9).as_slice().to_vec();
        assert!(!zip.is_empty());
        zip.pop();
        let parts = rand_split(&zip);

        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let mut gzip_flow = GzipByteFlow::with_mode(GzipMode::Decode);
        let mut sink = ByteFlowSink::default();

        run_pipeline(
            &parts,
            &mut [&mut gzip_flow],
            &mut source,
            &mut sink,
            &mut input_writer,
        );

        assert!(sink.status().is_error());
    }

    #[test]
    fn encode_decode_flow_roundtrip() {
        let str = rand_string(b'a' as i32, b'z' as i32, 1_000_000);
        let parts = rand_split(&str);

        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let mut encode = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut decode = GzipByteFlow::with_mode(GzipMode::Decode);
        let mut encode2 = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut decode2 = GzipByteFlow::with_mode(GzipMode::Decode);
        let mut sink = ByteFlowSink::default();

        run_pipeline(
            &parts,
            &mut [&mut encode, &mut decode, &mut encode2, &mut decode2],
            &mut source,
            &mut sink,
            &mut input_writer,
        );

        assert!(sink.status().is_ok(), "{}", sink.status());
        assert_eq!(str, sink.result().move_as_buffer_slice().as_slice());
    }

    #[test]
    fn bomb_flows_through_with_watermarks() {
        // Build a compressed bomb: 200 × 64 KiB of 'a'.
        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let mut encode = GzipByteFlow::with_mode(GzipMode::Encode);
        let mut sink = ByteFlowSink::default();
        source.set_parent(&mut encode);
        encode.set_parent(&mut sink);
        let s = vec![b'a'; 1 << 16];
        for _ in 0..200 {
            input_writer.append(&s, 0);
            source.wakeup();
        }
        source.close_input(Status::ok());
        assert!(sink.is_ready());
        assert!(sink.status().is_ok(), "{}", sink.status());
        let bomb = sink.result().move_as_buffer_slice().as_slice().to_vec();
        assert_eq!(bomb.len(), 200 * (1 << 16));

        // Decode the bomb through a watermarked pipeline.
        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let decode_options = Options {
            write_watermark: Watermark {
                low: 2 << 20,
                high: 4 << 20,
            },
            read_watermark: Watermark::default(),
        };
        let encode_options = Options {
            write_watermark: Watermark::default(),
            read_watermark: Watermark {
                low: 2 << 20,
                high: 4 << 20,
            },
        };
        let mut decode = GzipByteFlow::with_mode(GzipMode::Decode);
        decode.set_options(decode_options);
        let mut encode2 = GzipByteFlow::with_mode(GzipMode::Encode);
        encode2.set_options(encode_options);
        let mut decode2 = GzipByteFlow::with_mode(GzipMode::Decode);
        decode2.set_options(decode_options);
        let mut encode3 = GzipByteFlow::with_mode(GzipMode::Encode);
        encode3.set_options(encode_options);
        let mut decode3 = GzipByteFlow::with_mode(GzipMode::Decode);
        decode3.set_options(decode_options);
        let mut sink = ByteFlowSink::default();

        source.set_parent(&mut decode);
        decode.set_parent(&mut encode2);
        encode2.set_parent(&mut decode2);
        decode2.set_parent(&mut encode3);
        encode3.set_parent(&mut decode3);
        decode3.set_parent(&mut sink);

        assert!(!sink.is_ready());
        let mut left_size = bomb.len();
        input_writer.append(&bomb, 0);
        source.close_input(Status::ok());

        loop {
            decode3.wakeup();
            decode2.wakeup();
            decode.wakeup();
            source.wakeup();
            let size = sink.get_output().size();
            let mut head = sink.get_output().cut_head_offset(size);
            let chunk = head.move_as_buffer_slice();
            assert!(chunk.size() <= left_size);
            assert!(chunk.as_slice().iter().all(|&c| c == b'a'));
            left_size -= chunk.size();
            if sink.is_ready() {
                break;
            }
        }
        assert_eq!(left_size, 0);
        assert!(sink.status().is_ok());
    }

    #[test]
    fn move_sink_and_inplace_base_smoke() {
        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut source = ByteFlowSource::new(&input);
        let mut output_writer = ChainBufferWriter::new();
        let mut sink = ByteFlowMoveSink::with_output(&mut output_writer);

        source.set_parent(&mut sink);
        input_writer.append(b"move sink data", 0);
        source.wakeup();
        source.close_input(Status::ok());
        assert!(sink.is_ready());
        assert!(sink.status().is_ok());
        let mut reader = output_writer.extract_reader();
        assert_eq!(reader.size(), 15);
        let mut buf = vec![0u8; 15];
        assert_eq!(reader.advance(15, &mut buf), 15);
        assert_eq!(buf, b"move sink data");

        // Inplace base just wraps its input window.
        let mut input_writer = ChainBufferWriter::new();
        let input = input_writer.extract_reader();
        let mut inplace = ByteFlowInplaceBase::default();
        inplace.set_input(input);
        input_writer.append(b"inplace", 0);
        assert_eq!(inplace.common.get_read_size_impl(), 7);
        assert_eq!(inplace.output.size(), 0, "window starts empty");
    }
}
