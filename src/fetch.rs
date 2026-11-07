//! SSRF-guarded page fetch, mirroring `tools/fetch.py`.
//!
//! Only public http(s) targets are fetched: every address that is not globally
//! routable - private, loopback, link-local, multicast, reserved, unspecified,
//! carrier-grade NAT and the IPv6 transition ranges - is rejected before any
//! request. The same predicate checks IP literals, every resolved address and
//! every redirect hop. The body is capped and reduced to its visible text.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::{redirect, Url};
use serde_json::{json, Value};

use crate::config::Settings;
use crate::reliability::truncate;

pub const MAX_BYTES: usize = 200 * 1024;
pub const MAX_REDIRECTS: usize = 3;
const USER_AGENT: &str = "Sol-O/1.0 (+tool fetch)";
const SKIP_TAGS: [&str; 4] = ["script", "style", "noscript", "template"];

/// Refusal, transport or status failure; `transient` failures get one retry.
struct Failure {
    message: String,
    transient: bool,
}

fn reject(reason: &str) -> Failure {
    Failure {
        message: format!("url rejected: {reason}"),
        transient: false,
    }
}

fn refused(error: String) -> Failure {
    Failure {
        message: error,
        transient: false,
    }
}

/// Fetch one public page and return `{url, status, text, truncated}`.
pub fn fetch_url(url: &str, settings: &Settings) -> Result<Value, String> {
    let cleaned = url.trim();
    if cleaned.is_empty() {
        return Err("empty url".to_string());
    }
    let timeout = Duration::from_secs_f64(settings.tool_timeout_seconds.max(0.001));
    let client = Client::builder()
        .timeout(timeout)
        .redirect(redirect::Policy::none())
        .build()
        .map_err(|error| format!("fetch failed: {error}"))?;
    // One retry on transport errors and 5xx, like the reference.
    let mut last = String::new();
    for attempt in 0..=1 {
        match fetch_once(&client, cleaned, settings) {
            Ok(page) => return Ok(page),
            Err(failure) => {
                let retry = failure.transient && attempt == 0;
                last = failure.message;
                if !retry {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    Err(last)
}

fn fetch_once(client: &Client, url: &str, settings: &Settings) -> Result<Value, Failure> {
    let mut current = Url::parse(url).map_err(|_| reject("unparseable url"))?;
    for _hop in 0..=MAX_REDIRECTS {
        assert_public_url(&current)?;
        let response = client
            .get(current.clone())
            .header("User-Agent", USER_AGENT)
            .send()
            .map_err(|error| Failure {
                message: format!("fetch failed: {error}"),
                transient: true,
            })?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            if let Some(location) = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
            {
                current = current
                    .join(location)
                    .map_err(|_| reject("unparseable url"))?;
                continue;
            }
        }
        if !(200..300).contains(&status) {
            return Err(Failure {
                message: format!("fetch failed: HTTP {status}"),
                transient: status >= 500,
            });
        }
        let (body, truncated) = read_capped(response).map_err(|error| Failure {
            message: format!("fetch failed: {error}"),
            transient: true,
        })?;
        let text = extract_text(&body);
        let (text, capped) = cap_text(text, settings.max_result_chars.max(0) as usize);
        return Ok(json!({
            "url": current.as_str(),
            "status": status,
            "text": text,
            "truncated": truncated || capped,
        }));
    }
    Err(reject(&format!("too many redirects (>{MAX_REDIRECTS})")))
}

/// Validate one hop against the reference's refusal rules.
fn assert_public_url(url: &Url) -> Result<(), Failure> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(reject(&format!("unsupported scheme '{scheme}'")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject("credentials in url"));
    }
    let Some(raw_host) = url.host_str() else {
        return Err(reject("missing host"));
    };
    let host = raw_host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(reject("missing host"));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    match host.parse::<IpAddr>() {
        Ok(address) => {
            if is_public_ip(address) {
                Ok(())
            } else {
                Err(reject("url is not publicly routable"))
            }
        }
        Err(_) => check_address(host, port),
    }
}

/// Reject any resolved address that is not publicly routable.
fn check_address(host: &str, port: u16) -> Result<(), Failure> {
    let addresses = match (host, port).to_socket_addrs() {
        Ok(addresses) => addresses.map(|address| address.ip()).collect::<Vec<_>>(),
        Err(error) => {
            return Err(refused(format!("could not resolve host {host}: {error}")));
        }
    };
    if addresses.is_empty() || !addresses.iter().all(|address| is_public_ip(*address)) {
        return Err(reject("url is not publicly routable"));
    }
    Ok(())
}

/// True only for a globally routable IP address; zone-scoped input fails closed.
fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    // The IANA special-purpose blocks: this-network, private, CGNAT, loopback,
    // link-local, documentation, benchmarking, 6to4 relay, multicast, reserved.
    !(octets[0] == 0
        || octets[0] == 10
        || (octets[0] == 100 && (64..128).contains(&octets[1]))
        || octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 172 && (16..32).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 192 && octets[1] == 168)
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 224)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
    {
        return false;
    }
    // IPv4-mapped/compatible, NAT64, Teredo and IETF assignments, 6to4,
    // documentation and benchmarking blocks.
    if segments[..5].iter().all(|segment| *segment == 0)
        || (segments[0] == 0x0064
            && segments[1] == 0xff9b
            && segments[2..6].iter().all(|s| *s == 0))
        || (segments[0] == 0x2001 && segments[1] <= 0x01ff)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xfff0) == 0)
    {
        return false;
    }
    // Global unicast 2000::/3 is the only remaining allocation.
    (segments[0] & 0xe000) == 0x2000
}

/// Read a response body, stopping at [`MAX_BYTES`]; true when more remained.
fn read_capped(mut reader: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    reader
        .by_ref()
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    let truncated = body.len() > MAX_BYTES;
    body.truncate(MAX_BYTES);
    Ok((body, truncated))
}

/// Cap visible text to `max_result_chars` characters; true when capped.
fn cap_text(text: String, max_result_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_result_chars {
        return (text, false);
    }
    (truncate(&text, max_result_chars), true)
}

/// Decode and reduce a body to visible text.
///
/// shortcut: bodies are decoded as UTF-8 only (the reference honours the
/// Content-Type charset), upgrade if non-UTF-8 pages matter.
fn extract_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body).into_owned();
    if !text.contains('<') {
        return text;
    }
    visible_text(&text)
}

fn visible_text(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut fragment = String::new();
    let mut skip_depth = 0usize;
    let mut index = 0;

    let flush = |fragment: &mut String, parts: &mut Vec<String>| {
        let collapsed = collapse_whitespace(&decode_entities(fragment));
        if !collapsed.is_empty() {
            parts.push(collapsed);
        }
        fragment.clear();
    };

    while index < characters.len() {
        let character = characters[index];
        if character == '<' {
            let next = characters.get(index + 1).copied();
            let tag_start = next
                .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!' | '?'));
            if !tag_start {
                if skip_depth == 0 {
                    fragment.push(character);
                }
                index += 1;
                continue;
            }
            flush(&mut fragment, &mut parts);
            if characters[index..].starts_with(&['<', '!', '-', '-']) {
                // Comments are not text and may contain tags of their own.
                match find_sequence(&characters, index + 4, &['-', '-', '>']) {
                    Some(at) => {
                        index = at + 3;
                        continue;
                    }
                    None => break,
                }
            }
            let Some(close) = tag_end(&characters, index) else {
                // An unterminated tag is data to the reference parser too.
                if skip_depth == 0 {
                    fragment.extend(characters[index..].iter());
                }
                break;
            };
            if characters[index..].starts_with(&['<', '!'])
                || characters[index..].starts_with(&['<', '?'])
            {
                index = close + 1;
                continue;
            }
            let closing = characters.get(index + 1) == Some(&'/');
            let self_closing = characters.get(close.wrapping_sub(1)) == Some(&'/');
            let name: String = characters[index + 1 + usize::from(closing)..close]
                .iter()
                .take_while(|character| character.is_ascii_alphanumeric() || **character == ':')
                .collect::<String>()
                .to_ascii_lowercase();
            if SKIP_TAGS.contains(&name.as_str()) && !self_closing {
                if closing {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
            }
            index = close + 1;
            continue;
        }
        if skip_depth == 0 {
            fragment.push(character);
        }
        index += 1;
    }
    flush(&mut fragment, &mut parts);
    parts.join(" ")
}

/// Index of `needle` in `characters` at or after `start`.
fn find_sequence(characters: &[char], start: usize, needle: &[char]) -> Option<usize> {
    (start..characters.len()).find(|at| characters[*at..].starts_with(needle))
}

/// Byte-free index of the `>` closing a tag, respecting quoted attribute values.
fn tag_end(characters: &[char], start: usize) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut index = start + 1;
    while index < characters.len() {
        let character = characters[index];
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == '>' => return Some(index),
            None => {}
        }
        index += 1;
    }
    None
}

fn collapse_whitespace(text: &str) -> String {
    let mut collapsed = String::new();
    let mut in_whitespace = false;
    for character in text.chars() {
        if matches!(character, ' ' | '\t' | '\r' | '\u{c}' | '\u{b}') {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            in_whitespace = false;
            collapsed.push(character);
        }
    }
    collapsed.trim().to_string()
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::new();
    let mut rest = text;
    while let Some(ampersand) = rest.find('&') {
        decoded.push_str(&rest[..ampersand]);
        let tail = &rest[ampersand..];
        let Some(semicolon) = tail.find(';').filter(|offset| *offset <= 32) else {
            decoded.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semicolon];
        let replacement = numeric_entity(name).or_else(|| named_entity(name));
        match replacement {
            Some(character) => {
                decoded.push(character);
                rest = &tail[semicolon + 1..];
            }
            None => {
                decoded.push('&');
                rest = &tail[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

fn numeric_entity(name: &str) -> Option<char> {
    let digits = name.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

fn named_entity(name: &str) -> Option<char> {
    // shortcut: only common named entities are decoded, upgrade when pages need the full HTML5 table.
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "middot" => '·',
        "bull" => '•',
        "times" => '×',
        "divide" => '÷',
        "euro" => '€',
        "pound" => '£',
        "sect" => '§',
        "para" => '¶',
        "laquo" => '«',
        "raquo" => '»',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/rust-tests/fixtures/page.html"
    ));

    #[test]
    fn captured_html_is_stripped_to_its_visible_text() {
        assert_eq!(
            extract_text(PAGE.as_bytes()),
            "Cafe delivery & returns Weekend delivery Orders placed before 14:00 ship the same day\u{a0}— later orders move to Monday. Refunds take five business days. Questions? Mail help@example.com ."
        );
    }

    #[test]
    fn a_body_without_tags_is_returned_verbatim() {
        assert_eq!(extract_text(b"plain &amp; text"), "plain &amp; text");
        assert_eq!(extract_text(b""), "");
    }

    #[test]
    fn numeric_entities_are_decoded_and_unknown_ones_stay_literal() {
        assert_eq!(
            extract_text(b"<p>caf&#233; &#x2014; &unknown; &amp;</p>"),
            "café — &unknown; &"
        );
    }

    #[test]
    fn script_and_style_subtrees_never_reach_the_text() {
        let page = "<div>before<script>alert('<p>x</p>')</script><style>p{}</style>after</div>";
        assert_eq!(extract_text(page.as_bytes()), "before after");
    }

    #[test]
    fn comments_are_dropped_even_when_they_contain_markup() {
        assert_eq!(extract_text(b"<p>a<!-- <b>hidden</b> -->b</p>"), "a b");
    }

    #[test]
    fn the_body_cap_stops_at_200_kib_and_reports_truncation() {
        let exact = read_capped(std::io::repeat(b'a').take(MAX_BYTES as u64)).unwrap();
        assert_eq!(exact.0.len(), MAX_BYTES);
        assert!(!exact.1);

        let over = read_capped(std::io::repeat(b'a').take(MAX_BYTES as u64 + 5)).unwrap();
        assert_eq!(over.0.len(), MAX_BYTES);
        assert!(over.1);
    }

    #[test]
    fn the_text_cap_counts_characters_and_reports_truncation() {
        let (text, truncated) = cap_text("x".repeat(20), 10);
        assert_eq!(text.chars().count(), 10);
        assert!(truncated);

        let (text, truncated) = cap_text("exactly-ten".to_string(), 11);
        assert_eq!(text, "exactly-ten");
        assert!(!truncated);
    }

    #[test]
    fn public_literals_pass_the_address_check_without_dns() {
        let public = Url::parse("https://93.184.216.34/page").unwrap();
        assert!(assert_public_url(&public).is_ok());

        let loopback = Url::parse("http://127.0.0.1/").unwrap();
        assert_eq!(
            assert_public_url(&loopback).unwrap_err().message,
            "url rejected: url is not publicly routable"
        );
    }
}
