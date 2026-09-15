//! Port of `td/utils/Hints.{h,cpp}` (tdlib 1.8.67): the search index used for
//! contacts (`contacts_hints_`) and similar key→name registries.
//!
//! Mapping notes:
//! - The C++ `std::map<string, vector<KeyT>>` word tables become
//!   `BTreeMap<Vec<u8>, Vec<i64>>` (byte-ordered like the C++ `string` keys,
//!   so the `lower_bound` + prefix scan is a `BTreeMap::range` scan). Words
//!   are carried as bytes, not `String`, because the C++ decoder tolerates
//!   arbitrary byte input; `add`/`search` take `&str`, whose search words are
//!   always valid UTF-8.
//! - `td::unique` is sort + drop-duplicates, `Hash<KeyT>` maps stay
//!   `HashMap` (iteration order is unobservable: the empty-query result list
//!   is re-sorted by `(rating, key)` before it is returned, and multi-word
//!   queries never start from it).
//! - C++ `CHECK`s become panics with the same conditions.

use std::collections::{BTreeMap, HashMap};

use crate::translit::get_word_transliterations;
use crate::utf8::utf8_get_search_words;

/// The port of C++ `class Hints` (`KeyT = int64`, `RatingT = int64`).
#[derive(Default)]
pub struct Hints {
    word_to_keys: BTreeMap<Vec<u8>, Vec<i64>>,
    translit_word_to_keys: BTreeMap<Vec<u8>, Vec<i64>>,
    key_to_name: HashMap<i64, String>,
    key_to_rating: HashMap<i64, i64>,
}

impl Hints {
    /// C++ `Hints::add`: (re)index `key` under `name`; an empty name removes
    /// the key.
    pub fn add(&mut self, key: i64, name: &str) {
        if let Some(old_name) = self.key_to_name.get(&key) {
            if old_name == name {
                return;
            }
            let old_name = old_name.clone();
            let mut old_transliterations: Vec<String> = Vec::new();
            for old_word in get_words(&old_name) {
                delete_word(old_word.as_bytes(), key, &mut self.word_to_keys);

                for w in get_word_transliterations(&old_word, false) {
                    if w != old_word {
                        old_transliterations.push(w);
                    }
                }
            }
            for word in Self::fix_words(old_transliterations) {
                delete_word(word.as_bytes(), key, &mut self.translit_word_to_keys);
            }
        }
        if name.is_empty() {
            self.key_to_name.remove(&key);
            self.key_to_rating.remove(&key);
            return;
        }

        let mut transliterations: Vec<String> = Vec::new();
        for word in get_words(name) {
            add_word(word.as_bytes(), key, &mut self.word_to_keys);

            for w in get_word_transliterations(&word, false) {
                if w != word {
                    transliterations.push(w);
                }
            }
        }
        for word in Self::fix_words(transliterations) {
            add_word(word.as_bytes(), key, &mut self.translit_word_to_keys);
        }

        self.key_to_name.insert(key, name.to_owned());
    }

    /// C++ `Hints::remove`.
    pub fn remove(&mut self, key: i64) {
        self.add(key, "");
    }

    /// C++ `Hints::set_rating`.
    pub fn set_rating(&mut self, key: i64, rating: i64) {
        self.key_to_rating.insert(key, rating);
    }

    /// C++ `Hints::search`: the best `limit` keys matching all query words,
    /// with the total match count; `return_all_for_empty_query` lists every
    /// key for an empty query.
    pub fn search(
        &self,
        query: &str,
        limit: i32,
        return_all_for_empty_query: bool,
    ) -> (usize, Vec<i64>) {
        let mut results: Vec<i64> = Vec::new();

        if limit < 0 {
            return (self.key_to_name.len(), results);
        }

        let words = get_words(query);
        if return_all_for_empty_query && words.is_empty() {
            results.reserve(self.key_to_name.len());
            for key in self.key_to_name.keys() {
                results.push(*key);
            }
        }

        for (i, word) in words.iter().enumerate() {
            let keys = self.search_word(word);
            if i == 0 {
                results = keys;
                continue;
            }

            // Intersect the two sorted lists in place, like the C++.
            let mut results_pos = 0usize;
            let mut keys_pos = 0usize;
            let mut new_results_size = 0usize;
            while results_pos < results.len() && keys_pos < keys.len() {
                if results[results_pos] < keys[keys_pos] {
                    results_pos += 1;
                } else if results[results_pos] > keys[keys_pos] {
                    keys_pos += 1;
                } else {
                    results[new_results_size] = results[results_pos];
                    new_results_size += 1;
                    results_pos += 1;
                    keys_pos += 1;
                }
            }
            results.truncate(new_results_size);
        }

        let total_size = results.len();
        let rating = |key: &i64| self.key_to_rating.get(key).copied().unwrap_or(0);
        // CompareByRating: by rating, ties by key. A full sort + truncate
        // reproduces the C++ partial_sort's `limit`-prefix exactly (the
        // comparator is total: keys are unique).
        results.sort_unstable_by(|&a, &b| rating(&a).cmp(&rating(&b)).then(a.cmp(&b)));
        if total_size >= limit as usize {
            results.truncate(limit as usize);
        }

        (total_size, results)
    }

    /// C++ `Hints::has_key`.
    pub fn has_key(&self, key: i64) -> bool {
        self.key_to_name.contains_key(&key)
    }

    /// C++ `Hints::key_to_string`.
    pub fn key_to_string(&self, key: i64) -> String {
        self.key_to_name.get(&key).cloned().unwrap_or_default()
    }

    /// C++ `Hints::search_empty` — `search("", limit, true)`.
    pub fn search_empty(&self, limit: i32) -> (usize, Vec<i64>) {
        self.search("", limit, true)
    }

    /// C++ `Hints::size`.
    pub fn size(&self) -> usize {
        self.key_to_name.len()
    }

    /// C++ `Hints::fix_words`: sort, then drop every word that is a prefix of
    /// its successor; a lone empty word collapses to nothing.
    pub fn fix_words(mut words: Vec<String>) -> Vec<String> {
        words.sort();

        let mut new_words: Vec<String> = Vec::with_capacity(words.len());
        for i in 0..words.len() {
            let is_last = i + 1 == words.len();
            let next_is_superstring = !is_last && words[i + 1].starts_with(words[i].as_str());
            if !next_is_superstring {
                new_words.push(words[i].clone());
            }
        }
        if new_words.len() == 1 && new_words[0].is_empty() {
            new_words.clear();
        }
        new_words
    }

    /// C++ `Hints::search_word`: translit-table hits for `word`, plus
    /// direct-table hits for every transliteration of it.
    fn search_word(&self, word: &str) -> Vec<i64> {
        let mut results: Vec<i64> = Vec::new();
        add_search_results(&mut results, word.as_bytes(), &self.translit_word_to_keys);
        for w in get_word_transliterations(word, true) {
            add_search_results(&mut results, w.as_bytes(), &self.word_to_keys);
        }

        results.sort_unstable();
        results.dedup();
        results
    }
}

/// C++ `Hints::get_words` (private static): search words of a name, fixed.
fn get_words(name: &str) -> Vec<String> {
    Hints::fix_words(utf8_get_search_words(name))
}

/// C++ `Hints::add_word` (private static).
fn add_word(word: &[u8], key: i64, word_to_keys: &mut BTreeMap<Vec<u8>, Vec<i64>>) {
    let keys = word_to_keys.entry(word.to_vec()).or_default();
    assert!(!keys.contains(&key), "Hints: duplicate word key");
    keys.push(key);
}

/// C++ `Hints::delete_word` (private static): swap-remove `key` (order inside
/// a word's key list is unobservable — search results are re-sorted).
fn delete_word(word: &[u8], key: i64, word_to_keys: &mut BTreeMap<Vec<u8>, Vec<i64>>) {
    let keys = word_to_keys
        .get_mut(word)
        .expect("Hints: deleting a word that is not indexed");
    let key_pos = keys
        .iter()
        .position(|&k| k == key)
        .expect("Hints: key not found for word");
    if keys.len() == 1 {
        word_to_keys.remove(word);
    } else {
        let last = *keys.last().expect("len > 1 above");
        keys[key_pos] = last;
        keys.pop();
    }
}

/// C++ `Hints::add_search_results` (private static): every key list whose
/// word begins with `word` (the `lower_bound` + `begins_with` scan).
fn add_search_results(results: &mut Vec<i64>, word: &[u8], word_to_keys: &BTreeMap<Vec<u8>, Vec<i64>>) {
    for (w, keys) in word_to_keys.range(word.to_vec()..) {
        if !w.starts_with(word) {
            break;
        }
        results.extend_from_slice(keys);
    }
}
