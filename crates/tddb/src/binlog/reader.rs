//! Port of `td/db/binlog/Binlog.cpp` `detail::BinlogReader`: splits a byte
//! stream into validated events, with tdlib's exact error conditions.

use super::event::BinlogEvent;
use tdutils::status::Result;
use tdutils::status::Status;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    ReadLength,
    ReadEvent,
}

/// `td::detail::BinlogReader` over an in-memory stream. `read_next` returns
/// `Ok(None)` on a clean end of stream (including a torn trailing record —
/// the caller truncates at `offset()`, as in C++).
pub struct BinlogReader<'a> {
    input: &'a [u8],
    state: State,
    size: usize,
    offset: i64,
}

impl<'a> BinlogReader<'a> {
    pub fn new(input: &'a [u8]) -> BinlogReader<'a> {
        BinlogReader {
            input,
            state: State::ReadLength,
            size: 0,
            offset: 0,
        }
    }

    /// `BinlogReader::offset` — end offset of the last successfully read event.
    pub fn offset(&self) -> i64 {
        self.offset
    }

    fn remaining(&self) -> usize {
        self.input.len() - self.pos()
    }

    fn pos(&self) -> usize {
        self.offset as usize
    }

    /// `BinlogReader::read_next`
    pub fn read_next(&mut self) -> Result<Option<BinlogEvent>> {
        if self.state == State::ReadLength {
            if self.remaining() < 4 {
                return Result::Ok(None);
            }
            let start = self.offset as usize;
            let size_bytes: [u8; 4] = self.input[start..start + 4].try_into().unwrap();
            let size = u32::from_le_bytes(size_bytes) as usize;

            if size > BinlogEvent::MAX_SIZE {
                return Result::from_status(Status::error_message(&format!(
                    "Too big event size = {size}"
                )));
            }
            if size < BinlogEvent::MIN_SIZE {
                return Result::from_status(Status::error_message(&format!(
                    "Too small event size = {size}"
                )));
            }
            if !size.is_multiple_of(4) {
                return Result::from_status(Status::error(
                    -2,
                    &format!("Event of size {size} at offset {} out of sync", self.offset),
                ));
            }
            self.size = size;
            self.state = State::ReadEvent;
        }

        let start = self.offset as usize;
        if self.input.len() < start + self.size {
            return Result::Ok(None);
        }

        let raw_event = self.input[start..start + self.size].to_vec();
        let mut event = match BinlogEvent::init(raw_event) {
            Result::Ok(event) => event,
            Result::Err(status) => return Result::from_status(status),
        };
        let validated = event.validate();
        if validated.is_error() {
            return Result::from_status(validated);
        }
        self.offset += self.size as i64;
        event.offset = self.offset;
        self.state = State::ReadLength;
        Result::Ok(Some(event))
    }
}
