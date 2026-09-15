//! Port of `td/telegram/UserNames.{h,cpp}`: the ordered username set of a
//! user — active usernames, disabled usernames and the distinguished
//! editable username — with the td_api object conversion and the
//! byte-exact database serialization (`store`/`parse` live in the C++
//! header).
//!
//! The constructor's rule set (duplicate/empty rejection, the single
//! editable username invariant, editable-disabled tracking) mirrors
//! `Usernames::Usernames` verbatim; violations fall back to `check_validness`
//! resetting the object to empty, exactly like the C++ `LOG(ERROR)` path.

use crate::td_api;
use tdmtproto::telegram_api;
use tdutils::utf8::check_utf8;

/// `td::Usernames`.
#[derive(Clone, Debug)]
pub struct Usernames {
    active_usernames: Vec<String>,
    disabled_usernames: Vec<String>,
    editable_username_pos: i32,
    is_editable_username_disabled: bool,
    other_editable_usernames: Vec<String>,
}

impl Default for Usernames {
    /// `editable_username_pos == -1` is the "no editable username" state
    /// the C++ constructor initializes to; the derived 0 would break
    /// `has_editable_username`.
    fn default() -> Self {
        Usernames {
            active_usernames: Vec::new(),
            disabled_usernames: Vec::new(),
            editable_username_pos: -1,
            is_editable_username_disabled: false,
            other_editable_usernames: Vec::new(),
        }
    }
}

impl PartialEq for Usernames {
    /// `operator==` — the other_editable_usernames are not compared (the
    /// C++ deliberately leaves them out).
    fn eq(&self, other: &Self) -> bool {
        self.active_usernames == other.active_usernames
            && self.disabled_usernames == other.disabled_usernames
            && self.editable_username_pos == other.editable_username_pos
    }
}

impl Usernames {
    /// `Usernames::Usernames(string &&first_username, vector<username>)`.
    pub fn from_telegram(
        first_username: String,
        usernames: Vec<telegram_api::Username>,
    ) -> Usernames {
        if usernames.is_empty() {
            let mut result = Usernames::default();
            if !first_username.is_empty() {
                result.active_usernames.push(first_username);
                result.editable_username_pos = 0;
            }
            return result;
        }

        if !first_username.is_empty() {
            tdutils::log_error!(
                "Receive first username {first_username} with {} usernames",
                usernames.len()
            );
            return Usernames::default();
        }
        let mut was_editable = false;
        let mut result = Usernames::default();
        let mut received_usernames = std::collections::BTreeSet::new();
        for mut username in usernames {
            if username.username.is_empty() {
                tdutils::log_error!("Receive empty username");
                continue;
            }
            if !received_usernames.insert(username.username.clone()) {
                tdutils::log_error!("Receive duplicate username");
                continue;
            }
            if username.editable {
                if was_editable {
                    username.editable = false;
                    result
                        .other_editable_usernames
                        .push(username.username.clone());
                } else {
                    was_editable = true;
                }
            }
            if username.active {
                result.active_usernames.push(username.username);
                if username.editable {
                    result.is_editable_username_disabled = false;
                    result.editable_username_pos = result.active_usernames.len() as i32 - 1;
                }
            } else {
                result.disabled_usernames.push(username.username);
                if username.editable {
                    result.is_editable_username_disabled = true;
                    result.editable_username_pos = result.disabled_usernames.len() as i32 - 1;
                }
            }
        }
        debug_assert_eq!(result.has_editable_username(), was_editable);
        result.check_validness();
        result
    }

    /// `Usernames::get_usernames_object`.
    pub fn get_usernames_object(&self) -> td_api::Usernames {
        td_api::Usernames {
            active_usernames: self.active_usernames.clone(),
            disabled_usernames: self.disabled_usernames.clone(),
            editable_username: self.get_editable_username().to_owned(),
            collectible_usernames: self
                .active_usernames
                .iter()
                .chain(self.disabled_usernames.iter())
                .filter(|username| {
                    *username != self.get_editable_username()
                        && !self.other_editable_usernames.contains(username)
                })
                .cloned()
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.editable_username_pos == -1
            && self.active_usernames.is_empty()
            && self.disabled_usernames.is_empty()
    }

    pub fn get_first_username(&self) -> &str {
        if !self.has_first_username() {
            return "";
        }
        &self.active_usernames[0]
    }

    pub fn has_first_username(&self) -> bool {
        !self.active_usernames.is_empty()
    }

    pub fn get_editable_username(&self) -> &str {
        if !self.has_editable_username() {
            return "";
        }
        if self.is_editable_username_disabled {
            &self.disabled_usernames[self.editable_username_pos as usize]
        } else {
            &self.active_usernames[self.editable_username_pos as usize]
        }
    }

    pub fn has_editable_username(&self) -> bool {
        self.editable_username_pos != -1
    }

    pub fn get_active_usernames(&self) -> &Vec<String> {
        &self.active_usernames
    }

    /// `Usernames::change_editable_username`.
    pub fn change_editable_username(&self, new_username: &str) -> Usernames {
        let mut result = self.clone();
        if self.has_editable_username() {
            if self.is_editable_username_disabled {
                return result;
            }
            if new_username.is_empty() {
                result
                    .active_usernames
                    .remove(self.editable_username_pos as usize);
                result.editable_username_pos = -1;
            } else {
                result.active_usernames[self.editable_username_pos as usize] =
                    new_username.to_owned();
            }
        } else if !new_username.is_empty() {
            result.active_usernames.insert(0, new_username.to_owned());
            result.editable_username_pos = 0;
        }
        result.check_validness();
        result
    }

    /// `Usernames::can_toggle`.
    pub fn can_toggle(&self, for_bot: bool, username: &str) -> bool {
        if self.active_usernames.iter().any(|name| name == username) {
            if !self.has_editable_username()
                || self.is_editable_username_disabled
                || self.active_usernames[self.editable_username_pos as usize] != username
            {
                // disabling of non-editable username is always allowed
                return true;
            }
            if for_bot {
                // bots can disable editable username if there is another active username
                return self.active_usernames.len() >= 2;
            }
            // otherwise, editable user can't be disabled
            return false;
        }
        if self.disabled_usernames.iter().any(|name| name == username) {
            return true;
        }
        false
    }

    /// `Usernames::toggle`.
    pub fn toggle(&self, for_bot: bool, username: &str, is_active: bool) -> Usernames {
        let mut result = self.clone();
        if !self.can_toggle(for_bot, username) {
            return result;
        }
        for i in 0..result.active_usernames.len() {
            if result.active_usernames[i] == username {
                if !is_active {
                    // disable the username
                    let name = result.active_usernames.remove(i);
                    result.disabled_usernames.insert(0, name);
                    if result.has_editable_username() {
                        if result.is_editable_username_disabled {
                            result.editable_username_pos += 1;
                            if for_bot && result.active_usernames.is_empty() {
                                // activate the previously disabled editable username
                                let pos = result.editable_username_pos as usize;
                                let editable_username = result.disabled_usernames.remove(pos);
                                result.active_usernames.push(editable_username);
                                result.is_editable_username_disabled = false;
                                result.editable_username_pos = 0;
                            }
                        } else if i <= result.editable_username_pos as usize {
                            if i == result.editable_username_pos as usize {
                                debug_assert!(for_bot);
                                // the editable username is being disabled
                                result.editable_username_pos = 0;
                                result.is_editable_username_disabled = true;
                            } else {
                                debug_assert!(result.editable_username_pos > 0);
                                result.editable_username_pos -= 1;
                            }
                        }
                    }
                }
                result.check_validness();
                return result;
            }
        }
        for i in 0..result.disabled_usernames.len() {
            if result.disabled_usernames[i] == username {
                if is_active {
                    // activate the username
                    let name = result.disabled_usernames.remove(i);
                    result.active_usernames.push(name);
                    if result.is_editable_username_disabled {
                        debug_assert!(result.has_editable_username());
                        if result.editable_username_pos as usize == i {
                            // editable username was activated
                            result.is_editable_username_disabled = false;
                            result.editable_username_pos = result.active_usernames.len() as i32 - 1;
                        } else if i <= result.editable_username_pos as usize {
                            // editable username position decreased
                            debug_assert!(result.editable_username_pos > 0);
                            result.editable_username_pos -= 1;
                        }
                    }
                }
                result.check_validness();
                return result;
            }
        }
        unreachable!("toggle of a username not owned by the user");
    }

    /// `Usernames::deactivate_all`.
    pub fn deactivate_all(&self) -> Usernames {
        let mut result = Usernames::default();
        for (i, username) in self.active_usernames.iter().enumerate() {
            if !self.is_editable_username_disabled && i as i32 == self.editable_username_pos {
                result.active_usernames.push(username.clone());
                result.editable_username_pos = 0;
            } else {
                result.disabled_usernames.push(username.clone());
            }
        }
        if self.is_editable_username_disabled {
            result.editable_username_pos =
                self.editable_username_pos + result.disabled_usernames.len() as i32;
        }
        result
            .disabled_usernames
            .extend(self.disabled_usernames.iter().cloned());
        result.check_validness();
        result
    }

    /// `Usernames::can_reorder_to`.
    pub fn can_reorder_to(&self, new_username_order: &Vec<String>) -> bool {
        if new_username_order.len() != self.active_usernames.len() {
            return false;
        }
        let mut active_usernames: std::collections::BTreeSet<&String> =
            self.active_usernames.iter().collect();
        for username in new_username_order {
            if !active_usernames.remove(username) {
                return false;
            }
        }
        debug_assert!(active_usernames.is_empty());
        true
    }

    /// `Usernames::reorder_to`.
    pub fn reorder_to(&self, new_username_order: Vec<String>) -> Usernames {
        let mut result = Usernames {
            active_usernames: new_username_order,
            disabled_usernames: self.disabled_usernames.clone(),
            ..Usernames::default()
        };
        if self.has_editable_username() {
            if self.is_editable_username_disabled {
                result.editable_username_pos = self.editable_username_pos;
            } else {
                let editable_username = &self.active_usernames[self.editable_username_pos as usize];
                for (i, username) in result.active_usernames.iter().enumerate() {
                    if username == editable_username {
                        result.editable_username_pos = i as i32;
                        break;
                    }
                }
            }
            debug_assert!(result.has_editable_username());
        }
        result.check_validness();
        result
    }

    /// `Usernames::check_validness` — violations reset the object to empty
    /// (the C++ LOG(ERROR) + `*this = Usernames()` path).
    pub fn check_validness(&mut self) {
        let mut usernames = std::collections::BTreeSet::new();
        for username in &self.active_usernames {
            if username.is_empty()
                || !check_utf8(username.as_bytes())
                || !usernames.insert(username)
            {
                tdutils::log_error!("Have invalid active username \"{username}\"");
                *self = Usernames::default();
                return;
            }
        }
        for username in &self.disabled_usernames {
            if username.is_empty()
                || !check_utf8(username.as_bytes())
                || !usernames.insert(username)
            {
                tdutils::log_error!("Have invalid disabled username \"{username}\"");
                *self = Usernames::default();
                return;
            }
        }
        if self.is_editable_username_disabled {
            if self.editable_username_pos >= 0
                && self.editable_username_pos as usize >= self.disabled_usernames.len()
            {
                tdutils::log_error!(
                    "Have invalid editable username position {}",
                    self.editable_username_pos
                );
                *self = Usernames::default();
            }
        } else if self.editable_username_pos != -1
            && self.editable_username_pos as usize >= self.active_usernames.len()
        {
            tdutils::log_error!(
                "Have invalid editable username position {}",
                self.editable_username_pos
            );
            *self = Usernames::default();
        }
    }

    /// `Usernames::store` (Usernames.h) — requires a non-empty object.
    pub fn store(&self, out: &mut Vec<u8>) {
        use crate::store_parse::*;
        assert!(!self.is_empty(), "storing empty Usernames");
        let has_many_active_usernames = self.active_usernames.len() > 1;
        let has_disabled_usernames = !self.disabled_usernames.is_empty();
        let has_editable_username = self.has_editable_username();
        let has_active_usernames = !self.active_usernames.is_empty();
        let has_other_editable_usernames = !self.other_editable_usernames.is_empty();
        store_flags(
            out,
            [
                has_many_active_usernames,
                has_disabled_usernames,
                has_editable_username,
                has_active_usernames,
                self.is_editable_username_disabled,
                has_other_editable_usernames,
            ],
        );
        if self.is_editable_username_disabled {
            assert!(has_editable_username);
            if has_active_usernames {
                store_string_vec(out, &self.active_usernames);
            }
            store_i32(out, self.editable_username_pos);
        } else if has_many_active_usernames {
            store_string_vec(out, &self.active_usernames);
            if has_editable_username {
                store_i32(out, self.editable_username_pos);
            }
        } else if has_active_usernames {
            store_string(out, &self.active_usernames[0]);
        }
        if has_disabled_usernames {
            store_string_vec(out, &self.disabled_usernames);
        }
        if has_other_editable_usernames {
            store_string_vec(out, &self.other_editable_usernames);
        }
    }

    /// `Usernames::parse` (Usernames.h).
    pub fn parse(input: &[u8]) -> Result<Usernames, &'static str> {
        let mut parser = crate::store_parse::Parser::new(input);
        let result = Usernames::parse_tail(&mut parser)?;
        parser.fetch_end()?;
        Ok(result)
    }

    /// The parse over a shared stream (a Usernames stored inline inside
    /// the `User` blob).
    pub fn parse_tail(parser: &mut crate::store_parse::Parser) -> Result<Usernames, &'static str> {
        let [
            has_many_active_usernames,
            has_disabled_usernames,
            has_editable_username,
            has_active_usernames,
            is_editable_username_disabled,
            has_other_editable_usernames,
        ] = parser.parse_flags()?;
        let mut result = Usernames {
            is_editable_username_disabled,
            ..Usernames::default()
        };
        if is_editable_username_disabled {
            if has_active_usernames {
                result.active_usernames = parser.string_vec()?;
            }
            result.editable_username_pos = parser.i32_()?;
        } else if has_many_active_usernames {
            result.active_usernames = parser.string_vec()?;
            if has_editable_username {
                result.editable_username_pos = parser.i32_()?;
            }
        } else if has_active_usernames {
            result.active_usernames.push(parser.string()?);
            if has_editable_username {
                result.editable_username_pos = 0;
            }
        }
        if has_disabled_usernames {
            result.disabled_usernames = parser.string_vec()?;
        }
        if has_other_editable_usernames {
            result.other_editable_usernames = parser.string_vec()?;
        }
        result.check_validness();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn username(name: &str, editable: bool, active: bool) -> telegram_api::Username {
        telegram_api::Username {
            flags: 0,
            editable,
            active,
            username: name.to_owned(),
        }
    }

    #[test]
    fn single_first_username_is_editable_and_active() {
        let usernames = Usernames::from_telegram("durov".to_owned(), Vec::new());
        assert!(usernames.has_editable_username());
        assert_eq!(usernames.get_editable_username(), "durov");
        assert_eq!(usernames.get_first_username(), "durov");
        assert!(!usernames.is_empty());
        let object = usernames.get_usernames_object();
        assert_eq!(object.active_usernames, ["durov"]);
        assert_eq!(object.disabled_usernames, Vec::<String>::new());
        assert_eq!(object.editable_username, "durov");
        assert_eq!(object.collectible_usernames, Vec::<String>::new());
    }

    #[test]
    fn first_username_is_dropped_when_the_vector_arrives() {
        // The C++ constructor drops the WHOLE input (LOG + return) when a
        // legacy first username arrives together with a username vector.
        let usernames = Usernames::from_telegram(
            "legacy".to_owned(),
            vec![username("a", true, true), username("b", false, true)],
        );
        assert!(usernames.is_empty());
        assert!(!usernames.has_editable_username());
        // The vector alone builds the editable username.
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![username("a", true, true), username("b", false, true)],
        );
        assert_eq!(*usernames.get_active_usernames(), ["a", "b"]);
        assert_eq!(usernames.get_editable_username(), "a");
    }

    #[test]
    fn duplicates_and_empties_are_skipped() {
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![
                username("", true, true),
                username("a", false, true),
                username("a", false, true),
                username("b", false, true),
            ],
        );
        assert_eq!(*usernames.get_active_usernames(), ["a", "b"]);
    }

    #[test]
    fn disabled_editable_username_is_tracked() {
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![username("x", false, true), username("old", true, false)],
        );
        assert!(usernames.is_editable_username_disabled);
        assert_eq!(usernames.get_editable_username(), "old");
        assert_eq!(*usernames.get_active_usernames(), ["x"]);
    }

    #[test]
    fn second_editable_username_moves_to_other_editable() {
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![
                username("one", true, true),
                username("two", true, true),
                username("three", false, true),
            ],
        );
        assert_eq!(usernames.get_editable_username(), "one");
        assert_eq!(usernames.other_editable_usernames, ["two"]);
        let object = usernames.get_usernames_object();
        assert_eq!(object.collectible_usernames, ["three"]);
    }

    #[test]
    fn toggle_disables_and_reenables() {
        let usernames = Usernames::from_telegram("main".to_owned(), Vec::new());
        let disabled = usernames.toggle(false, "main", false);
        // disabling the editable username of a non-bot is not allowed:
        // the toggle returns the state unchanged (actives keep "main")
        assert_eq!(*disabled.get_active_usernames(), ["main"]);

        let two = Usernames::from_telegram(
            String::new(),
            vec![
                username("first", true, true),
                username("second", false, true),
            ],
        );
        let disabled = two.toggle(false, "second", false);
        assert_eq!(*disabled.get_active_usernames(), ["first"]);
        assert_eq!(disabled.disabled_usernames, ["second"]);
        let reenabled = disabled.toggle(false, "second", true);
        assert_eq!(*reenabled.get_active_usernames(), ["first", "second"]);
    }

    #[test]
    fn change_editable_username_replaces_in_place() {
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![username("keep", false, true), username("edit", true, true)],
        );
        let changed = usernames.change_editable_username("new");
        assert_eq!(changed.get_editable_username(), "new");
        assert_eq!(*changed.get_active_usernames(), ["keep", "new"]);
        let removed = changed.change_editable_username("");
        assert_eq!(*removed.get_active_usernames(), ["keep"]);
        assert!(!removed.has_editable_username());
    }

    #[test]
    fn reorder_keeps_the_editable_position() {
        let usernames = Usernames::from_telegram(
            String::new(),
            vec![
                username("first", true, true),
                username("second", false, true),
            ],
        );
        assert!(usernames.can_reorder_to(&vec!["second".to_owned(), "first".to_owned()]));
        let reordered = usernames.reorder_to(vec!["second".to_owned(), "first".to_owned()]);
        assert_eq!(reordered.get_editable_username(), "first");
        assert_eq!(reordered.editable_username_pos, 1);
    }

    #[test]
    fn serialization_round_trips_both_shapes() {
        let single = Usernames::from_telegram("solo".to_owned(), Vec::new());
        let mut bytes = Vec::new();
        single.store(&mut bytes);
        assert_eq!(Usernames::parse(&bytes).unwrap(), single);

        let many = Usernames::from_telegram(
            String::new(),
            vec![
                username("one", true, true),
                username("two", false, true),
                username("three", false, true),
                username("gone", false, false),
            ],
        );
        let mut bytes = Vec::new();
        many.store(&mut bytes);
        assert_eq!(Usernames::parse(&bytes).unwrap(), many);

        let disabled_editable = Usernames::from_telegram(
            String::new(),
            vec![
                username("kept", false, true),
                username("hidden", true, false),
            ],
        );
        let mut bytes = Vec::new();
        disabled_editable.store(&mut bytes);
        assert_eq!(Usernames::parse(&bytes).unwrap(), disabled_editable);
    }
}
