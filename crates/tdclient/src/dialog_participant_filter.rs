//! The participant-filter plane: `DialogParticipantFilter.{h,cpp}` +
//! `ChannelParticipantFilter.{h,cpp}` — the td_api `ChatMembersFilter*` /
//! `SupergroupMembersFilter*` decision tables used by searchChatMembers and
//! getSupergroupMembers.

use crate::chat_manager::DialogParticipant;
use crate::chat_manager::DialogType;
use crate::chat_manager::DialogId;
use crate::user_manager::UserId;
use crate::td_api;

/// The `MessageTopic` carried by the Mention filters — the generated
/// `td_api::MessageTopic` value. Only the wire queries read it
/// ([blocked: net]); the offline suitability tables never do.
pub type TopicId = td_api::MessageTopic;

/// `td::DialogParticipantFilter` (DialogParticipantFilter.h:19-25).
#[derive(Clone, Debug, PartialEq)]
pub enum DialogParticipantFilter {
    Contacts,
    Administrators,
    Members,
    Restricted,
    Banned,
    /// `chatMembersFilterMention` — carries the topic when
    /// `MessageTopic::get_message_topic` succeeded (DialogParticipantFilter.cpp:68-75;
    /// a resolution failure is swallowed, the type stays Mention either way).
    Mention {
        message_topic: Option<TopicId>,
    },
    Bots,
}

impl DialogParticipantFilter {
    /// `DialogParticipantFilter::DialogParticipantFilter(Td *, DialogId,
    /// td_api::ChatMembersFilter)` (DialogParticipantFilter.cpp:40-73). A null
    /// filter is `Type::Members`; an unknown topic id only leaves the topic
    /// unresolved, never fails the request.
    pub fn from_td_api(
        _dialog_id: DialogId,
        filter: Option<&td_api::ChatMembersFilter>,
    ) -> DialogParticipantFilter {
        use td_api::ChatMembersFilter as F;
        match filter {
            None => DialogParticipantFilter::Members,
            Some(F::chatMembersFilterContacts(_)) => DialogParticipantFilter::Contacts,
            Some(F::chatMembersFilterAdministrators(_)) => DialogParticipantFilter::Administrators,
            Some(F::chatMembersFilterMembers(_)) => DialogParticipantFilter::Members,
            Some(F::chatMembersFilterRestricted(_)) => DialogParticipantFilter::Restricted,
            Some(F::chatMembersFilterBanned(_)) => DialogParticipantFilter::Banned,
            Some(F::chatMembersFilterMention(mention)) => {
                // `MessageTopic::get_message_topic(td, dialog_id,
                // mention.topic_id)` — the full thread/forum/direct-messages
                // resolution machine [blocked: MessageTopic plane]; the C++
                // swallows its errors here, so leaving the topic unresolved
                // is offline-identical.
                DialogParticipantFilter::Mention {
                    message_topic: mention.topic_id.clone(),
                }
            }
            Some(F::chatMembersFilterBots(_)) => DialogParticipantFilter::Bots,
        }
    }

    /// `DialogParticipantFilter::as_channel_participant_filter`
    /// (DialogParticipantFilter.cpp:75-116).
    pub fn as_channel_participant_filter(&self, query: &str) -> ChannelParticipantFilter {
        let mut filter = match self {
            DialogParticipantFilter::Contacts => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Contacts,
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Administrators => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Administrators,
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Members => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Search,
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Restricted => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Restricted,
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Banned => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Banned,
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Mention { message_topic } => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Mention,
                message_topic: message_topic.clone(),
                ..ChannelParticipantFilter::bare()
            },
            DialogParticipantFilter::Bots => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Bots,
                ..ChannelParticipantFilter::bare()
            },
        };
        // every query-bearing type takes the caller's query
        // (ChannelParticipantFilter.cpp assigns `filter.query_ = query` per arm)
        if !matches!(
            self,
            DialogParticipantFilter::Administrators | DialogParticipantFilter::Bots
        ) {
            filter.query = query.to_string();
        }
        filter
    }

    /// `DialogParticipantFilter::has_query` (DialogParticipantFilter.cpp:118-135).
    pub fn has_query(&self) -> bool {
        !matches!(
            self,
            DialogParticipantFilter::Administrators | DialogParticipantFilter::Bots
        )
    }

    /// `DialogParticipantFilter::is_dialog_participant_suitable`
    /// (DialogParticipantFilter.cpp:137-163).
    pub fn is_dialog_participant_suitable(
        &self,
        env: &dyn DialogParticipantFilterEnv,
        participant: &DialogParticipant,
    ) -> bool {
        match self {
            DialogParticipantFilter::Contacts => {
                participant.dialog_id_.get_type() == DialogType::User
                    && env.is_user_contact(participant.dialog_id_.get_user_id())
            }
            DialogParticipantFilter::Administrators => participant.status_.is_administrator(),
            DialogParticipantFilter::Members => participant.status_.is_member(),
            DialogParticipantFilter::Restricted => participant.status_.is_restricted(),
            DialogParticipantFilter::Banned => participant.status_.is_banned(),
            DialogParticipantFilter::Mention { .. } => true,
            DialogParticipantFilter::Bots => {
                participant.dialog_id_.get_type() == DialogType::User
                    && env.is_user_bot(participant.dialog_id_.get_user_id())
            }
        }
    }
}

/// The `const Td *td` slice `is_dialog_participant_suitable` reads.
pub trait DialogParticipantFilterEnv {
    /// `user_manager_->is_user_contact(user_id)`.
    fn is_user_contact(&self, user_id: UserId) -> bool;
    /// `user_manager_->is_user_bot(user_id)`.
    fn is_user_bot(&self, user_id: UserId) -> bool;
}

/// `td::ChannelParticipantFilter::Type` (ChannelParticipantFilter.h:16-17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelParticipantFilterType {
    Recent,
    Contacts,
    Administrators,
    Search,
    Mention,
    Restricted,
    Banned,
    Bots,
}

/// `td::ChannelParticipantFilter` (ChannelParticipantFilter.h:14-46).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelParticipantFilter {
    pub(crate) filter_type: ChannelParticipantFilterType,
    pub(crate) query: String,
    pub(crate) message_topic: Option<TopicId>,
}

impl ChannelParticipantFilter {
    fn bare() -> ChannelParticipantFilter {
        ChannelParticipantFilter {
            filter_type: ChannelParticipantFilterType::Recent,
            query: String::new(),
            message_topic: None,
        }
    }

    /// `ChannelParticipantFilter::ChannelParticipantFilter(Td *, DialogId,
    /// td_api::SupergroupMembersFilter)` (ChannelParticipantFilter.cpp:24-64) —
    /// the query string rides every query-bearing arm, like the C++
    /// per-variant `filter.query_ = query` stores.
    pub fn from_td_api(
        _dialog_id: DialogId,
        filter: Option<&td_api::SupergroupMembersFilter>,
    ) -> ChannelParticipantFilter {
        use td_api::SupergroupMembersFilter as F;
        match filter {
            None => ChannelParticipantFilter::recent(),
            Some(F::supergroupMembersFilterRecent(_)) => ChannelParticipantFilter::recent(),
            Some(F::supergroupMembersFilterContacts(f)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Contacts,
                query: f.query.clone(),
                ..ChannelParticipantFilter::bare()
            },
            Some(F::supergroupMembersFilterAdministrators(_)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Administrators,
                ..ChannelParticipantFilter::bare()
            },
            Some(F::supergroupMembersFilterSearch(f)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Search,
                query: f.query.clone(),
                ..ChannelParticipantFilter::bare()
            },
            Some(F::supergroupMembersFilterMention(f)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Mention,
                query: f.query.clone(),
                // the topic resolution rides the same [blocked: MessageTopic]
                // plane as the DialogParticipantFilter::Mention arm
                message_topic: f.topic_id.clone(),
            },
            Some(F::supergroupMembersFilterRestricted(f)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Restricted,
                query: f.query.clone(),
                ..ChannelParticipantFilter::bare()
            },
            Some(F::supergroupMembersFilterBanned(f)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Banned,
                query: f.query.clone(),
                ..ChannelParticipantFilter::bare()
            },
            Some(F::supergroupMembersFilterBots(_)) => ChannelParticipantFilter {
                filter_type: ChannelParticipantFilterType::Bots,
                ..ChannelParticipantFilter::bare()
            },
        }
    }

    /// `ChannelParticipantFilter::recent` — the static recent filter.
    pub fn recent() -> ChannelParticipantFilter {
        ChannelParticipantFilter {
            filter_type: ChannelParticipantFilterType::Recent,
            ..ChannelParticipantFilter::bare()
        }
    }

    pub fn is_administrators(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Administrators
    }
    pub fn is_bots(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Bots
    }
    pub fn is_recent(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Recent
    }
    pub fn is_contacts(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Contacts
    }
    pub fn is_search(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Search
    }
    pub fn is_restricted(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Restricted
    }
    pub fn is_banned(&self) -> bool {
        self.filter_type == ChannelParticipantFilterType::Banned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_manager::DialogParticipantStatus;

    struct TestEnv {
        contacts: Vec<i64>,
        bots: Vec<i64>,
    }

    impl DialogParticipantFilterEnv for TestEnv {
        fn is_user_contact(&self, user_id: UserId) -> bool {
            self.contacts.contains(&user_id.get())
        }
        fn is_user_bot(&self, user_id: UserId) -> bool {
            self.bots.contains(&user_id.get())
        }
    }

    fn user_participant(user_id: i64, status: DialogParticipantStatus) -> DialogParticipant {
        let mut participant =
            DialogParticipant::private_member(UserId::new(user_id), UserId::new(0));
        participant.status_ = status;
        participant
    }

    #[test]
    fn null_filter_is_members() {
        let filter = DialogParticipantFilter::from_td_api(DialogId::new(0), None);
        assert_eq!(filter, DialogParticipantFilter::Members);
        assert!(filter.has_query());
        // Members -> the channel Search filter with the carried query
        let channel_filter = filter.as_channel_participant_filter("abc");
        assert_eq!(
            channel_filter.filter_type,
            ChannelParticipantFilterType::Search
        );
        assert_eq!(channel_filter.query, "abc");
    }

    #[test]
    fn generated_filters_map_by_ctor() {
        let filter = DialogParticipantFilter::from_td_api(
            DialogId::new(0),
            Some(&td_api::ChatMembersFilter::chatMembersFilterContacts(
                td_api::chatMembersFilterContacts {},
            )),
        );
        assert_eq!(filter, DialogParticipantFilter::Contacts);
        assert!(!filter.as_channel_participant_filter("").query.is_empty());

        let filter = DialogParticipantFilter::from_td_api(
            DialogId::new(0),
            Some(&td_api::ChatMembersFilter::chatMembersFilterAdministrators(
                td_api::chatMembersFilterAdministrators {},
            )),
        );
        assert_eq!(filter, DialogParticipantFilter::Administrators);
        assert!(!filter.has_query());
        // Administrators -> the channel Administrators filter with no query
        assert_eq!(
            filter.as_channel_participant_filter("ignored").query,
            String::new()
        );
    }

    #[test]
    fn mention_carries_topic_and_query() {
        let filter = DialogParticipantFilter::from_td_api(
            DialogId::new(0),
            Some(&td_api::ChatMembersFilter::chatMembersFilterMention(
                td_api::chatMembersFilterMention {
                    topic_id: Some(td_api::MessageTopic::messageTopicForum(
                        td_api::messageTopicForum {
                            forum_topic_id: 7,
                        },
                    )),
                },
            )),
        );
        assert!(matches!(filter, DialogParticipantFilter::Mention { .. }));
        assert!(filter.has_query());
        let channel_filter = filter.as_channel_participant_filter("q");
        assert_eq!(
            channel_filter.filter_type,
            ChannelParticipantFilterType::Mention
        );
        assert_eq!(channel_filter.query, "q");
        assert!(channel_filter.message_topic.is_some());
    }

    #[test]
    fn suitability_table() {
        let env = TestEnv {
            contacts: vec![1],
            bots: vec![2],
        };
        let member = user_participant(1, DialogParticipantStatus::member(0, String::new()));
        let banned = user_participant(2, DialogParticipantStatus::banned(0, String::new()));

        assert!(DialogParticipantFilter::Contacts.is_dialog_participant_suitable(&env, &member));
        assert!(!DialogParticipantFilter::Contacts.is_dialog_participant_suitable(&env, &banned));
        assert!(DialogParticipantFilter::Bots.is_dialog_participant_suitable(&env, &banned));
        assert!(DialogParticipantFilter::Members.is_dialog_participant_suitable(&env, &member));
        assert!(!DialogParticipantFilter::Members.is_dialog_participant_suitable(&env, &banned));
        assert!(DialogParticipantFilter::Banned.is_dialog_participant_suitable(&env, &banned));
        assert!(DialogParticipantFilter::Mention { message_topic: None }
            .is_dialog_participant_suitable(&env, &member));
    }

    #[test]
    fn channel_filter_recent_default() {
        let filter = ChannelParticipantFilter::from_td_api(DialogId::new(0), None);
        assert!(filter.is_recent());
        assert!(!filter.is_administrators());

        let filter = ChannelParticipantFilter::from_td_api(
            DialogId::new(0),
            Some(&td_api::SupergroupMembersFilter::supergroupMembersFilterRecent(
                td_api::supergroupMembersFilterRecent {},
            )),
        );
        assert!(filter.is_recent());

        let filter = ChannelParticipantFilter::from_td_api(
            DialogId::new(0),
            Some(&td_api::SupergroupMembersFilter::supergroupMembersFilterSearch(
                td_api::supergroupMembersFilterSearch {
                    query: "x".to_owned(),
                },
            )),
        );
        assert!(filter.is_search());
        assert_eq!(filter.query, "x");
    }
}
