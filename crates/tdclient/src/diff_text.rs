//! The `DiffText` port (DiffText.{h,cpp}) — the proofread-AI diff value
//! class: the server answers a proofread `messages.composeMessageWithAI`
//! with a `diff_text` carrying `messageEntityDiff*` entities, which this
//! class converts into the `td_api::diffText` answer
//! (`ProofreadMessageWithAiQuery::on_result`, TranslationManager.cpp:559).

use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::utf8::utf8_utf16_length;

/// `DiffText::DiffEntity::Type` (DiffText.h:19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffEntityType {
    Insert,
    Replace,
    Delete,
}

/// `DiffText::DiffEntity` (DiffText.h:17-33).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffEntity {
    type_: DiffEntityType,
    offset: i32,
    length: i32,
    old_text: String,
}

impl DiffEntity {
    /// `DiffEntity::get_diff_entity_type_object` (DiffText.cpp:15-27).
    fn get_diff_entity_type_object(&self) -> td_api::DiffEntityType {
        match self.type_ {
            DiffEntityType::Insert => {
                td_api::DiffEntityType::diffEntityTypeInsert(td_api::diffEntityTypeInsert)
            }
            DiffEntityType::Replace => {
                td_api::DiffEntityType::diffEntityTypeReplace(td_api::diffEntityTypeReplace {
                    old_text: self.old_text.clone(),
                })
            }
            DiffEntityType::Delete => {
                td_api::DiffEntityType::diffEntityTypeDelete(td_api::diffEntityTypeDelete)
            }
        }
    }

    /// `DiffEntity::get_diff_entity_object` (DiffText.cpp:29-31).
    fn get_diff_entity_object(&self) -> td_api::DiffEntity {
        td_api::DiffEntity {
            offset: self.offset,
            length: self.length,
            r#type: Some(self.get_diff_entity_type_object()),
        }
    }
}

/// `DiffText` (DiffText.h:16-46) — the text plus its diff entities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DiffText {
    text: String,
    entities: Vec<DiffEntity>,
}

impl DiffText {
    /// `DiffText::check_entities` (DiffText.cpp:33-48) — the entities
    /// must not intersect and must not begin or end in the middle of
    /// characters.
    fn check_entities(&self) -> bool {
        let text_len = utf8_utf16_length(&self.text) as i32;
        let mut cur_pos = 0;
        for entity in &self.entities {
            if entity.offset < cur_pos {
                return false;
            }
            if entity.length < 0 || entity.length > text_len - entity.offset {
                return false;
            }
            cur_pos = entity.offset + entity.length;
        }
        true
    }

    /// `DiffText::DiffText` (DiffText.cpp:50-89) — `None` stays the
    /// default (the "Receive no diff text" band); the non-diff entities
    /// log and drop, an invalid layout clears the entity list, and the
    /// valid tail drops the empty-length entities.
    pub fn new(text_with_entities: Option<telegram_api::TextWithEntities>) -> Self {
        let Some(text_with_entities) = text_with_entities else {
            tdutils::log_error!("Receive no diff text");
            return Self::default();
        };

        let mut result = Self {
            text: text_with_entities.text,
            entities: Vec::with_capacity(text_with_entities.entities.len()),
        };
        for server_entity in text_with_entities.entities {
            match server_entity {
                telegram_api::MessageEntity::messageEntityUnknown(_) => {}
                telegram_api::MessageEntity::messageEntityDiffInsert(entity) => {
                    result.entities.push(DiffEntity {
                        type_: DiffEntityType::Insert,
                        offset: entity.offset,
                        length: entity.length,
                        old_text: String::new(),
                    });
                }
                telegram_api::MessageEntity::messageEntityDiffReplace(entity) => {
                    result.entities.push(DiffEntity {
                        type_: DiffEntityType::Replace,
                        offset: entity.offset,
                        length: entity.length,
                        old_text: entity.old_text,
                    });
                }
                telegram_api::MessageEntity::messageEntityDiffDelete(entity) => {
                    result.entities.push(DiffEntity {
                        type_: DiffEntityType::Delete,
                        offset: entity.offset,
                        length: entity.length,
                        old_text: String::new(),
                    });
                }
                other => {
                    tdutils::log_error!("Receive diff entity {}", other.to_debug_string());
                }
            }
        }

        if !result.check_entities() {
            tdutils::log_error!("Receive invalid diff entities");
            result.entities.clear();
        } else {
            result.entities.retain(|entity| entity.length > 0);
        }
        result
    }

    /// `DiffText::get_diff_text_object` (DiffText.cpp:91-95).
    pub fn get_diff_text_object(&self) -> td_api::DiffText {
        td_api::DiffText {
            text: self.text.clone(),
            entities: self
                .entities
                .iter()
                .map(|entity| entity.get_diff_entity_object())
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(offset: i32, length: i32) -> telegram_api::MessageEntity {
        telegram_api::MessageEntity::messageEntityDiffInsert(
            telegram_api::messageEntityDiffInsert { offset, length },
        )
    }

    fn replace(offset: i32, length: i32, old_text: &str) -> telegram_api::MessageEntity {
        telegram_api::MessageEntity::messageEntityDiffReplace(
            telegram_api::messageEntityDiffReplace {
                offset,
                length,
                old_text: old_text.to_owned(),
            },
        )
    }

    fn delete(offset: i32, length: i32) -> telegram_api::MessageEntity {
        telegram_api::MessageEntity::messageEntityDiffDelete(
            telegram_api::messageEntityDiffDelete { offset, length },
        )
    }

    /// The `DiffText` ctor (DiffText.cpp:50-89) over a non-empty answer.
    #[test]
    fn diff_text_converts_the_diff_entities() {
        let diff = DiffText::new(Some(telegram_api::TextWithEntities {
            text: "hello world".to_owned(),
            entities: vec![
                insert(0, 2),
                delete(2, 1),
                replace(6, 5, "world"),
                telegram_api::MessageEntity::messageEntityUnknown(
                    telegram_api::messageEntityUnknown {
                        offset: 0,
                        length: 0,
                    },
                ),
            ],
        }));
        assert_eq!(diff.text, "hello world");
        assert_eq!(diff.entities.len(), 3, "the unknown entity drops");
        // the object conversion carries the type triple (DiffText.cpp:91-95)
        let object = diff.get_diff_text_object();
        assert_eq!(object.text, "hello world");
        assert_eq!(object.entities.len(), 3);
        assert!(matches!(
            object.entities[0].r#type,
            Some(td_api::DiffEntityType::diffEntityTypeInsert(_))
        ));
        assert!(matches!(
            object.entities[1].r#type,
            Some(td_api::DiffEntityType::diffEntityTypeDelete(_))
        ));
        assert!(matches!(
            object.entities[2].r#type,
            Some(td_api::DiffEntityType::diffEntityTypeReplace(_))
        ));
    }

    /// The null band (DiffText.cpp:54-57) — the default empty diff.
    #[test]
    fn diff_text_null_band_stays_empty() {
        let diff = DiffText::new(None);
        assert_eq!(diff, DiffText::default());
        let object = diff.get_diff_text_object();
        assert_eq!(object.text, "");
        assert!(object.entities.is_empty());
    }

    /// The invalid-layout band (DiffText.cpp:83-85) — intersecting
    /// entities clear the list; the empty-length band drops them.
    #[test]
    fn diff_text_rejects_an_invalid_layout() {
        // intersecting pair (offset 0 + length 5, then offset 3)
        let diff = DiffText::new(Some(telegram_api::TextWithEntities {
            text: "hello world".to_owned(),
            entities: vec![insert(0, 5), delete(3, 2)],
        }));
        assert!(diff.entities.is_empty());

        // the empty-length tail drops on the valid layout
        let diff = DiffText::new(Some(telegram_api::TextWithEntities {
            text: "hello world".to_owned(),
            entities: vec![insert(0, 0), replace(2, 3, "abc")],
        }));
        assert_eq!(diff.entities.len(), 1);
    }
}
