//! Extracts the golden test vectors embedded as base64 string literals in the
//! C++ td repository's `test/data.cpp` into binary files, a `sha256sums.txt`
//! manifest, and a provenance `README.md` (plan step S04).
//!
//! The five blobs and how the C++ tests decode them:
//!
//! | symbol                | decode call         | consumer                     |
//! |-----------------------|---------------------|------------------------------|
//! | `thumbnail`           | `base64url_decode`  | `test/tdclient.cpp`          |
//! | `gzip_bomb`           | `base64url_decode`  | `test/http.cpp`              |
//! | `gzip`                | `base64url_decode`  | `test/http.cpp`              |
//! | `sqlite_sample_db_v3` | `base64_decode`     | `test/db.cpp` (SQLCipher 3) |
//! | `sqlite_sample_db_v4` | `base64_decode`     | `test/db.cpp` (SQLCipher 4) |
//!
//! The decoder mirrors td's `tdutils/td/utils/base64.cpp` semantics exactly:
//! optional `=` padding, strict per-variant alphabet, nonzero discarded tail
//! bits rejected.
//!
//! Usage: `extract-test-data <td-repo-root> <output-dir>`

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

/// One golden blob: symbol name in `data.cpp`, whether the C++ tests decode it
/// with the URL-safe variant, and the test site that consumes it.
struct VectorDef {
    name: &'static str,
    url: bool,
    consumer: &'static str,
}

const VECTORS: &[VectorDef] = &[
    VectorDef {
        name: "thumbnail",
        url: true,
        consumer: "test/tdclient.cpp `base64url_decode`",
    },
    VectorDef {
        name: "gzip_bomb",
        url: true,
        consumer: "test/http.cpp `base64url_decode`",
    },
    VectorDef {
        name: "gzip",
        url: true,
        consumer: "test/http.cpp `base64url_decode`",
    },
    VectorDef {
        name: "sqlite_sample_db_v3",
        url: false,
        consumer: "test/db.cpp `base64_decode` (SQLCipher 3)",
    },
    VectorDef {
        name: "sqlite_sample_db_v4",
        url: false,
        consumer: "test/db.cpp `base64_decode` (SQLCipher 4)",
    },
];

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(td_root), Some(out_dir), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: extract-test-data <td-repo-root> <output-dir>");
        std::process::exit(2);
    };
    if let Err(err) = run(&td_root, &out_dir) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run(td_root: &str, out_dir: &str) -> Result<(), String> {
    let td_root = Path::new(td_root);
    let data_cpp = td_root.join("test/data.cpp");
    let src = fs::read_to_string(&data_cpp)
        .map_err(|e| format!("reading {}: {e}", data_cpp.display()))?;

    let out_dir = Path::new(out_dir);
    fs::create_dir_all(out_dir).map_err(|e| format!("creating {}: {e}", out_dir.display()))?;

    let commit = git_head(td_root);
    let mut sums = String::new();
    let mut rows = String::new();
    for def in VECTORS {
        let literal = parse_array(&src, def.name)?;
        let decoded =
            td_base64_decode(&literal, def.url).map_err(|e| format!("{}: {e}", def.name))?;
        let file_name = format!("{}.bin", def.name);
        let path = out_dir.join(&file_name);
        fs::write(&path, &decoded).map_err(|e| format!("writing {}: {e}", path.display()))?;
        let sha_hex = hex_lower(Sha256::digest(&decoded).as_slice());
        println!("{}\t{}\t{sha_hex}", def.name, decoded.len(),);
        sums.push_str(&format!("{sha_hex}  {file_name}\n"));
        let variant = if def.url { "base64url" } else { "base64" };
        rows.push_str(&format!(
            "| `{file_name}` | `{}` | {} | `{sha_hex}` | {variant} | {} |\n",
            def.name,
            decoded.len(),
            def.consumer,
        ));
    }
    fs::write(out_dir.join("sha256sums.txt"), sums)
        .map_err(|e| format!("writing sha256sums.txt: {e}"))?;
    fs::write(out_dir.join("README.md"), readme(&commit, &rows))
        .map_err(|e| format!("writing README.md: {e}"))?;
    Ok(())
}

/// Extracts the string literal assigned to `static const char <name>_arr[]`,
/// concatenating adjacent C++ string literals. Rejects escape sequences: the
/// golden data is plain base64 text, so a backslash means the file changed
/// shape.
fn parse_array(src: &str, name: &str) -> Result<String, String> {
    let marker = format!("static const char {name}_arr[] =");
    let Some(start) = src.find(&marker) else {
        return Err(format!("array `{name}_arr` not found"));
    };
    let bytes = src.as_bytes();
    let mut pos = skip_ws(bytes, start + marker.len());
    let mut literal = String::new();
    loop {
        if bytes.get(pos) != Some(&b'"') {
            return Err(format!("`{name}_arr`: expected string literal"));
        }
        let close = src[pos + 1..]
            .find('"')
            .map(|offset| pos + 1 + offset)
            .ok_or_else(|| format!("`{name}_arr`: unterminated string literal"))?;
        let chunk = &src[pos + 1..close];
        if chunk.contains('\\') {
            return Err(format!("`{name}_arr`: unexpected escape sequence"));
        }
        literal.push_str(chunk);
        pos = skip_ws(bytes, close + 1);
        match bytes.get(pos) {
            Some(b'"') => continue,
            Some(b';') => break,
            _ => return Err(format!("`{name}_arr`: expected `;` after string literals")),
        }
    }
    // data.cpp exports each blob via `const char *<name> = <name>_arr;`;
    // require that declaration so the symbol set cannot drift silently.
    if !src.contains(&format!("const char *{name} = {name}_arr;")) {
        return Err(format!("export declaration for `{name}` not found"));
    }
    Ok(literal)
}

fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while matches!(bytes.get(pos), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        pos += 1;
    }
    pos
}

/// Decodes base64 with td's exact leniency rules (`base64_drop_padding` +
/// `do_base64_decode_impl` in `tdutils/td/utils/base64.cpp`):
///
/// - at most two trailing `=` are dropped first;
/// - for the standard variant the padded length must be a multiple of 4;
/// - for the URL-safe variant unpadded input is canonical and its length must
///   not be ≡ 1 (mod 4);
/// - the alphabet is strict per variant;
/// - nonzero discarded bits in a final 2- or 3-character group are an error.
fn td_base64_decode(input: &str, url: bool) -> Result<Vec<u8>, String> {
    let mut chars = input.as_bytes();
    let mut padding = 0;
    while chars.last() == Some(&b'=') {
        chars = &chars[..chars.len() - 1];
        padding += 1;
    }
    if padding >= 3 {
        return Err("wrong string padding".into());
    }
    if (!url || padding > 0) && !(chars.len() + padding).is_multiple_of(4) {
        return Err("wrong padding length".into());
    }
    if url && chars.len() % 4 == 1 {
        return Err("wrong string length".into());
    }
    let mut out = Vec::with_capacity(chars.len() / 4 * 3 + 3);
    let mut i = 0;
    while i < chars.len() {
        let left = (chars.len() - i).min(4);
        let mut c: u32 = 0;
        for t in 0..left {
            let value = char_value(chars[i + t], url)?;
            c |= value << ((3 - t) * 6);
        }
        i += left;
        out.push((c >> 16) as u8);
        if left == 2 {
            if c & 0xffff != 0 {
                return Err("wrong padding in the string".into());
            }
        } else {
            out.push((c >> 8) as u8);
            if left == 3 {
                if c & 0xff != 0 {
                    return Err("wrong padding in the string".into());
                }
            } else {
                out.push(c as u8);
            }
        }
    }
    Ok(out)
}

fn char_value(byte: u8, url: bool) -> Result<u32, String> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' if !url => 62,
        b'/' if !url => 63,
        b'-' if url => 62,
        b'_' if url => 63,
        _ => return Err(format!("wrong character in the string: '{}'", byte as char)),
    };
    Ok(u32::from(value))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

fn git_head(td_root: &Path) -> String {
    Command::new("git")
        .args(["-C", td_root.to_str().unwrap_or("."), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn readme(commit: &str, rows: &str) -> String {
    format!(
        "# Golden vectors from C++ td `test/data.cpp`\n\n\
         Extracted by plan step S04; see `RUST_REWRITE_PLAN.md` in the C++ repo.\n\n\
         - Source file: `test/data.cpp` in the C++ td repository\n\
         - Source commit: `{commit}`\n\
         - Regenerate: from `conformance/`, `cargo run -- <td-repo-root> vectors`\n\n\
         Integrity: `sha256sum -c sha256sums.txt`\n\n\
         Verification (S04): bytes must equal the output of the standalone C++ dump\n\
         oracle in `../cpp_dump/` (`conformance/cpp_dump/verify.sh`), which decodes\n\
         the same embedded arrays the C++ tests consume.\n\n\
         | file | symbol in data.cpp | bytes | sha256 | variant | decoded by C++ tests |\n\
         |------|--------------------|-------|--------|---------|----------------------|\n\
         {rows}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only encoder (mirrors `base64_encode_impl`; the URL variant emits
    /// no padding, the standard variant pads with `=`).
    fn encode(input: &[u8], url: bool) -> String {
        const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let alphabet: &[u8; 64] = if url { URL } else { STD };
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let mut n = u32::from(chunk[0]) << 16;
            if chunk.len() > 1 {
                n |= u32::from(chunk[1]) << 8;
            }
            if chunk.len() > 2 {
                n |= u32::from(chunk[2]);
            }
            out.push(alphabet[(n >> 18 & 63) as usize] as char);
            out.push(alphabet[(n >> 12 & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(alphabet[(n >> 6 & 63) as usize] as char);
            } else if !url {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(alphabet[(n & 63) as usize] as char);
            } else if !url {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn decodes_rfc_4648_vectors() {
        assert_eq!(td_base64_decode("", false).unwrap(), b"");
        assert_eq!(td_base64_decode("TWFu", false).unwrap(), b"Man");
        assert_eq!(td_base64_decode("TWE=", false).unwrap(), b"Ma");
        assert_eq!(td_base64_decode("TQ==", false).unwrap(), b"M");
        // URL variant: unpadded canonical form.
        assert_eq!(td_base64_decode("TWE", true).unwrap(), b"Ma");
        assert_eq!(td_base64_decode("TQ", true).unwrap(), b"M");
        assert_eq!(td_base64_decode("_-8", true).unwrap(), b"\xff\xef");
    }

    #[test]
    fn rejects_malformed_input() {
        // Invalid character.
        assert!(td_base64_decode("TW@n", false).is_err());
        // Alphabet mixing.
        assert!(td_base64_decode("TW-n", false).is_err());
        assert!(td_base64_decode("TW+n", true).is_err());
        // URL length ≡ 1 (mod 4).
        assert!(td_base64_decode("TWFua", true).is_err());
        // Three or more padding characters.
        assert!(td_base64_decode("TQ===", false).is_err());
        // Standard variant: length incl. padding must be a multiple of 4.
        assert!(td_base64_decode("TWF", false).is_err());
        assert!(td_base64_decode("TWFu=", false).is_err());
        // Nonzero discarded bits in the final group.
        assert!(td_base64_decode("TR==", false).is_err());
        assert!(td_base64_decode("TWX", true).is_err());
    }

    #[test]
    fn round_trips_every_length() {
        let data: Vec<u8> = (0..u8::MAX).collect();
        for len in 0..data.len() {
            let input = &data[..len];
            assert_eq!(
                td_base64_decode(&encode(input, true), true).unwrap(),
                input,
                "url round-trip failed at len {len}"
            );
            assert_eq!(
                td_base64_decode(&encode(input, false), false).unwrap(),
                input,
                "standard round-trip failed at len {len}"
            );
        }
    }

    #[test]
    fn parses_data_cpp_shaped_source() {
        let src = "static const char thumbnail_arr[] =\n    \"AB\"\n    \"CD\";\n\
                   const char *thumbnail = thumbnail_arr;\n";
        assert_eq!(parse_array(src, "thumbnail").unwrap(), "ABCD");
    }

    #[test]
    fn parse_array_rejects_drift() {
        // Missing array.
        assert!(parse_array("int x;", "thumbnail").is_err());
        // Escape sequence inside the literal.
        let escaped = "static const char x_arr[] = \"A\\nB\";\nconst char *x = x_arr;\n";
        assert!(parse_array(escaped, "x").is_err());
        // Missing export declaration.
        let no_export = "static const char x_arr[] = \"AB\";\n";
        assert!(parse_array(no_export, "x").is_err());
        // Terminator without literal.
        let empty = "static const char x_arr[] = ;\nconst char *x = x_arr;\n";
        assert!(parse_array(empty, "x").is_err());
    }
}
