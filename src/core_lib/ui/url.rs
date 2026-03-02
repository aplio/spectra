use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlSpan {
    pub start: usize,
    pub end: usize,
}

impl UrlSpan {
    pub fn contains_byte(self, byte: usize) -> bool {
        self.start <= byte && byte < self.end
    }

    pub fn as_str(self, text: &str) -> &str {
        &text[self.start..self.end]
    }
}

pub fn find_web_url_spans(text: &str) -> Vec<UrlSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let Some(scheme_len) = scheme_len_at(bytes, i) else {
            i += 1;
            continue;
        };

        if !is_boundary_before(bytes, i) {
            i += 1;
            continue;
        }

        let mut end = i + scheme_len;
        while end < bytes.len() {
            let b = bytes[end];
            if is_url_terminator_byte(b) {
                break;
            }
            end += 1;
        }

        end = trim_trailing_punctuation(bytes, i, end);
        if end > i + scheme_len {
            spans.push(UrlSpan { start: i, end });
            i = end;
        } else {
            i += scheme_len;
        }
    }

    spans
}

pub fn write_hyperlink_open<W: Write>(writer: &mut W, url: &str) -> io::Result<()> {
    write!(writer, "\x1b]8;;{url}\x1b\\")
}

pub fn write_hyperlink_close<W: Write>(writer: &mut W) -> io::Result<()> {
    write!(writer, "\x1b]8;;\x1b\\")
}

fn scheme_len_at(bytes: &[u8], start: usize) -> Option<usize> {
    if starts_with_ignore_ascii_case(bytes, start, b"https://") {
        Some(8)
    } else if starts_with_ignore_ascii_case(bytes, start, b"http://") {
        Some(7)
    } else {
        None
    }
}

fn starts_with_ignore_ascii_case(bytes: &[u8], start: usize, needle: &[u8]) -> bool {
    let end = start.saturating_add(needle.len());
    if end > bytes.len() {
        return false;
    }
    bytes[start..end].eq_ignore_ascii_case(needle)
}

fn is_boundary_before(bytes: &[u8], start: usize) -> bool {
    if start == 0 {
        return true;
    }
    let prev = bytes[start - 1];
    if prev.is_ascii_alphanumeric() {
        return false;
    }
    !matches!(prev, b'/' | b'_' | b'-' | b'.')
}

fn is_url_terminator_byte(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b'<' | b'>' | b'"' | b'\'' | b'`')
}

fn trim_trailing_punctuation(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start {
        match bytes[end - 1] {
            b'.' | b',' | b';' | b':' | b'!' | b'?' => {
                end -= 1;
            }
            b')' => {
                if trailing_closer_is_unbalanced(bytes, start, end, b'(', b')') {
                    end -= 1;
                } else {
                    break;
                }
            }
            b']' => {
                if trailing_closer_is_unbalanced(bytes, start, end, b'[', b']') {
                    end -= 1;
                } else {
                    break;
                }
            }
            b'}' => {
                if trailing_closer_is_unbalanced(bytes, start, end, b'{', b'}') {
                    end -= 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    end
}

fn trailing_closer_is_unbalanced(
    bytes: &[u8],
    start: usize,
    end: usize,
    open: u8,
    close: u8,
) -> bool {
    let candidate = &bytes[start..end];
    let opens = candidate.iter().filter(|&&b| b == open).count();
    let closes = candidate.iter().filter(|&&b| b == close).count();
    closes > opens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_basic_http_and_https_urls() {
        let text = "http://a.test and https://b.test/x";
        let spans = find_web_url_spans(text);
        let urls: Vec<&str> = spans.iter().map(|s| s.as_str(text)).collect();
        assert_eq!(urls, vec!["http://a.test", "https://b.test/x"]);
    }

    #[test]
    fn detects_scheme_case_insensitively() {
        let text = "Visit HTTPS://Example.com/docs";
        let spans = find_web_url_spans(text);
        let urls: Vec<&str> = spans.iter().map(|s| s.as_str(text)).collect();
        assert_eq!(urls, vec!["HTTPS://Example.com/docs"]);
    }

    #[test]
    fn trims_common_trailing_punctuation() {
        let text = "see https://example.com/docs), then";
        let spans = find_web_url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].as_str(text), "https://example.com/docs");
    }

    #[test]
    fn keeps_balanced_parentheses() {
        let text = "https://example.com/path_(abc)";
        let spans = find_web_url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].as_str(text), text);
    }

    #[test]
    fn keeps_balanced_brackets_for_ipv6_host_literals() {
        let text = "http://[::1]";
        let spans = find_web_url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].as_str(text), text);
    }

    #[test]
    fn ignores_embedded_word_like_prefix() {
        let text = "abchttps://example.com";
        assert!(find_web_url_spans(text).is_empty());
    }

    #[test]
    fn hyperlink_sequences_are_written() {
        let mut out = Vec::new();
        write_hyperlink_open(&mut out, "https://example.com").expect("open");
        write_hyperlink_close(&mut out).expect("close");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "\x1b]8;;https://example.com\x1b\\\x1b]8;;\x1b\\"
        );
    }
}
