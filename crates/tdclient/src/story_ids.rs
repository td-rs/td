//! The story id planes (M.47): `StoryId.h`, `StoryFullId.h`,
//! `StoryListId.h`, `StoryAlbumId.h` and `StoryAlbumFullId.h`. The store /
//! parse bytes follow the C++ `td::store`/`td::parse` of the wrapped
//! integers (StoryId.h:71-81, StoryFullId.h:60-70, StoryAlbumId.h:78-88,
//! StoryAlbumFullId.h:56-66).

use crate::chat_manager::{ChannelId, ChatId};
use crate::dialog_id::DialogId;
use crate::store_parse::{self, Parser};
use crate::td_api;
use crate::user_manager::UserId;
use tdmtproto::telegram_api;
use tdutils::status::Result;

/// `td::StoryId` (StoryId.h:20-88).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoryId(i32);

impl StoryId {
    /// `StoryId::MAX_SERVER_STORY_ID` (StoryId.h:24).
    pub const MAX_SERVER_STORY_ID: i32 = 1999999999;

    pub const fn new(id: i32) -> StoryId {
        StoryId(id)
    }

    /// `StoryId::get_input_story_ids` (StoryId.h:33-41).
    pub fn get_input_story_ids(story_ids: &[StoryId]) -> Vec<i32> {
        story_ids.iter().map(|story_id| story_id.get()).collect()
    }

    /// `StoryId::get_story_ids` (StoryId.h:43-51).
    pub fn get_story_ids(input_story_ids: &[i32]) -> Vec<StoryId> {
        input_story_ids.iter().copied().map(StoryId::new).collect()
    }

    pub const fn get(self) -> i32 {
        self.0
    }

    /// `StoryId::is_valid` — `id > 0` (StoryId.h:57-59).
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `StoryId::is_server` (StoryId.h:61-63).
    pub const fn is_server(self) -> bool {
        self.0 > 0 && self.0 <= Self::MAX_SERVER_STORY_ID
    }

    /// `StoryId::store` (StoryId.h:71-74).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `StoryId::parse` (StoryId.h:76-79).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryId, &'static str> {
        Ok(StoryId::new(parser.i32_()?))
    }
}

impl std::fmt::Display for StoryId {
    /// `operator<<(StringBuilder, StoryId)` (StoryId.h:95-97).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "story {}", self.0)
    }
}

/// `td::StoryFullId` (StoryFullId.h:17-77).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StoryFullId {
    dialog_id: DialogId,
    story_id: StoryId,
}

impl StoryFullId {
    pub const fn new(dialog_id: DialogId, story_id: StoryId) -> StoryFullId {
        StoryFullId {
            dialog_id,
            story_id,
        }
    }

    pub const fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub const fn get_story_id(self) -> StoryId {
        self.story_id
    }

    /// `StoryFullId::is_valid` (StoryFullId.h:45-47).
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid() && self.story_id.is_valid()
    }

    /// `StoryFullId::is_server` (StoryFullId.h:49-51).
    pub fn is_server(self) -> bool {
        self.dialog_id.is_valid() && self.story_id.is_server()
    }

    /// `StoryFullId::store` (StoryFullId.h:59-64).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.story_id.store(out);
    }

    /// `StoryFullId::parse` (StoryFullId.h:66-70).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryFullId, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let story_id = StoryId::parse(parser)?;
        Ok(StoryFullId::new(dialog_id, story_id))
    }
}

impl std::fmt::Display for StoryFullId {
    /// `operator<<(StringBuilder, StoryFullId)` (StoryFullId.h:85-87); the
    /// DialogId half prints as its raw id (the base has no DialogId
    /// `Display`).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} in dialog {}", self.story_id, self.dialog_id.get())
    }
}

/// `td::StoryListId` (StoryListId.h:19-95) — the `None`/`Main`/`Archive`
/// discriminator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StoryListId {
    /// `Type::None` (StoryListId.h:20) — the default.
    #[default]
    None,
    /// `StoryListId::main()` (StoryListId.h:40-42).
    Main,
    /// `StoryListId::archive()` (StoryListId.h:44-46).
    Archive,
}

impl StoryListId {
    /// `StoryListId(const td_api::object_ptr<td_api::StoryList> &)`
    /// (StoryListId.h:26-38); `None` for the null pointer.
    pub fn from_story_list_object(
        story_list: Option<&td_api::StoryList>,
    ) -> std::result::Result<StoryListId, &'static str> {
        let Some(story_list) = story_list else {
            return Ok(StoryListId::None);
        };
        match story_list {
            td_api::StoryList::storyListMain(_) => Ok(StoryListId::Main),
            td_api::StoryList::storyListArchive(_) => Ok(StoryListId::Archive),
        }
    }

    /// `StoryListId::get_story_list_object` (StoryListId.h:48-59); `None`
    /// for the C++ nullptr.
    pub fn get_story_list_object(self) -> Option<td_api::StoryList> {
        match self {
            StoryListId::None => None,
            StoryListId::Main => Some(td_api::StoryList::storyListMain(td_api::storyListMain)),
            StoryListId::Archive => Some(td_api::StoryList::storyListArchive(
                td_api::storyListArchive,
            )),
        }
    }

    /// `StoryListId::is_valid` (StoryListId.h:61-63).
    pub fn is_valid(self) -> bool {
        matches!(self, StoryListId::Main | StoryListId::Archive)
    }
}

impl std::fmt::Display for StoryListId {
    /// `operator<<(StringBuilder, StoryListId)` (StoryListId.h:85-93).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoryListId::Main => write!(f, "MainStoryList"),
            StoryListId::Archive => write!(f, "ArchiveStoryList"),
            StoryListId::None => write!(f, "InvalidStoryList"),
        }
    }
}

/// `td::StoryAlbumId` (StoryAlbumId.h:19-92).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoryAlbumId(i32);

impl StoryAlbumId {
    pub const fn new(id: i32) -> StoryAlbumId {
        StoryAlbumId(id)
    }

    pub const fn get(self) -> i32 {
        self.0
    }

    /// `StoryAlbumId::is_valid` (StoryAlbumId.h:47-49).
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// `StoryAlbumId::get_story_album_ids_object` (StoryAlbumId.h:55-64); the
    /// `get_input_story_album_ids` twin (StoryAlbumId.h:66-68) is the same
    /// conversion.
    pub fn get_story_album_ids_object(album_ids: &[StoryAlbumId]) -> Vec<i32> {
        album_ids.iter().map(|album_id| album_id.get()).collect()
    }

    /// `StoryAlbumId::get_story_album_ids` (StoryAlbumId.h:70-78).
    pub fn get_story_album_ids(album_ids: &[i32]) -> Vec<StoryAlbumId> {
        album_ids.iter().copied().map(StoryAlbumId::new).collect()
    }

    /// `StoryAlbumId::store` (StoryAlbumId.h:80-83).
    pub fn store(self, out: &mut Vec<u8>) {
        store_parse::store_i32(out, self.0);
    }

    /// `StoryAlbumId::parse` (StoryAlbumId.h:85-88).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryAlbumId, &'static str> {
        Ok(StoryAlbumId::new(parser.i32_()?))
    }
}

impl std::fmt::Display for StoryAlbumId {
    /// `operator<<(StringBuilder, StoryAlbumId)` (StoryAlbumId.h:99-101).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "story album {}", self.0)
    }
}

/// `td::StoryAlbumFullId` (StoryAlbumFullId.h:16-75).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StoryAlbumFullId {
    dialog_id: DialogId,
    story_album_id: StoryAlbumId,
}

impl StoryAlbumFullId {
    pub const fn new(dialog_id: DialogId, story_album_id: StoryAlbumId) -> StoryAlbumFullId {
        StoryAlbumFullId {
            dialog_id,
            story_album_id,
        }
    }

    pub const fn get_dialog_id(self) -> DialogId {
        self.dialog_id
    }

    pub const fn get_story_album_id(self) -> StoryAlbumId {
        self.story_album_id
    }

    /// `StoryAlbumFullId::is_valid` (StoryAlbumFullId.h:43-45).
    pub fn is_valid(self) -> bool {
        self.dialog_id.is_valid() && self.story_album_id.is_valid()
    }

    /// `StoryAlbumFullId::store` (StoryAlbumFullId.h:55-60).
    pub fn store(self, out: &mut Vec<u8>) {
        self.dialog_id.store(out);
        self.story_album_id.store(out);
    }

    /// `StoryAlbumFullId::parse` (StoryAlbumFullId.h:62-66).
    pub fn parse(parser: &mut Parser) -> std::result::Result<StoryAlbumFullId, &'static str> {
        let dialog_id = DialogId::parse_from(parser)?;
        let story_album_id = StoryAlbumId::parse(parser)?;
        Ok(StoryAlbumFullId::new(dialog_id, story_album_id))
    }
}

impl std::fmt::Display for StoryAlbumFullId {
    /// `operator<<(StringBuilder, StoryAlbumFullId)`
    /// (StoryAlbumFullId.h:85-87); the DialogId half prints as its raw id.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} of dialog {}",
            self.story_album_id,
            self.dialog_id.get()
        )
    }
}

/// The `StoryListId` → active_stories row discriminator
/// (`add_active_stories`, StoryDb.cpp:232-236).
pub fn story_list_id_to_int(story_list_id: StoryListId) -> i32 {
    if story_list_id == StoryListId::Archive {
        1
    } else {
        0
    }
}

/// The `td_api::story`-plane `storyFullId` conversion — the C++
/// `StoryFullId` has no direct object form; the requests pass the pair
/// through [`td_api::StoryFullId`].
pub fn story_full_id_to_object(story_full_id: StoryFullId) -> td_api::StoryFullId {
    td_api::StoryFullId {
        poster_chat_id: story_full_id.get_dialog_id().get(),
        story_id: story_full_id.get_story_id().get(),
    }
}

/// The inverse of [`story_full_id_to_object`]; the validity of the parts is
/// the caller's business, like the C++ request sites.
pub fn story_full_id_from_object(object: &td_api::StoryFullId) -> StoryFullId {
    StoryFullId::new(
        DialogId::new(object.poster_chat_id),
        StoryId::new(object.story_id),
    )
}

/// The `Status`-taking parse of a td_api `storyFullId`, as the request
/// handlers use it (Requests.cpp:4763-4766): the C++ accepts the raw ids.
pub fn parse_story_full_id_object(
    object: Option<&td_api::StoryFullId>,
) -> Result<Option<StoryFullId>> {
    let Some(object) = object else {
        return Result::Ok(None);
    };
    Result::Ok(Some(story_full_id_from_object(object)))
}

/// The `telegram_api::Peer` → `DialogId` intake used by the story planes
/// (DialogId.h:53-70).
pub fn dialog_id_from_peer(peer: Option<&telegram_api::Peer>) -> DialogId {
    let Some(peer) = peer else {
        return DialogId::default();
    };
    match peer {
        telegram_api::Peer::peerUser(peer) => DialogId::from_user_id(UserId::new(peer.user_id)),
        telegram_api::Peer::peerChat(peer) => DialogId::from_chat_id(ChatId::new(peer.chat_id)),
        telegram_api::Peer::peerChannel(peer) => {
            DialogId::from_channel_id(ChannelId::new(peer.channel_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story_id_validity_boundaries() {
        // StoryId.h:24-63 — server ids are positive and bounded.
        assert!(!StoryId::new(0).is_valid());
        assert!(StoryId::new(1).is_valid());
        assert!(StoryId::new(StoryId::MAX_SERVER_STORY_ID).is_server());
        assert!(!StoryId::new(StoryId::MAX_SERVER_STORY_ID + 1).is_server());
        assert!(StoryId::new(StoryId::MAX_SERVER_STORY_ID + 1).is_valid());
        assert!(!StoryId::new(-1).is_valid());
    }

    #[test]
    fn id_vector_conversions_round_trip() {
        let ids = StoryId::get_story_ids(&[1, 5, 100]);
        assert_eq!(StoryId::get_input_story_ids(&ids), vec![1, 5, 100]);
        let album_ids = StoryAlbumId::get_story_album_ids(&[2, 9]);
        assert_eq!(
            StoryAlbumId::get_story_album_ids_object(&album_ids),
            vec![2, 9]
        );
    }

    #[test]
    fn story_full_id_store_parse_round_trip() {
        let value = StoryFullId::new(
            DialogId::from_user_id(crate::user_manager::UserId::new(3)),
            StoryId::new(77),
        );
        let mut out = Vec::new();
        value.store(&mut out);
        let mut parser = Parser::new(&out);
        let parsed = StoryFullId::parse(&mut parser).expect("parse");
        assert_eq!(parsed, value);
    }

    #[test]
    fn story_list_object_conversions() {
        assert_eq!(
            StoryListId::from_story_list_object(Some(&td_api::StoryList::storyListMain(
                td_api::storyListMain
            )))
            .expect("main"),
            StoryListId::Main
        );
        assert_eq!(
            StoryListId::from_story_list_object(None).expect("none"),
            StoryListId::None
        );
        assert_eq!(
            StoryListId::Archive.get_story_list_object(),
            Some(td_api::StoryList::storyListArchive(
                td_api::storyListArchive
            ))
        );
        assert!(StoryListId::Main.is_valid());
        assert!(!StoryListId::None.is_valid());
    }
}
