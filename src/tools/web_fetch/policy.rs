//! Network-independent URL and response-classification policy for `web_fetch`.

use anyhow::Result;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use url::{Host, Url};

pub(crate) const MAX_URL_LENGTH: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Html,
    Text,
}

pub(crate) fn parse_fetch_url(input: &str) -> Result<Url> {
    if input.len() > MAX_URL_LENGTH {
        anyhow::bail!("URL exceeds the maximum length of {MAX_URL_LENGTH}");
    }
    let url = Url::parse(input).map_err(|error| anyhow::anyhow!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!(
            "unsupported URL scheme \"{}\" (only http and https are allowed)",
            url.scheme()
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("credentials in URLs are not allowed");
    }
    Ok(url)
}

pub(crate) fn ip_literal(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        Host::Domain(_) => None,
    }
}

/// Whether an address is a globally reachable unicast destination.
///
/// The check is deliberately conservative: transition, translation, document,
/// benchmarking, private, loopback, link-local, multicast and reserved ranges
/// are refused. IPv4-mapped IPv6 is classified by its embedded IPv4 address.
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if octets[0] == 0
        || octets[0] == 10
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || octets[0] == 127
        || (octets[0] == 169 && octets[1] == 254)
        || (octets[0] == 172 && (16..=31).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 192 && octets[1] == 168)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        || octets[0] >= 224
    {
        return false;
    }
    true
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_public_ipv4(ipv4);
    }
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let segments = ip.segments();
    let global_unicast = (segments[0] & 0xe000) == 0x2000;
    if !global_unicast {
        return false;
    }
    if (segments[0] == 0x2001
        && (segments[1] == 0
            || segments[1] == 0x0002
            || segments[1] == 0x0db8
            || (segments[1] & 0xfff0) == 0x0010
            || (segments[1] & 0xfff0) == 0x0020))
        || segments[0] == 0x2002
        || (segments[0] & 0xfff0) == 0x3ff0
    {
        return false;
    }
    true
}

pub(crate) fn same_origin(left: &Url, right: &Url) -> bool {
    left.origin() == right.origin()
}

pub(crate) fn resolve_redirect(location: &str, base: &Url) -> Result<Url> {
    base.join(location)
        .map_err(|error| anyhow::anyhow!("invalid redirect Location: {error}"))
}

pub(crate) fn classify_content_type(content_type: Option<&str>) -> Option<ContentKind> {
    let mime = content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return Some(ContentKind::Html);
    }
    if mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
    {
        return Some(ContentKind::Text);
    }
    None
}

pub(crate) fn parse_charset(content_type: Option<&str>) -> Option<String> {
    content_type?
        .split(';')
        .skip(1)
        .find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"').to_ascii_lowercase())
        })
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_and_credentialed_urls() {
        assert!(parse_fetch_url("ftp://example.com/").is_err());
        assert!(parse_fetch_url("file:///etc/passwd").is_err());
        assert!(parse_fetch_url("https://user:pass@example.com/").is_err());
        assert!(parse_fetch_url("https://example.com/").is_ok());
    }

    #[test]
    fn rejects_private_loopback_and_reserved_ipv4() {
        for ip in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn accepts_public_ipv4() {
        for ip in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn rejects_private_loopback_and_transition_ipv6() {
        for ip in [
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "2002::1",
            "64:ff9b::1.1.1.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn accepts_public_ipv6_and_public_mapped_ipv4() {
        for ip in ["2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        let a = Url::parse("https://example.com/a").unwrap();
        let b = Url::parse("https://example.com/b").unwrap();
        let c = Url::parse("http://example.com/b").unwrap();
        let d = Url::parse("https://example.com:8443/b").unwrap();
        assert!(same_origin(&a, &b));
        assert!(!same_origin(&a, &c));
        assert!(!same_origin(&a, &d));
    }

    #[test]
    fn classifies_html_text_and_binary() {
        assert_eq!(
            classify_content_type(Some("text/html; charset=utf-8")),
            Some(ContentKind::Html)
        );
        assert_eq!(
            classify_content_type(Some("application/json")),
            Some(ContentKind::Text)
        );
        assert_eq!(classify_content_type(Some("image/png")), None);
        assert_eq!(classify_content_type(None), None);
    }

    #[test]
    fn parses_charset_parameter() {
        assert_eq!(
            parse_charset(Some("text/html; charset=\"UTF-8\"")).as_deref(),
            Some("utf-8")
        );
        assert_eq!(parse_charset(Some("text/html")), None);
    }

    #[test]
    fn resolves_relative_redirects() {
        let base = Url::parse("https://example.com/a/b").unwrap();
        assert_eq!(
            resolve_redirect("../c", &base).unwrap().as_str(),
            "https://example.com/c"
        );
    }
}
