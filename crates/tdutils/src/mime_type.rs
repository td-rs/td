//! Port of `tdutils/td/utils/MimeType.{h,cpp}` plus the table its C++ build
//! generates: `tdutils/generate/mime_types.txt` (copied byte-identical to the
//! crate root as `mime_types.txt`, sha256 `cf624424122258b7f58815207fcdd7976
//! 9a1d3397ac2a1cbbe42124553af8244`) is processed at first use by
//! `generate_mime_types_gperf.cpp`'s rules into the two `gperf` tables the
//! C++ `extension_to_mime_type`/`mime_type_to_extension` functions look up.
//!
//! The C++ lookups are `%ignore-case` (ASCII case-insensitive on both
//! directions) and length-delimited; here the keys are stored lowercased and
//! queries are lowercased the same way. Table-building parity with the
//! generator (verified by the tests below):
//! - `mime → extension`: the preferred-extension list of six picks the
//!   mapped extension when present, otherwise the first one; duplicate MIME
//!   lines keep the first mapping (`std::map::emplace_hint` at `end()`).
//! - `extension → mime`: the first MIME wins (`std::map::emplace`); on a
//!   duplicate with a different `is_private_mime_type` (contains "/x-") the
//!   *public* type overwrites, otherwise the first is kept.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The `mime_types.txt` table, byte-identical to the C++
/// `tdutils/generate/mime_types.txt` at the pinned tdlib checkout.
static MIME_TYPES_TXT: &str = include_str!("../mime_types.txt");

/// `generate_mime_types_gperf.cpp`'s `preferred_extensions`.
const PREFERRED_EXTENSIONS: [(&str, &str); 6] = [
    ("image/jpeg", "jpg"),
    ("audio/mpeg", "mp3"),
    ("audio/midi", "midi"),
    ("text/x-pascal", "pas"),
    ("text/x-asm", "asm"),
    ("video/quicktime", "mov"),
];

/// The two generated tables.
struct MimeTables {
    extension_to_mime_type: HashMap<String, String>,
    mime_type_to_extension: HashMap<String, String>,
}

/// `generate_mime_types_gperf.cpp`'s `is_private_mime_type`.
fn is_private_mime_type(mime_type: &str) -> bool {
    mime_type.contains("/x-")
}

/// The C++ `split(std::string, char)` of the generator: split at the first
/// delimiter occurrence, the tail without it (empty when absent).
fn split_at(s: &str, delimiter: char) -> (&str, &str) {
    match s.find(delimiter) {
        Some(pos) => (&s[..pos], &s[pos + delimiter.len_utf8()..]),
        None => (s, ""),
    }
}

/// The generator's `main` loop over `mime_types.txt`.
fn build_tables() -> MimeTables {
    let mut mime_type_to_extension: HashMap<String, String> = HashMap::new();
    let mut extension_to_mime_type: HashMap<String, String> = HashMap::new();
    for line in MIME_TYPES_TXT.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        let (mime_type, extensions_string) = split_at(line, '\t');
        if mime_type.is_empty() {
            continue; // "Wrong MIME type description" — the C++ skips it
        }
        let extensions_string = extensions_string.trim_start_matches([' ', '\t']);
        if extensions_string.is_empty() {
            continue;
        }
        let mut extensions: Vec<&str> = Vec::new();
        let mut rest = extensions_string;
        while !rest.is_empty() {
            let (head, tail) = split_at(rest, ' ');
            extensions.push(head);
            rest = tail;
        }
        let mut index = 0;
        if let Some(preferred) = PREFERRED_EXTENSIONS
            .iter()
            .find(|(mime, _)| *mime == mime_type)
            .map(|(_, extension)| *extension)
        {
            index = extensions
                .iter()
                .position(|extension| *extension == preferred)
                .expect("preferred extension must be listed");
        }
        mime_type_to_extension
            .entry(mime_type.to_string())
            .or_insert_with(|| extensions[index].to_string());

        for extension in extensions {
            match extension_to_mime_type.get(extension) {
                None => {
                    extension_to_mime_type.insert(extension.to_string(), mime_type.to_string());
                }
                Some(existing) => {
                    if is_private_mime_type(existing) != is_private_mime_type(mime_type)
                        && !is_private_mime_type(mime_type)
                    {
                        extension_to_mime_type.insert(extension.to_string(), mime_type.to_string());
                    }
                }
            }
        }
    }
    MimeTables {
        extension_to_mime_type,
        mime_type_to_extension,
    }
}

fn tables() -> &'static MimeTables {
    static TABLES: OnceLock<MimeTables> = OnceLock::new();
    TABLES.get_or_init(build_tables)
}

fn lookup(map: &HashMap<String, String>, key: &str) -> Option<String> {
    // The gperf tables are `%ignore-case`: the keys are all lowercase in the
    // table file, so an ASCII-lowercased query matches them.
    map.get(&key.to_ascii_lowercase()).cloned()
}

/// `MimeType::to_extension`: map a MIME type to its canonical file
/// extension, or `default_value` when the type is empty or unknown (the
/// C++ logs the unknown type at INFO).
pub fn to_extension_or(mime_type: &str, default_value: &str) -> String {
    if mime_type.is_empty() {
        return default_value.to_string();
    }
    match lookup(&tables().mime_type_to_extension, mime_type) {
        Some(extension) => extension,
        None => {
            crate::log_info!("Unknown file MIME type {}", mime_type);
            default_value.to_string()
        }
    }
}

/// `MimeType::to_extension` with the C++ default `Slice()` default value.
pub fn to_extension(mime_type: &str) -> String {
    to_extension_or(mime_type, "")
}

/// `MimeType::from_extension`: map a file extension to its MIME type, or
/// `default_value` when the extension is empty or unknown (the C++ logs the
/// unknown extension at INFO).
pub fn from_extension_or(extension: &str, default_value: &str) -> String {
    if extension.is_empty() {
        return default_value.to_string();
    }
    match lookup(&tables().extension_to_mime_type, extension) {
        Some(mime_type) => mime_type,
        None => {
            crate::log_info!("Unknown file extension {}", extension);
            default_value.to_string()
        }
    }
}

/// `MimeType::from_extension` with the C++ default `Slice()` default value.
pub fn from_extension(extension: &str) -> String {
    from_extension_or(extension, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sizes_match_the_mime_types_file() {
        // 786 lines, no duplicate MIME lines; the duplicate extensions
        // deduplicate per the generator rules.
        let tables = tables();
        assert_eq!(tables.mime_type_to_extension.len(), 786);
        assert_eq!(tables.extension_to_mime_type.len(), 1005);
    }

    #[test]
    fn from_extension_spot_pairs() {
        assert_eq!(from_extension("ez"), "application/andrew-inset");
        assert_eq!(from_extension("json"), "application/json");
        assert_eq!(from_extension("ink"), "application/inkml+xml");
        assert_eq!(from_extension("7z"), "application/x-7z-compressed");
        assert_eq!(from_extension("tgs"), "application/x-tgsticker");
        assert_eq!(from_extension("webp"), "image/webp");
        assert_eq!(from_extension("wasm"), "application/wasm");
    }

    #[test]
    fn preferred_extensions_win_the_canonical_form() {
        // image/jpeg has extensions [jpeg, jpg, jpe] — the preferred "jpg"
        // is the canonical extension; from_extension answers for every one.
        assert_eq!(to_extension("image/jpeg"), "jpg");
        assert_eq!(from_extension("jpeg"), "image/jpeg");
        assert_eq!(from_extension("jpg"), "image/jpeg");
        assert_eq!(from_extension("jpe"), "image/jpeg");
        assert_eq!(to_extension("audio/mpeg"), "mp3");
        assert_eq!(from_extension("mpga"), "audio/mpeg");
        assert_eq!(to_extension("audio/midi"), "midi");
        assert_eq!(to_extension("text/x-pascal"), "pas");
        assert_eq!(to_extension("text/x-asm"), "asm");
        assert_eq!(to_extension("video/quicktime"), "mov");
        assert_eq!(from_extension("qt"), "video/quicktime");
    }

    #[test]
    fn duplicate_extensions_keep_first_public_wins() {
        // "sub": image/vnd.dvb.subtitle (line 658) vs text/vnd.dvb.subtitle
        // (line 723) — both public, the first stays.
        assert_eq!(from_extension("sub"), "image/vnd.dvb.subtitle");
        // "ttc": application/x-font-ttf (line 510, PRIVATE — contains
        // "/x-") vs font/collection (line 632, public) — the public type
        // overwrites the earlier private one.
        assert_eq!(from_extension("ttc"), "font/collection");
        // "wmz": application/x-ms-wmz (private, line 531) vs
        // application/x-msmetafile (private, line 539) — both private,
        // the first stays.
        assert_eq!(from_extension("wmz"), "application/x-ms-wmz");
        // "ttf": application/x-font-ttf (private, line 510) vs font/ttf
        // (public, line 634) — the public overwrite again.
        assert_eq!(from_extension("ttf"), "font/ttf");
    }

    #[test]
    fn lookups_ignore_ascii_case() {
        // The C++ gperf tables are generated with %ignore-case.
        assert_eq!(from_extension("JPG"), "image/jpeg");
        assert_eq!(from_extension("Jpg"), "image/jpeg");
        assert_eq!(to_extension("IMAGE/JPEG"), "jpg");
        assert_eq!(to_extension("Application/JSON"), "json");
    }

    #[test]
    fn empty_and_unknown_answers() {
        assert_eq!(from_extension(""), "");
        assert_eq!(to_extension(""), "");
        assert_eq!(
            from_extension_or("nosuchext", "application/x-fallback"),
            "application/x-fallback"
        );
        assert_eq!(to_extension_or("no/such-type", "bin"), "bin");
    }
}
