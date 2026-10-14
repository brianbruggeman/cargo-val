use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    MissingUnit(String),
    UnknownUnit(String),
    BadNumber(String),
    BadRequestLine(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLine {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
}

pub fn parse_duration(text: &str) -> Result<Duration, ParseError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ParseError::Empty);
    }
    let split = text
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| ParseError::MissingUnit(text.to_string()))?;
    let (digits, unit) = text.split_at(split);
    let amount: u64 = digits.parse().map_err(|_| ParseError::BadNumber(digits.to_string()))?;
    match unit {
        "ms" => Ok(Duration::from_millis(amount)),
        "s" => Ok(Duration::from_secs(amount)),
        "m" => Ok(Duration::from_secs(amount * 60)),
        other => Err(ParseError::UnknownUnit(other.to_string())),
    }
}

pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < bytes.len() && hex_pair(&bytes[index + 1..index + 3]).is_some() => {
                decoded.extend(hex_pair(&bytes[index + 1..index + 3]));
                index += 2;
            }
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
    let text = std::str::from_utf8(pair).ok()?;
    u8::from_str_radix(text, 16).ok()
}

pub fn parse_query(query: &str) -> BTreeMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

pub fn parse_request_line(line: &str) -> Result<RequestLine, ParseError> {
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some("HTTP/1.1"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ParseError::BadRequestLine(line.to_string()));
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(RequestLine { method: method.to_string(), path: path.to_string(), query: parse_query(query) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_reads_milliseconds() {
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
    }

    #[test]
    fn duration_reads_seconds() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
    }

    #[test]
    fn duration_reads_minutes() {
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
    }

    #[test]
    fn duration_trims_surrounding_whitespace() {
        assert_eq!(parse_duration("  5s\n"), Ok(Duration::from_secs(5)));
    }

    #[test]
    fn duration_rejects_empty_input() {
        assert_eq!(parse_duration("   "), Err(ParseError::Empty));
    }

    #[test]
    fn duration_rejects_a_bare_number() {
        assert_eq!(parse_duration("90"), Err(ParseError::MissingUnit("90".into())));
    }

    #[test]
    fn duration_rejects_unknown_units() {
        assert_eq!(parse_duration("4h"), Err(ParseError::UnknownUnit("h".into())));
    }

    #[test]
    fn duration_rejects_missing_digits() {
        assert_eq!(parse_duration("ms"), Err(ParseError::BadNumber(String::new())));
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
    }

    #[test]
    fn percent_decode_keeps_a_dangling_percent() {
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn query_splits_pairs_in_key_order() {
        let query = parse_query("zone=eu&page=2");
        assert_eq!(query.keys().collect::<Vec<_>>(), ["page", "zone"]);
    }

    #[test]
    fn query_defaults_a_flag_to_the_empty_string() {
        assert_eq!(parse_query("verbose").get("verbose").map(String::as_str), Some(""));
    }

    #[test]
    fn query_ignores_empty_segments() {
        assert_eq!(parse_query("&&a=1&").len(), 1);
    }

    #[test]
    fn request_line_splits_method_path_and_query() {
        let line = parse_request_line("GET /items/42?fields=name%2Cid HTTP/1.1").expect("a well formed line parses");
        assert_eq!(line.method, "GET");
        assert_eq!(line.path, "/items/42");
        assert_eq!(line.query.get("fields").map(String::as_str), Some("name,id"));
    }

    #[test]
    fn request_line_rejects_http_two_preface() {
        assert!(parse_request_line("PRI * HTTP/2.0").is_err());
    }
}
