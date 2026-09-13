//! Port of `td/db/binlog/BinlogEvent`: one binlog record. The wire layout is
//! byte-identical with C++ tdlib (little-endian fixed header + data + CRC32).

use tdutils::status::Result;
use tdutils::status::Status;

/// `td::BinlogEvent`
#[derive(Clone, Debug)]
pub struct BinlogEvent {
    pub offset: i64,

    pub size: u32,
    pub id: u64,
    /// type can be merged with flags
    pub type_: i32,
    pub flags: i32,
    pub extra: u64,
    crc32: u32,

    raw_event: Vec<u8>,
}

impl BinlogEvent {
    pub const MAX_SIZE: usize = 1 << 24;
    pub const HEADER_SIZE: usize = 4 + 8 + 4 + 4 + 8;
    pub const TAIL_SIZE: usize = 4;
    pub const MIN_SIZE: usize = Self::HEADER_SIZE + Self::TAIL_SIZE;

    /// `BinlogEvent::ServiceTypes`
    pub const TYPE_HEADER: i32 = -1;
    pub const TYPE_EMPTY: i32 = -2;
    pub const TYPE_AES_CTR_ENCRYPTION: i32 = -3;
    pub const TYPE_NO_ENCRYPTION: i32 = -4;

    /// `BinlogEvent::Flags`
    pub const FLAG_REWRITE: i32 = 1;
    pub const FLAG_PARTIAL: i32 = 2;

    /// `BinlogEvent::init` — parse a raw record; validates structural
    /// invariants (C++ CHECKs) as errors.
    pub fn init(raw_event: Vec<u8>) -> Result<BinlogEvent> {
        if raw_event.len() < Self::MIN_SIZE {
            return Result::from_status(Status::error_message("Too small event"));
        }
        let size = u32::from_le_bytes(raw_event[0..4].try_into().unwrap()) as usize;
        if size != raw_event.len() {
            return Result::from_status(Status::error_message(&format!(
                "Size of event changed: now {size} real size {}",
                raw_event.len()
            )));
        }
        if size < Self::MIN_SIZE {
            return Result::from_status(Status::error_message("Too small event"));
        }
        let id = u64::from_le_bytes(raw_event[4..12].try_into().unwrap());
        let type_ = i32::from_le_bytes(raw_event[12..16].try_into().unwrap());
        let flags = i32::from_le_bytes(raw_event[16..20].try_into().unwrap());
        let extra = u64::from_le_bytes(raw_event[20..28].try_into().unwrap());
        let crc = u32::from_le_bytes(raw_event[size - 4..size].try_into().unwrap());
        Result::Ok(BinlogEvent {
            offset: -1,
            size: size as u32,
            id,
            type_,
            flags,
            extra,
            crc32: crc,
            raw_event,
        })
    }

    /// `BinlogEvent::get_data`
    pub fn get_data(&self) -> &[u8] {
        assert!(self.raw_event.len() >= Self::MIN_SIZE);
        &self.raw_event[Self::HEADER_SIZE..self.raw_event.len() - Self::TAIL_SIZE]
    }

    pub fn is_empty(&self) -> bool {
        self.raw_event.is_empty()
    }

    /// Size of the raw record (C++ accesses `raw_event_` directly within the
    /// crate; this is the pub(crate) equivalent).
    pub(crate) fn raw_size(&self) -> usize {
        self.raw_event.len()
    }

    pub(crate) fn raw_event(&self) -> &[u8] {
        &self.raw_event
    }

    /// `BinlogEvent::validate` — full record check: size consistency and
    /// CRC32 over everything before the tail.
    pub fn validate(&self) -> Status {
        if self.raw_event.len() < Self::MIN_SIZE {
            return Status::error_message("Too small event");
        }
        let size = u32::from_le_bytes(self.raw_event[0..4].try_into().unwrap()) as usize;
        if self.size as usize != size || size != self.raw_event.len() {
            return Status::error_message(&format!(
                "Size of event changed: was {} now {} real size {}",
                self.size,
                size,
                self.raw_event.len()
            ));
        }
        let stored_crc32 = u32::from_le_bytes(self.raw_event[size - 4..size].try_into().unwrap());
        let calculated_crc = crc32(&self.raw_event[..size - Self::TAIL_SIZE]);
        if calculated_crc != self.crc32 || calculated_crc != stored_crc32 {
            return Status::error_message(&format!(
                "CRC mismatch actual {calculated_crc:08x} expected {:08x} LogEvent[id = {:x?}, type = {}, flags = {}, data = {} bytes]",
                self.crc32,
                self.id,
                self.type_,
                self.flags,
                self.get_data().len()
            ));
        }
        Status::ok()
    }

    /// `BinlogEvent::create_raw` — serialize a record with the given payload.
    pub fn create_raw(id: u64, type_: i32, flags: i32, data: &[u8]) -> Vec<u8> {
        let raw_size = data.len() + Self::MIN_SIZE;
        let mut raw_event = Vec::with_capacity(raw_size);
        raw_event.extend_from_slice(&(raw_size as i32).to_le_bytes());
        raw_event.extend_from_slice(&id.to_le_bytes());
        raw_event.extend_from_slice(&type_.to_le_bytes());
        raw_event.extend_from_slice(&flags.to_le_bytes());
        raw_event.extend_from_slice(&0u64.to_le_bytes()); // extra
        raw_event.extend_from_slice(data);
        let crc = crc32(&raw_event[..raw_size - Self::TAIL_SIZE]);
        raw_event.extend_from_slice(&crc.to_le_bytes());
        raw_event
    }
}

/// `td::crc32` (IEEE CRC-32, as used by `td/utils/crypto.h`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}
