//! Port of `td/net/HttpQuery.h` — the client-side response subset.
//!
//! C++ `HttpQuery` also carries server-side state (`url_path_`, `files_`,
//! form-data); those arrive with the reader-side steps. The client-facing
//! `Wget` surface needs the response code, reason phrase, headers, body and
//! the JSON-derived [`HttpQuery::args`] (`HttpReader::parse_json_parameters`
//! runs on `application/json` responses, so a JSON body is readable field by
//! field via [`HttpQuery::get_arg`] — the access pattern every simple-config
//! fetcher uses).
//!
//! Note: reqwest normalizes response header names to lowercase, so header
//! keys here are lowercase while the C++ class preserves server casing.
//! [`HttpQuery::get_header`] keeps the C++ exact-match semantics.
//!
//! Deviation: `application/x-www-form-urlencoded` response bodies are not
//! split into args (`HttpReader::parse_parameters` is server-side form
//! parsing); the body stays in `body`.

use tdutils::json::{JsonParser, do_json_skip, json_string_decode};
use tdutils::status::{Result, Status};

/// `HttpReader::MAX_TOTAL_PARAMETERS_LENGTH`.
const MAX_TOTAL_PARAMETERS_LENGTH: usize = 1 << 20;

/// `td::HttpQuery` (response subset).
#[derive(Clone, Debug, Default)]
pub struct HttpQuery {
    /// `code_` — the HTTP status code.
    pub code: i32,
    /// `reason_`.
    pub reason: String,
    /// `headers_` (lowercased keys, in response order).
    pub headers: Vec<(String, String)>,
    /// `content_` (empty for parsed JSON bodies, like the C++ `ReadArgs`).
    pub body: Vec<u8>,
    /// `args_` — the fields of a JSON response body
    /// (`HttpReader::parse_json_parameters`): string values are JSON-decoded,
    /// other values keep their raw JSON text; a bare JSON string body lands
    /// under the `"content"` key.
    pub args: Vec<(String, String)>,
}

impl HttpQuery {
    /// `HttpQuery::get_header` — case-sensitive exact key match, first
    /// occurrence, `None` instead of the C++ empty `Slice`.
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `HttpQuery::get_arg` — first match, `None` instead of the C++ empty
    /// `Slice`.
    pub fn get_arg(&self, key: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `HttpReader::parse_json_parameters` — splits a JSON body into
    /// [`HttpQuery::args`]. The C++ reader clears `content_` and logs
    /// non-fatal parse errors; the fatal size overflow (HTTP 413) is the
    /// caller's error return.
    pub fn parse_json_parameters(&mut self) -> Result<()> {
        if self.body.is_empty() {
            return Result::Ok(());
        }
        if self.body.len() > MAX_TOTAL_PARAMETERS_LENGTH {
            return Result::Err(Status::error(
                413,
                "Request Entity Too Large: too many parameters",
            ));
        }

        let mut parser = JsonParser::new(&self.body);
        parser.skip_whitespaces();
        if parser.peek_char() == b'"' {
            return match json_string_decode(&mut parser) {
                Result::Ok(value) => {
                    self.args.push(("content".to_string(), value));
                    Result::Ok(())
                }
                Result::Err(error) => Result::Err(Status::error(
                    400,
                    &format!(
                        "Bad Request: can't parse string content: {}",
                        error.message()
                    ),
                )),
            };
        }
        // `Parser::skip('{')` in C++ sets the status on mismatch instead of
        // panicking like the Rust `JsonParser::skip`, so it is emulated here.
        parser.skip_whitespaces();
        if parser.peek_char() != b'{' {
            return Result::Err(Status::error(400, "Bad Request: JSON object expected"));
        }
        parser.advance(1);
        loop {
            parser.skip_whitespaces();
            if parser.try_skip(b"}") {
                parser.skip_whitespaces();
                if parser.empty() {
                    return Result::Ok(());
                }
                return Result::Err(Status::error(
                    400,
                    "Bad Request: unexpected data after object end",
                ));
            }
            if parser.empty() {
                return Result::Err(Status::error(400, "Bad Request: expected parameter name"));
            }
            let key = match json_string_decode(&mut parser) {
                Result::Ok(key) => key,
                Result::Err(error) => {
                    return Result::Err(Status::error(
                        400,
                        &format!(
                            "Bad Request: can't parse parameter name: {}",
                            error.message()
                        ),
                    ));
                }
            };
            parser.skip_whitespaces();
            if parser.peek_char() != b':' {
                return Result::Err(Status::error(
                    400,
                    "Bad Request: can't parse object, ':' expected",
                ));
            }
            parser.advance(1);
            parser.skip_whitespaces();
            let value = if parser.peek_char() == b'"' {
                match json_string_decode(&mut parser) {
                    Result::Ok(value) => value,
                    Result::Err(error) => {
                        return Result::Err(Status::error(
                            400,
                            &format!(
                                "Bad Request: can't parse parameter value: {}",
                                error.message()
                            ),
                        ));
                    }
                }
            } else {
                const DEFAULT_MAX_DEPTH: i32 = 100;
                let begin = parser.position();
                let skip_status = do_json_skip(&mut parser, DEFAULT_MAX_DEPTH);
                if skip_status.is_error() {
                    return Result::Err(Status::error(
                        400,
                        &format!(
                            "Bad Request: can't parse parameter value: {}",
                            skip_status.message()
                        ),
                    ));
                }
                String::from_utf8_lossy(&self.body[begin..parser.position()]).into_owned()
            };
            self.args.push((key, value));

            parser.skip_whitespaces();
            if parser.peek_char() != b'}' && !parser.try_skip(b",") {
                return Result::Err(Status::error(
                    400,
                    "Bad Request: expected next field or object end",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<(String, String)> {
        let mut query = HttpQuery {
            body: body.as_bytes().to_vec(),
            ..HttpQuery::default()
        };
        match query.parse_json_parameters() {
            Result::Ok(()) => query.args,
            Result::Err(status) => panic!("unexpected parse error: {status}"),
        }
    }

    #[test]
    fn splits_object_fields() {
        // String values are decoded, everything else keeps its raw text.
        let args = parse(
            r#"{"str": "a \"quoted\" value", "arr": [ 1 , {"x":null} ], "num": -1.5e3, "bool": true, "n": null}"#,
        );
        assert_eq!(
            args,
            vec![
                ("str".to_string(), "a \"quoted\" value".to_string()),
                ("arr".to_string(), r#"[ 1 , {"x":null} ]"#.to_string()),
                ("num".to_string(), "-1.5e3".to_string()),
                ("bool".to_string(), "true".to_string()),
                ("n".to_string(), "null".to_string()),
            ]
        );
        // The body is cleared by the caller (the `ReadArgs` state), the
        // parser itself only fills args.
    }

    #[test]
    fn bare_string_lands_under_content() {
        let args = parse(r#""oiVv...==" "#);
        assert_eq!(args, vec![("content".to_string(), "oiVv...==".to_string())]);
    }

    #[test]
    fn empty_body_is_ok() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn parse_errors_match_the_cpp_messages() {
        fn error_of(body: &str) -> Status {
            let mut query = HttpQuery {
                body: body.as_bytes().to_vec(),
                ..HttpQuery::default()
            };
            match query.parse_json_parameters() {
                Result::Ok(()) => panic!("expected an error for {body}"),
                Result::Err(status) => status,
            }
        }

        assert_eq!(
            error_of("[]").message(),
            "Bad Request: JSON object expected"
        );
        assert_eq!(
            error_of("{\"a\" 1}").message(),
            "Bad Request: can't parse object, ':' expected"
        );
        assert_eq!(
            error_of("{\"a\"}").message(),
            "Bad Request: can't parse object, ':' expected"
        );
        assert_eq!(
            error_of("{\"a\": }").message(),
            "Bad Request: can't parse parameter value: Unexpected symbol '}'"
        );
        assert_eq!(
            error_of("{\"a\":1 \"b\":2}").message(),
            "Bad Request: expected next field or object end"
        );
        assert_eq!(
            error_of("{\"a\":1} x").message(),
            "Bad Request: unexpected data after object end"
        );
        assert_eq!(
            error_of("{\"a\":1,\"b\":2}!").message(),
            "Bad Request: unexpected data after object end"
        );
    }

    #[test]
    fn size_overflow_is_413() {
        let mut query = HttpQuery {
            body: vec![b' '; MAX_TOTAL_PARAMETERS_LENGTH + 1],
            ..HttpQuery::default()
        };
        match query.parse_json_parameters() {
            Result::Ok(()) => panic!("expected the 413"),
            Result::Err(status) => {
                assert_eq!(status.code(), 413);
                assert_eq!(
                    status.message(),
                    "Request Entity Too Large: too many parameters"
                );
            }
        }
    }
}
