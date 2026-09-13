//! Port of `td/db/SeqKeyValue.h`: an in-memory key-value store with a
//! monotonically increasing sequence number for change tracking.

use std::collections::HashMap;

/// `td::SeqKeyValue`
#[derive(Default)]
pub struct SeqKeyValue {
    map: HashMap<String, String>,
    current_id: u64,
}

pub type SeqNo = u64;

impl SeqKeyValue {
    /// `SeqKeyValue::set` — returns the new sequence number, or 0 if the
    /// value is unchanged.
    pub fn set(&mut self, key: &str, value: &str) -> SeqNo {
        assert!(!key.is_empty());
        match self.map.get_mut(key) {
            Some(old) if *old == value => return 0,
            Some(old) => *old = value.to_string(),
            None => {
                self.map.insert(key.to_string(), value.to_string());
            }
        }
        self.next_seq_no()
    }

    /// `SeqKeyValue::erase`
    pub fn erase(&mut self, key: &str) -> SeqNo {
        if self.map.remove(key).is_none() {
            return 0;
        }
        self.next_seq_no()
    }

    /// `SeqKeyValue::erase_batch`
    pub fn erase_batch(&mut self, keys: &[String]) -> SeqNo {
        let mut count = 0;
        for key in keys {
            if self.map.remove(key).is_some() {
                count += 1;
            }
        }
        if count == 0 {
            return 0;
        }
        let result = self.current_id + 1;
        self.current_id += count as u64;
        result
    }

    /// `SeqKeyValue::seq_no`
    pub fn seq_no(&self) -> SeqNo {
        self.current_id + 1
    }

    /// `SeqKeyValue::get` — empty string when missing.
    pub fn get(&self, key: &str) -> String {
        self.map.get(key).cloned().unwrap_or_default()
    }

    /// `SeqKeyValue::isset`
    pub fn isset(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// `SeqKeyValue::size`
    pub fn size(&self) -> usize {
        self.map.len()
    }

    /// `SeqKeyValue::get_all`
    pub fn get_all(&self) -> HashMap<String, String> {
        self.map.clone()
    }

    fn next_seq_no(&mut self) -> SeqNo {
        self.current_id += 1;
        self.current_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_key_value_semantics() {
        let mut kv = SeqKeyValue::default();
        assert_eq!(kv.seq_no(), 1);
        assert_eq!(kv.set("a", "1"), 1);
        assert_eq!(kv.set("b", "2"), 2);
        assert_eq!(kv.set("a", "1"), 0, "unchanged value is a no-op");
        assert_eq!(kv.set("a", "11"), 3);
        assert_eq!(kv.seq_no(), 4);

        assert_eq!(kv.get("a"), "11");
        assert_eq!(kv.get("missing"), "");
        assert!(kv.isset("a"));
        assert!(!kv.isset("missing"));
        assert_eq!(kv.size(), 2);

        assert_eq!(kv.erase("a"), 4);
        assert_eq!(kv.erase("a"), 0);
        assert!(!kv.isset("a"));

        assert_eq!(kv.erase_batch(&["missing".to_string()]), 0);
        let result = kv.erase_batch(&["b".to_string(), "c".to_string()]);
        // only "b" existed: one seq number consumed starting at the old seq_no
        assert_eq!(result, 5);
        assert_eq!(kv.seq_no(), 6);
        assert_eq!(kv.size(), 0);

        kv.set("z", "26");
        let all = kv.get_all();
        assert_eq!(all.get("z").map(String::as_str), Some("26"));
    }
}
