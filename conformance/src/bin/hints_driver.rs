//! Rust twin of `hints_oracle/oracle.cpp` — the same line protocol answered by
//! the ported `Hints`/`translit`/`utf8` modules; `hints_oracle/verify.sh`
//! byte-compares the two outputs.

use std::io::{self, BufRead, Write};

use tdutils::hints::Hints;
use tdutils::translit::get_word_transliterations;
use tdutils::utf8::utf8_get_search_words;

fn join_keys(keys: &[i64]) -> String {
    keys.iter()
        .map(|key| key.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn join_strings(parts: &[String]) -> String {
    parts.join("|")
}

fn parse_i64(s: &str) -> i64 {
    s.trim().parse().unwrap_or(0)
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut hints = Hints::default();
    for line in stdin.lock().lines() {
        let line = line.expect("stdin");
        let line = line.trim_end_matches(['\r', '\n']);
        let (cmd, rest) = match line.split_once(' ') {
            Some((cmd, rest)) => (cmd, rest),
            None => (line, ""),
        };
        let answer = if cmd.is_empty() || cmd.starts_with('#') {
            "#".to_owned()
        } else {
            match cmd {
                "add" => {
                    let (key, name) = rest.split_once(' ').unwrap_or((rest, ""));
                    hints.add(parse_i64(key), name);
                    "ok".to_owned()
                }
                "rating" => {
                    let (key, rating) = rest.split_once(' ').expect("rating key value");
                    hints.set_rating(parse_i64(key), parse_i64(rating));
                    "ok".to_owned()
                }
                "search" => {
                    let (limit, tail) = rest.split_once(' ').expect("search limit flag query");
                    let (flag, query) = tail.split_once(' ').unwrap_or((tail, ""));
                    let (total, keys) = hints.search(query, parse_i64(limit) as i32, flag == "1");
                    format!("{total} {}", join_keys(&keys))
                }
                "words" => join_strings(&Hints::fix_words(utf8_get_search_words(rest))),
                "translit" => {
                    let (flag, word) = rest.split_once(' ').expect("translit flag word");
                    join_strings(&get_word_transliterations(word, flag == "1"))
                }
                "size" => hints.size().to_string(),
                "has_key" => i32::from(hints.has_key(parse_i64(rest))).to_string(),
                "key_to_string" => hints.key_to_string(parse_i64(rest)),
                _ => "ERR".to_owned(),
            }
        };
        writeln!(out, "{answer}").expect("stdout");
    }
}
