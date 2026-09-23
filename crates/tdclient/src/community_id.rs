//! `CommunityId` (td/telegram/CommunityId.h) — the M.34 Wave-3 community
//! id.

use crate::chat_manager::ChannelId;
use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};

/// `td::CommunityId` — the positive community identifier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommunityId(i64);

impl CommunityId {
    pub fn new(community_id: i64) -> CommunityId {
        CommunityId(community_id)
    }

    /// `is_valid` (CommunityId.h:16-18).
    pub fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `get` (CommunityId.h:19-21).
    pub fn get(self) -> i64 {
        self.0
    }

    /// `get_fake_dialog_id` (CommunityId.h:22-24).
    pub fn get_fake_dialog_id(self) -> DialogId {
        DialogId::from_channel_id(ChannelId::new(self.0))
    }

    /// `CommunityId::store` (CommunityId.h:35-37).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i64(out, self.0);
    }

    /// `CommunityId::parse` (CommunityId.h:39-41).
    pub fn parse_from(parser: &mut Parser) -> Result<CommunityId, &'static str> {
        Ok(CommunityId::new(parser.i64_()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_and_round_trip() {
        let community_id = CommunityId::new(7);
        assert!(community_id.is_valid());
        assert!(!CommunityId::default().is_valid());
        assert_eq!(community_id.get(), 7);
        let mut data = Vec::new();
        community_id.store(&mut data);
        let mut parser = Parser::new(&data);
        assert_eq!(CommunityId::parse_from(&mut parser).unwrap(), community_id);
    }
}
