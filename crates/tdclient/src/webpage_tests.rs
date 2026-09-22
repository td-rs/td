//! The M.17.1 tests: byte-exact store/parse round trips over every
//! WebPageBlock/RichText field, the wire intakes and the object
//! builders. The test env stores bare FileIds (the FileManager codec
//! stand-in) and answers `None` for every media-object builder — the
//! collapsed-manager shape.

use crate::emoji_status::CustomEmojiId;
use crate::files::FileId;
use crate::formatted_date::FormattedDate;
use crate::inline_keyboard_button::{
    InlineKeyboardButton, InlineKeyboardButtonType, KeyboardButtonStyle, RichButtonStyle,
    TargetDialogTypes, get_inline_keyboard_button_from_td_api,
};
use crate::photo::{
    DialogPhoto, Dimensions, Photo, PhotoSize, parse_dialog_photo, parse_dimensions, parse_photo,
    store_dialog_photo, store_dimensions, store_photo,
};
use crate::store_parse::{Parser, version};
use crate::td_api;
use crate::user_manager::UserId;
use crate::webpage::*;

/// The test env: FileIds round-trip through the raw codec; every
/// media-object builder answers `None` like the collapsed managers.
#[derive(Default)]
struct TestEnv {
    file_counter: i32,
}

impl crate::photo::DialogPhotoStoreEnv for TestEnv {
    fn store_file_id(&mut self, file_id: FileId, out: &mut Vec<u8>) {
        crate::store_parse::store_i32(out, file_id.get());
    }

    fn parse_file_id(
        &mut self,
        parser: &mut Parser<'_>,
    ) -> std::result::Result<FileId, &'static str> {
        Ok(FileId::new(parser.i32_()?, 0))
    }
}

impl WebPageBlockEnv for TestEnv {
    fn get_document_object(&self, _file_id: FileId) -> Option<td_api::Document> {
        None
    }
    fn get_animation_object(&self, _file_id: FileId) -> Option<td_api::Animation> {
        None
    }
    fn get_audio_object(&self, _file_id: FileId) -> Option<td_api::Audio> {
        None
    }
    fn get_video_object(&self, _file_id: FileId) -> Option<td_api::Video> {
        None
    }
    fn get_video_note_object(&self, _file_id: FileId) -> Option<td_api::VideoNote> {
        None
    }
    fn get_voice_note_object(&self, _file_id: FileId) -> Option<td_api::VoiceNote> {
        None
    }
    fn get_accent_color_id_object(
        &self,
        accent_color_id: crate::peer_color::AccentColorId,
        _fallback: crate::peer_color::AccentColorId,
    ) -> Option<i32> {
        Some(accent_color_id.get())
    }
    fn get_chat_photo_info_object(&self, _photo: &DialogPhoto) -> Option<td_api::ChatPhotoInfo> {
        None
    }
    fn get_instant_view_link(&self, url: &str, rhash: &str) -> String {
        format!("https://t.me/iv?url={url}&rhash={rhash}")
    }
    fn get_instant_view_link_rhash(&self, _real_url: &str) -> String {
        String::new()
    }
    fn get_instant_view_link_url(&self, _real_url: &str) -> String {
        String::new()
    }
    fn as_input_document(
        &mut self,
        file_id: FileId,
    ) -> Option<(i64, tdmtproto::telegram_api::InputDocument)> {
        if !file_id.is_valid() {
            return None;
        }
        self.file_counter += 1;
        let id = self.file_counter as i64;
        Some((
            id,
            tdmtproto::telegram_api::InputDocument::inputDocument(
                tdmtproto::telegram_api::inputDocument {
                    id,
                    access_hash: 0,
                    file_reference: Vec::new(),
                },
            ),
        ))
    }
    fn as_input_photo(
        &mut self,
        file_id: FileId,
    ) -> Option<(i64, tdmtproto::telegram_api::InputPhoto)> {
        if !file_id.is_valid() {
            return None;
        }
        self.file_counter += 1;
        let id = self.file_counter as i64;
        Some((
            id,
            tdmtproto::telegram_api::InputPhoto::inputPhoto(tdmtproto::telegram_api::inputPhoto {
                id,
                access_hash: 0,
                file_reference: Vec::new(),
            }),
        ))
    }
    fn ensure_input_user(
        &mut self,
        _user_id: UserId,
    ) -> std::result::Result<(), tdutils::status::Status> {
        Ok(())
    }
    fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
        user_id.get()
    }
    fn is_my_user_bot(&self) -> bool {
        false
    }
    fn on_get_page_block_channel(
        &mut self,
        channel: tdmtproto::telegram_api::channel,
    ) -> Option<ChatLinkInfo> {
        let channel_id = crate::chat_manager::ChannelId::new(channel.id);
        if !channel_id.is_valid() {
            tdutils::log_error!("Receive invalid channel {}", channel.id);
            return None;
        }
        let photo = DialogPhoto::get_dialog_photo(channel.photo);
        Some(ChatLinkInfo {
            title: channel.title,
            photo,
            username: channel.username,
            accent_color_id: crate::peer_color::AccentColorId::from_peer_id(channel.id),
            channel_id,
        })
    }
}

fn round_trip<T>(
    f: impl FnOnce(&mut TestEnv, &mut Vec<u8>),
    g: impl FnOnce(&mut TestEnv, &mut Parser<'_>) -> std::result::Result<T, &'static str>,
    check: impl FnOnce(T),
) {
    let mut env = TestEnv::default();
    let mut out = Vec::new();
    f(&mut env, &mut out);
    let mut env = TestEnv::default();
    let mut parser = Parser::new(&out);
    let parsed = g(&mut env, &mut parser).expect("parse round trip");
    parser.fetch_end().expect("no trailing bytes");
    check(parsed);
}

fn plain(text: &str) -> RichText {
    RichText {
        r#type: RichTextType::Plain,
        content: text.to_owned(),
        ..RichText::default()
    }
}

#[test]
fn web_page_id_basics() {
    let id = WebPageId::new(123);
    assert_eq!(id.get(), 123);
    assert!(id.is_valid());
    assert!(!WebPageId::default().is_valid());
    assert_eq!(id.to_string(), "link preview 123");
    let mut out = Vec::new();
    id.store(&mut out);
    assert_eq!(out, 123i64.to_le_bytes());
    let mut parser = Parser::new(&out);
    assert_eq!(WebPageId::parse(&mut parser).unwrap(), id);
}

#[test]
fn rich_text_plain_round_trip() {
    round_trip(
        |env, out| {
            plain("hello, world").store(env, out);
        },
        RichText::parse,
        |text| {
            assert_eq!(text.r#type, RichTextType::Plain);
            assert_eq!(text.content, "hello, world");
            assert!(text.texts.is_empty());
            assert!(!text.empty());
        },
    );
    // the default rich text is the empty plain
    assert!(RichText::default().empty());
}

#[test]
fn rich_text_all_typed_fields_round_trip() {
    // the per-type fields: each typed rich text stores exactly the rows
    // the C++ type guard persists
    let rich_text = RichText {
        r#type: RichTextType::Button,
        texts: vec![
            plain("bold"),
            RichText {
                r#type: RichTextType::Url,
                content: "https://example.com".to_owned(),
                texts: vec![plain("link")],
                web_page_id: WebPageId::new(456),
                ..RichText::default()
            },
        ],
        button_style: RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleDanger(
            td_api::buttonStyleDanger {},
        ))),
        button: Some(InlineKeyboardButton {
            type_: InlineKeyboardButtonType::UrlAuth,
            id: -12345,
            user_id: UserId::new(77),
            style: KeyboardButtonStyle::from_td_api(
                Some(td_api::ButtonStyle::buttonStylePrimary(
                    td_api::buttonStylePrimary {},
                )),
                536870912,
            ),
            text: "Press me".to_owned(),
            forward_text: "Forward!".to_owned(),
            data: "https://example.com".to_owned(),
        }),
        ..RichText::default()
    };
    round_trip(
        |env, out| rich_text.store(env, out),
        RichText::parse,
        |parsed| {
            assert_eq!(parsed, rich_text);
        },
    );

    // the scalar typed fields each survive their own type guard
    let icon = RichText {
        r#type: RichTextType::Icon,
        content: (640u32 * 65536 + 480).to_string(),
        document_file_id: FileId::new(7, 0),
        ..RichText::default()
    };
    round_trip(
        |env, out| icon.store(env, out),
        RichText::parse,
        |parsed| assert_eq!(parsed, icon),
    );
    let url = RichText {
        r#type: RichTextType::Url,
        content: "https://example.com".to_owned(),
        texts: vec![plain("link")],
        web_page_id: WebPageId::new(89),
        ..RichText::default()
    };
    round_trip(
        |env, out| url.store(env, out),
        RichText::parse,
        |parsed| assert_eq!(parsed, url),
    );
    let emoji = RichText {
        r#type: RichTextType::CustomEmoji,
        content: "alt".to_owned(),
        custom_emoji_id: CustomEmojiId::new(536870912),
        ..RichText::default()
    };
    round_trip(
        |env, out| emoji.store(env, out),
        RichText::parse,
        |parsed| assert_eq!(parsed, emoji),
    );
    let date = RichText {
        r#type: RichTextType::FormattedDate,
        texts: vec![plain("d")],
        date: FormattedDate::from_raw(1700000000, 3),
        ..RichText::default()
    };
    round_trip(
        |env, out| date.store(env, out),
        RichText::parse,
        |parsed| assert_eq!(parsed, date),
    );
    let mention_name = RichText {
        r#type: RichTextType::MentionName,
        texts: vec![plain("m")],
        user_id: UserId::new(42),
        ..RichText::default()
    };
    round_trip(
        |env, out| mention_name.store(env, out),
        RichText::parse,
        |parsed| assert_eq!(parsed, mention_name),
    );
}

#[test]
fn rich_text_icon_recovers_from_bad_file_id() {
    // an Icon whose stored file id is invalid resets to the empty plain,
    // exactly like the C++ parse
    round_trip(
        |_env, out| {
            // hand-crafting the raw record with file id 0
            store_i32_raw(out, RichTextType::Icon as i32);
            crate::store_parse::store_string(out, "");
            crate::store_parse::store_i32(out, 0); // no texts
            crate::store_parse::store_i32(out, 0); // the invalid file id
        },
        RichText::parse,
        |parsed| {
            assert!(parsed.empty());
            assert_eq!(parsed.r#type, RichTextType::Plain);
        },
    );
}

fn store_i32_raw(out: &mut Vec<u8>, value: i32) {
    crate::store_parse::store_i32(out, value);
}

#[test]
fn ordered_list_labels() {
    assert_eq!(ordered_list_label_for_test(1, "1"), "1.");
    assert_eq!(ordered_list_label_for_test(27, "1"), "27.");
    assert_eq!(ordered_list_label_for_test(1, "a"), "a.");
    assert_eq!(ordered_list_label_for_test(26, "a"), "z.");
    assert_eq!(ordered_list_label_for_test(27, "a"), "aa.");
    assert_eq!(ordered_list_label_for_test(1, "A"), "A.");
    assert_eq!(ordered_list_label_for_test(1, "i"), "i.");
    assert_eq!(ordered_list_label_for_test(4, "i"), "iv.");
    assert_eq!(ordered_list_label_for_test(9, "i"), "ix.");
    assert_eq!(ordered_list_label_for_test(14, "i"), "xiv.");
    assert_eq!(ordered_list_label_for_test(1900, "I"), "MCM.");
    assert_eq!(ordered_list_label_for_test(4000, "i"), "4000.");
    assert_eq!(ordered_list_label_for_test(2, "q"), "2.");
}

#[test]
fn caption_and_table_cell_round_trip() {
    let caption = WebPageBlockCaption {
        text: plain("caption"),
        credit: plain("(c) someone"),
    };
    round_trip(
        |env, out| caption.store(env, out),
        WebPageBlockCaption::parse,
        |parsed| assert_eq!(parsed, caption),
    );

    let cell = WebPageBlockTableCell {
        text: plain("cell"),
        is_header: true,
        align_left: false,
        align_center: false,
        align_right: true,
        valign_top: false,
        valign_middle: false,
        valign_bottom: true,
        colspan: 3,
        rowspan: 2,
    };
    round_trip(
        |env, out| cell.store(env, out),
        WebPageBlockTableCell::parse,
        |parsed| {
            assert_eq!(parsed, cell);
            assert!(parsed.align_right && !parsed.align_left && !parsed.align_center);
        },
    );
    // the default cell stores only the flag block
    let default_cell = WebPageBlockTableCell::default();
    round_trip(
        |env, out| default_cell.store(env, out),
        WebPageBlockTableCell::parse,
        |parsed| {
            assert_eq!(parsed, default_cell);
        },
    );
}

#[test]
fn related_article_round_trip() {
    let article = RelatedArticle {
        url: "https://example.com/a".to_owned(),
        web_page_id: WebPageId::new(1001),
        title: "A title".to_owned(),
        description: "A description".to_owned(),
        photo: Photo::default(),
        author: "An author".to_owned(),
        published_date: 1700000000,
    };
    round_trip(
        |env, out| article.store(env, out),
        RelatedArticle::parse,
        |parsed| {
            assert_eq!(parsed, article);
            assert!(parsed.photo.is_empty());
        },
    );
}

#[test]
fn photo_record_round_trip() {
    let photo = Photo {
        id: 12345,
        date: 1700000001,
        minithumbnail: "\x01\x02\x03".to_owned(),
        photos: vec![PhotoSize {
            size_type: 'x' as i32,
            dimensions: Dimensions {
                width: 640,
                height: 480,
            },
            file_id: FileId::new(3, 0),
            size: 12,
            progressive_sizes: Vec::new(),
        }],
        has_stickers: true,
        sticker_file_ids: vec![FileId::new(4, 0)],
        ..Photo::default()
    };
    round_trip(
        |env, out| store_photo(env, &photo, out),
        parse_photo,
        |parsed| {
            assert_eq!(parsed.id, 12345);
            assert_eq!(parsed.date, 1700000001);
            assert_eq!(parsed.photos.len(), 1);
            assert_eq!(parsed.photos[0].file_id.get(), 3);
            assert!(parsed.has_stickers);
            assert_eq!(parsed.sticker_file_ids.len(), 1);
            assert_eq!(parsed.sticker_file_ids[0].get(), 4);
        },
    );
}

#[test]
fn dialog_photo_and_dimensions_round_trip() {
    let dialog_photo = DialogPhoto {
        small_file_id: FileId::new(1, 0),
        big_file_id: FileId::new(2, 0),
        minithumbnail: String::new(),
        has_animation: true,
        is_personal: false,
    };
    round_trip(
        |env, out| store_dialog_photo(env, &dialog_photo, out),
        parse_dialog_photo,
        |parsed| {
            assert!(parsed.has_animation);
            assert!(!parsed.is_personal);
            assert_eq!(parsed.small_file_id.get(), 1);
            assert_eq!(parsed.big_file_id.get(), 2);
        },
    );

    round_trip(
        |_env, out| {
            store_dimensions(
                out,
                Dimensions {
                    width: 800,
                    height: 600,
                },
            )
        },
        |_env, parser| parse_dimensions(parser),
        |parsed| {
            assert_eq!(parsed.width, 800);
            assert_eq!(parsed.height, 600);
        },
    );
}

#[test]
fn text_blocks_round_trip() {
    // every one-field text block + the parametrized ones
    let blocks = vec![
        WebPageBlock::Title(plain("t")),
        WebPageBlock::Subtitle(plain("s")),
        WebPageBlock::AuthorDate {
            author: plain("a"),
            date: 1700000002,
        },
        WebPageBlock::Header(plain("h")),
        WebPageBlock::Subheader(plain("sh")),
        WebPageBlock::Paragraph(plain("p")),
        WebPageBlock::Preformatted {
            text: plain("code"),
            language: "rust".to_owned(),
        },
        WebPageBlock::Footer(plain("f")),
        WebPageBlock::Divider,
        WebPageBlock::Anchor {
            name: "anchor".to_owned(),
        },
        WebPageBlock::Heading {
            text: plain("big"),
            size: 3,
        },
        WebPageBlock::Math {
            source: "1+1".to_owned(),
        },
        WebPageBlock::Thinking(plain("hmm")),
        WebPageBlock::Kicker(plain("k")),
    ];
    for block in &blocks {
        let expected = block.clone();
        let expected_type = block.get_type();
        round_trip(
            |env, out| block.store(env, out),
            WebPageBlock::parse_block,
            move |parsed| {
                assert_eq!(parsed.get_type(), expected_type);
                assert_eq!(parsed, expected);
            },
        );
    }
}

#[test]
fn media_and_structured_blocks_round_trip() {
    let photo = Photo {
        id: 777,
        ..Photo::default()
    };
    let caption = WebPageBlockCaption {
        text: plain("cap"),
        credit: RichText::default(),
    };
    let blocks = vec![
        WebPageBlock::Animation {
            animation_file_id: FileId::new(10, 0),
            caption: caption.clone(),
            need_autoplay: true,
            has_spoiler: true,
        },
        WebPageBlock::Animation {
            animation_file_id: FileId::default(),
            caption: caption.clone(),
            need_autoplay: false,
            has_spoiler: false,
        },
        WebPageBlock::Photo {
            photo,
            caption: caption.clone(),
            url: "https://example.com".to_owned(),
            web_page_id: WebPageId::new(5),
            has_spoiler: false,
        },
        WebPageBlock::Video {
            video_file_id: FileId::new(11, 0),
            caption: caption.clone(),
            need_autoplay: true,
            is_looped: true,
            has_spoiler: false,
        },
        WebPageBlock::Audio {
            audio_file_id: FileId::new(12, 0),
            caption: caption.clone(),
        },
        WebPageBlock::VoiceNote {
            voice_note_file_id: FileId::new(13, 0),
            caption: caption.clone(),
        },
        WebPageBlock::Document {
            document_file_id: FileId::new(14, 0),
            caption: caption.clone(),
        },
        WebPageBlock::Cover(Box::new(WebPageBlock::Paragraph(plain("cover")))),
        WebPageBlock::Embedded {
            url: "https://example.com/embed".to_owned(),
            html: "<b>hi</b>".to_owned(),
            poster_photo: Photo::default(),
            dimensions: Dimensions {
                width: 100,
                height: 50,
            },
            caption: caption.clone(),
            is_full_width: true,
            allow_scrolling: false,
        },
        WebPageBlock::EmbeddedPost {
            url: "https://example.com/post".to_owned(),
            author: "anon".to_owned(),
            author_photo: Photo::default(),
            date: 1700000003,
            page_blocks: vec![WebPageBlock::Paragraph(plain("post body"))],
            caption: caption.clone(),
        },
        WebPageBlock::Collage {
            page_blocks: vec![
                WebPageBlock::Paragraph(plain("one")),
                WebPageBlock::Paragraph(plain("two")),
            ],
            caption,
        },
        WebPageBlock::Details {
            header: plain("details"),
            page_blocks: vec![WebPageBlock::Paragraph(plain("inside"))],
            is_open: true,
        },
        WebPageBlock::BlockQuote {
            text: plain("quote"),
            credit: plain("who"),
        },
        WebPageBlock::ExpandableBlockQuote {
            text: plain("expand"),
            credit: plain("who2"),
        },
        WebPageBlock::PullQuote {
            text: plain("pull"),
            credit: plain("who3"),
        },
        WebPageBlock::BlockQuoteBlocks {
            page_blocks: vec![WebPageBlock::Paragraph(plain("quote blocks"))],
            caption: plain("caption text"),
        },
        WebPageBlock::Map {
            location: crate::chat_manager::Location::from_td_api_location(&td_api::Location {
                latitude: 1.5,
                longitude: 2.5,
                horizontal_accuracy: 10.0,
            }),
            zoom: 13,
            dimensions: Dimensions {
                width: 100,
                height: 200,
            },
            caption: WebPageBlockCaption::default(),
        },
        WebPageBlock::ButtonRow(WebPageBlockButtonRow {
            buttons: vec![WebPageBlockRowButton {
                text: plain("click"),
                style: RichButtonStyle::from_td_api(Some(td_api::ButtonStyle::buttonStyleSuccess(
                    td_api::buttonStyleSuccess {},
                ))),
                button: InlineKeyboardButton {
                    type_: InlineKeyboardButtonType::Url,
                    id: 0,
                    user_id: UserId::default(),
                    style: KeyboardButtonStyle::default(),
                    text: "b".to_owned(),
                    forward_text: String::new(),
                    data: "https://example.com".to_owned(),
                },
            }],
            align_left: false,
            align_center: true,
            align_right: false,
        }),
        WebPageBlock::RelatedArticles {
            header: plain("related"),
            related_articles: vec![RelatedArticle {
                url: "u".to_owned(),
                web_page_id: WebPageId::default(),
                title: String::new(),
                description: String::new(),
                photo: Photo::default(),
                author: String::new(),
                published_date: 0,
            }],
        },
        WebPageBlock::List(WebPageBlockList {
            items: vec![
                WebPageBlockListItem {
                    label: String::new(),
                    page_blocks: vec![WebPageBlock::Paragraph(plain("bullet"))],
                    has_checkbox: true,
                    is_checked: true,
                    value: -1,
                    r#type: String::new(),
                },
                WebPageBlockListItem {
                    label: "1.".to_owned(),
                    page_blocks: vec![WebPageBlock::Paragraph(plain("ordered"))],
                    has_checkbox: false,
                    is_checked: false,
                    value: 1,
                    r#type: "1".to_owned(),
                },
            ],
            start: 5,
            is_reversed: true,
            r#type: "a".to_owned(),
        }),
        WebPageBlock::Table(WebPageBlockTable {
            title: plain("table"),
            cells: vec![vec![
                WebPageBlockTableCell {
                    text: plain("r0c0"),
                    is_header: true,
                    align_left: true,
                    align_center: false,
                    align_right: false,
                    valign_top: false,
                    valign_middle: false,
                    valign_bottom: false,
                    colspan: 1,
                    rowspan: 1,
                },
                WebPageBlockTableCell::default(),
            ]],
            is_bordered: true,
            is_striped: true,
            is_compact: false,
        }),
        WebPageBlock::ChatLink(ChatLinkInfo {
            title: "Channel".to_owned(),
            photo: DialogPhoto::default(),
            username: "example".to_owned(),
            accent_color_id: crate::peer_color::AccentColorId::new(3),
            channel_id: crate::chat_manager::ChannelId::new(1001),
        }),
        WebPageBlock::Unsupported { version: 1 },
    ];
    for block in &blocks {
        let expected = block.clone();
        let expected_type = block.get_type();
        round_trip(
            |env, out| block.store(env, out),
            WebPageBlock::parse_block,
            move |parsed| {
                assert_eq!(parsed.get_type(), expected_type);
                assert_eq!(parsed, expected);
            },
        );
    }
}

#[test]
fn unsupported_block_version_drives_need_reget() {
    let current = WebPageBlock::Unsupported { version: 1 };
    assert!(!current.need_reget());
    let stale = WebPageBlock::Unsupported { version: 0 };
    assert!(stale.need_reget());
    let covered = WebPageBlock::Cover(Box::new(stale));
    assert!(covered.need_reget());
}

#[test]
fn heading_size_clamps_on_parse() {
    round_trip(
        |env, out| {
            // hand-craft a Heading with size 9 (out of the 1..=6 range);
            // the empty flag block precedes the body like the C++ store
            crate::store_parse::store_i32(out, BlockType::Heading as i32);
            crate::store_parse::store_flags(out, []);
            plain("x").store(env, out);
            crate::store_parse::store_i32(out, 9);
        },
        WebPageBlock::parse_block,
        |parsed| match parsed {
            WebPageBlock::Heading { size, .. } => assert_eq!(size, 6),
            other => panic!("wrong block {other:?}"),
        },
    );
}

#[test]
fn unknown_block_type_rejected() {
    let mut env = TestEnv::default();
    let mut out = Vec::new();
    crate::store_parse::store_i32(&mut out, 37); // == BlockType::Size
    let mut parser = Parser::new(&out);
    let error = WebPageBlock::parse_block(&mut env, &mut parser).unwrap_err();
    assert_eq!(error, "Can't parse unknown BlockType");
}

#[test]
fn rich_text_object_plain_and_anchor_two_pass() {
    // the two-pass anchor resolution: an Anchor named "a1" and a URL
    // "#a1" pointing at it resolve into richTextAnchorLink/richTextReference
    let blocks = vec![
        WebPageBlock::Anchor {
            name: "a1".to_owned(),
        },
        WebPageBlock::Paragraph(RichText {
            r#type: RichTextType::Url,
            content: "https://example.com/page#a1".to_owned(),
            texts: vec![plain("go")],
            ..RichText::default()
        }),
    ];
    let mut env = TestEnv::default();
    let mut user_manager = TestUserManager;
    let objects = get_page_blocks_object(
        &blocks,
        &mut env,
        &mut user_manager,
        "https://example.com/page",
        "",
        true,
    );
    assert_eq!(objects.len(), 2);
    // the anchor object + the resolved url object
    let td_api::PageBlock::pageBlockParagraph(paragraph) = objects[1].as_ref() else {
        panic!("expected the paragraph block");
    };
    let Some(text) = paragraph.text.as_deref() else {
        panic!("expected the paragraph text");
    };
    match text {
        td_api::RichText::richTextAnchorLink(link) => {
            assert_eq!(link.anchor_name, "a1");
            assert_eq!(link.url, "https://example.com/page#a1");
        }
        other => panic!("expected the anchor link resolution, got {other:?}"),
    }
}

/// The minimal `UserInputEnv` for the object builders.
struct TestUserManager;

impl crate::inline_keyboard_button::InlineKeyboardButtonEnv for TestUserManager {
    fn is_test_dc(&self) -> bool {
        false
    }
}

impl crate::inline_keyboard_button::UserInputEnv for TestUserManager {
    fn get_input_user(
        &mut self,
        _user_id: UserId,
    ) -> std::result::Result<tdmtproto::telegram_api::InputUser, tdutils::status::Status> {
        Ok(tdmtproto::telegram_api::InputUser::inputUserEmpty(
            tdmtproto::telegram_api::inputUserEmpty {},
        ))
    }

    fn is_my_user_bot(&self) -> bool {
        false
    }

    fn get_user_id_object(&mut self, user_id: UserId, _source: &str) -> i64 {
        user_id.get()
    }
}

#[test]
fn wire_rich_text_intake_text_url() {
    let documents = DocumentsById::new();
    let wire = tdmtproto::telegram_api::RichText::textUrl(tdmtproto::telegram_api::textUrl {
        text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
            tdmtproto::telegram_api::textPlain {
                text: "click".to_owned(),
            },
        ))),
        url: "https://example.com".to_owned(),
        webpage_id: 4242,
    });
    let rich_text = get_rich_text_from_telegram(wire, &documents);
    assert_eq!(rich_text.r#type, RichTextType::Url);
    assert_eq!(rich_text.content, "https://example.com");
    assert_eq!(rich_text.web_page_id, WebPageId::new(4242));
    assert_eq!(rich_text.texts.len(), 1);
    assert_eq!(rich_text.get_full_text(), "click");
}

#[test]
fn wire_rich_text_intake_icon() {
    let mut documents = DocumentsById::new();
    documents.insert(555, FileId::new(9, 0));
    let wire = tdmtproto::telegram_api::RichText::textImage(tdmtproto::telegram_api::textImage {
        document_id: 555,
        w: 640,
        h: 480,
    });
    let rich_text = get_rich_text_from_telegram(wire, &documents);
    assert_eq!(rich_text.r#type, RichTextType::Icon);
    assert_eq!(rich_text.document_file_id, FileId::new(9, 0));
    // content packs width * 65536 + height
    assert_eq!(rich_text.content, (640u32 * 65536 + 480).to_string());
}

#[test]
fn get_rich_text_from_td_api_validation() {
    let mut env = TestEnv::default();
    // bad UTF-8 is impossible through String; the emoji-id validation
    // and the icon ban are the observable checks
    let error = get_rich_text_from_td_api(
        &mut env,
        Some(Box::new(td_api::RichText::richTextCustomEmoji(
            td_api::richTextCustomEmoji {
                custom_emoji_id: 0,
                alternative_text: "alt".to_owned(),
            },
        ))),
    )
    .unwrap_err();
    assert_eq!(error.code(), 400);
    assert_eq!(error.message(), "Invalid custom emoji identifier specified");

    let error = get_rich_text_from_td_api(
        &mut env,
        Some(Box::new(td_api::RichText::richTextIcon(
            td_api::richTextIcon {
                document: None,
                width: 1,
                height: 1,
            },
        ))),
    )
    .unwrap_err();
    assert_eq!(error.message(), "Block is not allowed");

    // the mention passthrough collapses to the inner text
    let mention = get_rich_text_from_td_api(
        &mut env,
        Some(Box::new(td_api::RichText::richTextMention(
            td_api::richTextMention {
                text: Some(Box::new(td_api::RichText::richTextPlain(
                    td_api::richTextPlain {
                        text: "@user".to_owned(),
                    },
                ))),
                username: "@user".to_owned(),
            },
        ))),
    )
    .unwrap();
    assert_eq!(mention.r#type, RichTextType::Plain);
    assert_eq!(mention.content, "@user");
}

#[test]
fn get_index_mask_over_url_family() {
    let mut url = plain("");
    url.r#type = RichTextType::Url;
    url.content = "https://example.com".to_owned();
    let url_mask = crate::dialog_id::message_search_filter_index_mask(
        crate::dialog_id::MessageSearchFilter::Url,
    );
    assert_eq!(url.get_index_mask(), url_mask);
    // anchor urls index nothing
    url.content = "#anchor".to_owned();
    assert_eq!(url.get_index_mask(), 0);
    // hashtags/plain index nothing
    assert_eq!(plain("#tag").get_index_mask(), 0);
}

#[test]
fn inline_keyboard_button_store_parse_round_trip() {
    let button = InlineKeyboardButton {
        type_: InlineKeyboardButtonType::SwitchInline,
        id: TargetDialogTypes::from_mask(0).get_mask(),
        user_id: UserId::default(),
        style: KeyboardButtonStyle::from_td_api(None, 536870912),
        text: "switch".to_owned(),
        forward_text: String::new(),
        data: "query".to_owned(),
    };
    let mut out = Vec::new();
    button.store(&mut out);
    let mut parser = Parser::new(&out);
    let parsed = InlineKeyboardButton::parse(&mut parser).unwrap();
    parser.fetch_end().unwrap();
    assert_eq!(parsed, button);
}

#[test]
fn get_inline_keyboard_button_from_td_api_url() {
    let button = get_inline_keyboard_button_from_td_api(
        td_api::InlineKeyboardButton {
            text: "go".to_owned(),
            icon_custom_emoji_id: 0,
            style: None,
            r#type: Some(
                td_api::InlineKeyboardButtonType::inlineKeyboardButtonTypeUrl(
                    td_api::inlineKeyboardButtonTypeUrl {
                        url: "https://example.com".to_owned(),
                    },
                ),
            ),
        },
        true,
        &TestUserManager,
    )
    .unwrap();
    assert_eq!(button.type_, InlineKeyboardButtonType::Url);
    // the ported check_link normalizes the host form with the trailing slash
    assert_eq!(button.data, "https://example.com/");
    assert_eq!(button.text, "go");
}

#[test]
fn wire_page_block_intake_paragraph_and_list() {
    let mut env = TestEnv::default();
    let empty = DocumentsById::new();
    let photos = PhotosById::new();

    let paragraph = get_web_page_block(
        &mut env,
        tdmtproto::telegram_api::PageBlock::pageBlockParagraph(
            tdmtproto::telegram_api::pageBlockParagraph {
                text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
                    tdmtproto::telegram_api::textPlain {
                        text: "para".to_owned(),
                    },
                ))),
            },
        ),
        &empty,
        &empty,
        &empty,
        &photos,
        &empty,
        &empty,
    )
    .unwrap();
    match paragraph {
        WebPageBlock::Paragraph(text) => assert_eq!(text.content, "para"),
        other => panic!("wrong block {other:?}"),
    }

    let list = get_web_page_block(
        &mut env,
        tdmtproto::telegram_api::PageBlock::pageBlockList(tdmtproto::telegram_api::pageBlockList {
            items: vec![Box::new(
                tdmtproto::telegram_api::PageListItem::pageListItemText(
                    tdmtproto::telegram_api::pageListItemText {
                        flags: 0,
                        checkbox: true,
                        checked: false,
                        text: Some(Box::new(tdmtproto::telegram_api::RichText::textPlain(
                            tdmtproto::telegram_api::textPlain {
                                text: "item".to_owned(),
                            },
                        ))),
                    },
                ),
            )],
        }),
        &empty,
        &empty,
        &empty,
        &photos,
        &empty,
        &empty,
    )
    .unwrap();
    match list {
        WebPageBlock::List(list) => {
            assert_eq!(list.items.len(), 1);
            assert!(list.items[0].has_checkbox);
            assert!(!list.items[0].is_checked);
            assert_eq!(list.items[0].label, "");
        }
        other => panic!("wrong block {other:?}"),
    }

    // the unsupported constructor drops to None -> the vector wraps it
    let blocks = get_web_page_blocks_from_telegram(
        &mut env,
        vec![Box::new(
            tdmtproto::telegram_api::PageBlock::pageBlockUnsupported(
                tdmtproto::telegram_api::pageBlockUnsupported {},
            ),
        )],
        &empty,
        &empty,
        &empty,
        &photos,
        &empty,
        &empty,
    );
    assert_eq!(blocks.len(), 1);
    assert!(matches!(
        blocks[0],
        WebPageBlock::Unsupported { version: 1 }
    ));
}

#[test]
fn version_constants_match_the_cxx_enum() {
    use crate::store_parse::version as v;
    assert_eq!(v::FIX_WEB_PAGE_INSTANT_VIEW_DATABASE, 9);
    assert_eq!(v::FIX_PAGE_BLOCK_AUDIO_EMPTY_FILE, 11);
    assert_eq!(v::SUPPORT_INSTANT_VIEW_2_0, 18);
    assert_eq!(v::ADD_KEYBOARD_BUTTON_FLAGS, 35);
    assert_eq!(v::ADD_PAGE_BLOCK_CHAT_LINK_FLAGS, 50);
    assert_eq!(v::SUPPORT_RICH_MESSAGES, 60);
    assert_eq!(v::NEXT, 62);
}

#[test]
fn log_event_version_framing_still_pins() {
    // the store/parse of the blocks is written behind the log-event
    // version prefix in production; pin the framing here
    let mut out = Vec::new();
    crate::store_parse::store_log_event_version(&mut out);
    let mut parser = Parser::new_log_event(&out).unwrap();
    assert_eq!(parser.version(), version::LOG_EVENT_STORE_VERSION);
    assert_eq!(version::LOG_EVENT_STORE_VERSION, version::NEXT - 1);
    let _ = &mut parser;
}
