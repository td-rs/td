//! `td::BusinessChatLink` + `BusinessChatLinks` (BusinessChatLink.h/.cpp,
//! M.48). The link text rides the message-entity plane through the env
//! seam (`get_message_text` / `get_formatted_text_object`).

use crate::business_manager::BusinessEnv;
use crate::td_api;
use tdmtproto::telegram_api;

/// `td::BusinessChatLink` (BusinessChatLink.h:13-31).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BusinessChatLink {
    link: String,
    title: String,
    view_count: i32,
    text: Option<td_api::FormattedText>,
}

impl BusinessChatLink {
    /// `BusinessChatLink(businessChatLink)` (BusinessChatLink.cpp:11-17).
    pub fn from_telegram(
        env: &mut impl BusinessEnv,
        link: telegram_api::BusinessChatLink,
    ) -> BusinessChatLink {
        BusinessChatLink {
            link: link.link,
            title: link.title,
            view_count: link.views,
            text: env.get_message_text(link.message, Some(link.entities), "BusinessChatLink"),
        }
    }

    /// `BusinessChatLink::is_valid` (BusinessChatLink.h:21-23).
    pub fn is_valid(&self) -> bool {
        !self.link.is_empty()
    }

    /// `get_business_chat_link_object` (BusinessChatLink.cpp:19-23).
    pub fn get_business_chat_link_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessChatLink {
        td_api::BusinessChatLink {
            link: self.link.clone(),
            text: env.get_formatted_text_object(&self.text, true, -1),
            title: self.title.clone(),
            view_count: self.view_count,
        }
    }
}

impl std::fmt::Display for BusinessChatLink {
    /// `operator<<` (BusinessChatLink.cpp:25-27).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} {} {}]", self.link, self.title, self.view_count)
    }
}

/// `td::BusinessChatLinks` (BusinessChatLink.h:33-44).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BusinessChatLinks {
    business_chat_links: Vec<BusinessChatLink>,
}

impl BusinessChatLinks {
    /// `BusinessChatLinks(businessChatLink[])` (BusinessChatLink.cpp:29-39)
    /// — the invalid links are dropped with an error log.
    pub fn from_telegram(
        env: &mut impl BusinessEnv,
        links: Vec<telegram_api::BusinessChatLink>,
    ) -> BusinessChatLinks {
        let mut result = BusinessChatLinks::default();
        for link in links {
            let business_chat_link = BusinessChatLink::from_telegram(env, link);
            if !business_chat_link.is_valid() {
                tdutils::log_error!("Receive invalid {business_chat_link} business link");
                continue;
            }
            result.business_chat_links.push(business_chat_link);
        }
        result
    }

    /// `get_business_chat_links_object` (BusinessChatLink.cpp:41-45).
    pub fn get_business_chat_links_object(
        &self,
        env: &mut impl BusinessEnv,
    ) -> td_api::BusinessChatLinks {
        td_api::BusinessChatLinks {
            links: self
                .business_chat_links
                .iter()
                .map(|link| link.get_business_chat_link_object(env))
                .collect(),
        }
    }
}

impl std::fmt::Display for BusinessChatLinks {
    /// `operator<<` (BusinessChatLink.cpp:47-49).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.business_chat_links)
    }
}
