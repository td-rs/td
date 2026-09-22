//! Fuzz target: the `MessageContent` database blob (M.9.2/M.9.3) — the
//! value every MessageDb message row and `StoreMessage` binlog event
//! carries (`store_message_content`/`parse_message_content`,
//! MessageContent.cpp:1831-4514). These bytes are attacker-reachable:
//! they persist on disk and replay on every client start.
//!
//! Input layout: one leading flag byte (bit 0 selects the framing — 0 is
//! the LogEvent form version-prefix+body through `Parser::new_log_event`,
//! 1 is the bare body at the current version), the rest is the blob.
//! Panic = bug: any byte string must parse or report an error, never
//! crash; the manager-mediated FileId seams round-trip raw ids here (the
//! `RawIdStorage` semantics), and a rejected body degrades to
//! `MessageUnsupported`, matching the C++ `is_bad` arm.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tdclient::message_content::{
    MessageContentStorage, parse_message_content, store_message_content,
};
use tdclient::messages_manager::{MessageReactions, MinChannel};
use tdclient::store_parse::Parser;
use tdclient::user_manager::FileId;

/// Silence the tdlib log: the C++-parity rejects ("Load a message with an
/// invalid content of type …") print per exec at fuzz rates and would
/// drown the campaign log. Level 0 keeps only FATAL.
static QUIET_LOG: std::sync::Once = std::sync::Once::new();

const TAG_REACTIONS: i32 = -1000;
const TAG_REPLY_MARKUP: i32 = -1001;
const TAG_MIN_CHANNEL: i32 = -1002;

/// The raw-id seam: every manager-mediated record round-trips as a raw
/// int (the same semantics as the in-tree round-trip test storage), so a
/// successfully parsed blob re-stores byte-identically regardless of the
/// media arms the input exercised.
struct RawIdStorage;

impl MessageContentStorage for RawIdStorage {
    fn store_file(&self, file_id: FileId, out: &mut Vec<u8>) {
        tdclient::store_parse::store_i32(out, file_id.get());
    }
    fn parse_file(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        Ok(FileId::new(parser.i32_()?, 0))
    }
    fn store_animation(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_animation(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_audio(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_audio(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_document(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_document(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_video(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_video(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_video_note(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_video_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_voice_note(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_voice_note(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_sticker(&self, file_id: FileId, out: &mut Vec<u8>) {
        self.store_file(file_id, out);
    }
    fn parse_sticker(&mut self, parser: &mut Parser) -> Result<FileId, &'static str> {
        self.parse_file(parser)
    }
    fn store_poll(&self, poll_id: tdclient::message_content::PollId, out: &mut Vec<u8>) {
        tdclient::store_parse::store_i64(out, poll_id.get());
    }
    fn parse_poll(
        &mut self,
        parser: &mut Parser,
    ) -> Result<tdclient::message_content::PollId, &'static str> {
        Ok(tdclient::message_content::PollId::new(parser.i64_()?))
    }
    fn store_background(
        &self,
        background_id: tdclient::message_content::BackgroundId,
        out: &mut Vec<u8>,
    ) {
        tdclient::store_parse::store_i64(out, background_id.get());
    }
    fn parse_background(
        &mut self,
        parser: &mut Parser,
    ) -> Result<tdclient::message_content::BackgroundId, &'static str> {
        Ok(tdclient::message_content::BackgroundId::new(parser.i64_()?))
    }
    fn store_reactions(&mut self, _reactions: &MessageReactions, out: &mut Vec<u8>) {
        tdclient::store_parse::store_i32(out, TAG_REACTIONS);
    }
    fn parse_reactions(&mut self, input: &mut Parser) -> Result<MessageReactions, &'static str> {
        let tag = input.i32_()?;
        if tag != TAG_REACTIONS {
            return Err("wrong manager record tag");
        }
        Ok(MessageReactions::default())
    }
    fn store_reply_markup(
        &mut self,
        _reply_markup: &tdmtproto::telegram_api::ReplyMarkup,
        out: &mut Vec<u8>,
    ) {
        tdclient::store_parse::store_i32(out, TAG_REPLY_MARKUP);
    }
    fn parse_reply_markup(
        &mut self,
        input: &mut Parser,
    ) -> Result<tdmtproto::telegram_api::ReplyMarkup, &'static str> {
        let tag = input.i32_()?;
        if tag != TAG_REPLY_MARKUP {
            return Err("wrong manager record tag");
        }
        Ok(tdmtproto::telegram_api::ReplyMarkup::replyKeyboardHide(
            tdmtproto::telegram_api::replyKeyboardHide::default(),
        ))
    }
    fn store_min_channel(&mut self, _min_channel: &MinChannel, out: &mut Vec<u8>) {
        tdclient::store_parse::store_i32(out, TAG_MIN_CHANNEL);
    }
    fn parse_min_channel(&mut self, input: &mut Parser) -> Result<MinChannel, &'static str> {
        let tag = input.i32_()?;
        if tag != TAG_MIN_CHANNEL {
            return Err("wrong manager record tag");
        }
        Ok(MinChannel::default())
    }
    fn on_load_web_page_url_from_database(
        &mut self,
        _web_page_id: tdclient::message_content::WebPageId,
        _url: &str,
    ) {
    }
}

fuzz_target!(|data: &[u8]| {
    QUIET_LOG.call_once(|| {
        tdutils::logging::set_verbosity_level(tdutils::logging::FATAL);
    });
    let Some((&framing, blob)) = data.split_first() else {
        return;
    };
    let parse_result = if (framing & 0x01) == 0 {
        // the LogEvent framing: the version prefix is fetched and checked
        let Ok(mut parser) = Parser::new_log_event(blob) else {
            return;
        };
        parse_message_content(&mut parser, &mut RawIdStorage)
    } else {
        // the bare body at the current version
        let mut parser = Parser::new(blob);
        parse_message_content(&mut parser, &mut RawIdStorage)
    };
    if let Ok(content) = parse_result {
        // a parsed blob must re-store without crashing (byte identity of
        // the degraded raw-id repr is not required — the input may mix
        // versions/arms the re-store canonifies)
        let mut out = Vec::new();
        store_message_content(&content, &RawIdStorage, &mut out);
        let mut parser = Parser::new(&out);
        let _ = parse_message_content(&mut parser, &mut RawIdStorage);
    }
});
