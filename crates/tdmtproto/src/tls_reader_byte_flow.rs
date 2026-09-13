//! Port of `td/mtproto/TlsReaderByteFlow.{h,cpp}`: strips the fake-TLS
//! record headers from the incoming stream of an emulated-TLS MTProxy
//! connection. Only the application-data records (`\x17\x03\x03 <len>`) are
//! passed through.

use tdutils::buffer::ChainBufferReader;
use tdutils::byte_flow::{ByteFlowBase, ByteFlowBaseCommon, ByteFlowInterface};
use tdutils::status::Status;

/// `td::mtproto::TlsReaderByteFlow`.
pub struct TlsReaderByteFlow<'a> {
    base: ByteFlowBase<'a>,
}

impl<'a> TlsReaderByteFlow<'a> {
    pub fn new() -> Self {
        Self {
            base: ByteFlowBase::default(),
        }
    }

    /// `TlsReaderByteFlow::loop`.
    fn step(
        common: &mut ByteFlowBaseCommon<'_>,
        output: &mut tdutils::buffer::ChainBufferWriter,
    ) -> bool {
        let input_size = common.input().expect("tls reader input").size();
        if input_size < 5 {
            common.set_need_size(5);
            return false;
        }

        let mut it = common.input().expect("tls reader input").clone();
        let mut buf = [0u8; 5];
        it.advance(5, &mut buf);
        if &buf[..3] != b"\x17\x03\x03" {
            common.close_input(Status::error_message(
                "Invalid bytes at the beginning of a packet (emulated tls)",
            ));
            return false;
        }
        let len = ((buf[3] as usize) << 8) | buf[4] as usize;
        if it.size() < len {
            common.set_need_size(5 + len);
            return false;
        }

        let head = it.cut_head_offset(len);
        output.append_reader(head);
        let input = common.input_mut().expect("tls reader input");
        *input = it;
        true
    }
}

impl<'a> Default for TlsReaderByteFlow<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> ByteFlowInterface for TlsReaderByteFlow<'a> {
    fn close_input(&mut self, status: Status) {
        self.base.common.close_input(status);
        // The C++ close runs the full wakeup over the virtual loop().
        self.wakeup();
    }

    fn wakeup(&mut self) {
        let output = &mut self.base.output;
        let common_ptr = &mut self.base.common as *mut ByteFlowBaseCommon<'a>;
        let mut step = move || unsafe { Self::step(&mut *common_ptr, output) };
        unsafe { &mut *common_ptr }.wakeup_with(&mut step);
    }

    fn set_parent(&mut self, parent: &mut dyn ByteFlowInterface) {
        // SAFETY: the pipeline contract requires every parent to outlive the
        // connection it is set on (the C++ raw-pointer discipline).
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
