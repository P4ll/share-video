use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::{Arc, LazyLock},
    time::Duration,
};

use reqwest::{
    Client, Response, StatusCode,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{ACCEPT_ENCODING, CONTENT_TYPE, HeaderName, LOCATION},
};
use scraper::{Html, Selector};
use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};
use url::{Host, Url};

use crate::source::{Format, MAX_URL_BYTES, Source, has_unsafe_chars, parse_web_url};

const MAX_CONCURRENT: usize = 8;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REDIRECTS: usize = 3;
const MAX_REQUESTS: usize = 12;
const MAX_PINS: usize = 8;
const MAX_CANDIDATES: usize = 5;
const MAX_HTML_BYTES: usize = 512 * 1024;
const PROBE_BYTES: usize = 1024;

#[derive(Clone)]
pub(crate) struct Extractor(&'static Semaphore);

impl Default for Extractor {
    fn default() -> Self {
        static PERMITS: Semaphore = Semaphore::const_new(MAX_CONCURRENT);
        Self(&PERMITS)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    Invalid,
    Unavailable,
    Busy,
}

impl Extractor {
    pub(crate) async fn resolve(&self, input: &str) -> Result<Source, ResolveError> {
        if let Some(source) = Source::parse(input) {
            return Ok(source);
        }
        let url = public_url(input).ok_or(ResolveError::Invalid)?;
        let permit = Arc::new(self.0.try_acquire().map_err(|_| ResolveError::Busy)?);
        tokio::time::timeout(TOTAL_TIMEOUT, resolve_page(url, permit))
            .await
            .ok()
            .flatten()
            .ok_or(ResolveError::Unavailable)
    }
}

async fn resolve_page(url: Url, permit: Arc<SemaphorePermit<'static>>) -> Option<Source> {
    let client = Client::builder()
        .dns_resolver(Arc::new(PublicDns(Arc::clone(&permit))))
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        // Every hop gets a fresh connection and therefore a fresh DNS check.
        .pool_max_idle_per_host(0)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;
    let mut remaining = MAX_REQUESTS;
    let mut response = get(&client, url, &mut remaining).await?;
    let mime = content_type(&response)?;
    if let Some(probe) = probe_policy(response.status(), &mime, Format::from_path(response.url())) {
        return verify(response, probe).await;
    }
    if response.status() != StatusCode::OK
        || !matches!(mime.essence_str(), "text/html" | "application/xhtml+xml")
        || response.content_length().is_some_and(|n| n > MAX_HTML_BYTES as u64)
    {
        return None;
    }

    let mut html = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if html.len() + chunk.len() > MAX_HTML_BYTES {
            return None;
        }
        html.extend_from_slice(&chunk);
    }
    let page = response.url().clone();
    drop(response);
    let candidates = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        media_candidates(&String::from_utf8_lossy(&html), &page)
    })
    .await
    .ok()?;

    for (url, hint) in candidates {
        let Some(response) = get(&client, url, &mut remaining).await else {
            continue;
        };
        let hint = Format::from_path(response.url()).or(hint);
        if let Some(probe) = content_type(&response).and_then(|mime| probe_policy(response.status(), &mime, hint))
            && let Some(source) = verify(response, probe).await
        {
            return Some(source);
        }
    }
    None
}

async fn get(client: &Client, mut url: Url, remaining: &mut usize) -> Option<Response> {
    for hop in 0..=MAX_REDIRECTS {
        *remaining = remaining.checked_sub(1)?;
        let response = client.get(url).header(ACCEPT_ENCODING, "identity").send().await.ok()?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Some(response);
        }
        if hop == MAX_REDIRECTS {
            break;
        }
        url = join_public_url(response.url(), singleton_header(&response, LOCATION)?)?;
    }
    None
}

fn singleton_header(response: &Response, name: HeaderName) -> Option<&str> {
    let values = response.headers().get_all(name);
    let mut values = values.iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn content_type(response: &Response) -> Option<mime::Mime> {
    singleton_header(response, CONTENT_TYPE)?.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Known(Format),
    Signature(Format),
}

fn media_mime(mime: &mime::Mime) -> Option<Format> {
    match mime.essence_str() {
        "video/mp4" | "application/mp4" => Some(Format::Mp4),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" | "application/mpegurl"
        | "audio/mpegurl" | "audio/x-mpegurl" => Some(Format::Hls),
        "application/dash+xml" => Some(Format::Dash),
        _ => None,
    }
}

fn probe_policy(status: StatusCode, mime: &mime::Mime, hint: Option<Format>) -> Option<Probe> {
    if !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        return None;
    }
    if let Some(format) = media_mime(mime) {
        return Some(Probe::Known(format));
    }
    let format = hint?;
    match (mime.essence_str(), format) {
        ("application/octet-stream", _)
        | ("text/plain", Format::Hls)
        | ("text/xml" | "application/xml", Format::Dash) => Some(Probe::Signature(format)),
        _ => None,
    }
}

async fn verify(mut response: Response, probe: Probe) -> Option<Source> {
    let format = match probe {
        Probe::Known(format) => format,
        Probe::Signature(format) => {
            let mut prefix = [0; PROBE_BYTES];
            let mut len = 0;
            while len < prefix.len() {
                let Some(chunk) = response.chunk().await.ok()? else {
                    break;
                };
                let n = chunk.len().min(prefix.len() - len);
                prefix[len..len + n].copy_from_slice(&chunk[..n]);
                len += n;
                if has_signature(format, &prefix[..len]) {
                    return Some(format.source(response.url().clone()));
                }
            }
            return None;
        }
    };
    Some(format.source(response.url().clone()))
}

fn has_signature(format: Format, prefix: &[u8]) -> bool {
    match format {
        Format::Mp4 => prefix.len() >= 8 && matches!(&prefix[4..8], b"ftyp" | b"styp"),
        Format::Hls => {
            let prefix = prefix.strip_prefix(b"\xef\xbb\xbf").unwrap_or(prefix);
            matches!(prefix.strip_prefix(b"#EXTM3U"), Some([b'\r' | b'\n', ..]))
        }
        Format::Dash => {
            let text = String::from_utf8_lossy(prefix);
            let mut text = text.trim_start_matches('\u{feff}').trim_start();
            if let Some(declaration) = text.strip_prefix("<?xml") {
                let Some((_, rest)) = declaration.split_once("?>") else { return false };
                text = rest.trim_start();
            }
            while let Some(comment) = text.strip_prefix("<!--") {
                let Some((_, rest)) = comment.split_once("-->") else { return false };
                text = rest.trim_start();
            }
            let Some(tag) = text.strip_prefix('<') else { return false };
            let Some(end) = tag.find(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '>' | '/')) else {
                return false;
            };
            tag[..end].rsplit(':').next() == Some("MPD")
        }
    }
}

fn media_candidates(html: &str, page: &Url) -> Vec<(Url, Option<Format>)> {
    static TAGS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("video[src], video source[src], script[type]").expect("constant selector")
    });
    let document = Html::parse_document(html);
    let mut candidates = Vec::new();
    // shortcut: resolve against the final page URL; support a validated <base> if sites need it.
    let mut add = |reference: &str, hint| {
        if let Some(url) = join_public_url(page, reference) && &url != page {
            let hint = Format::from_path(&url).or(hint);
            if let Some((_, existing)) = candidates.iter_mut().find(|(prior, _)| prior == &url) {
                *existing = Option::or(*existing, hint);
            } else {
                candidates.push((url, hint));
            }
        }
        candidates.len() == MAX_CANDIDATES
    };
    for element in document.select(&TAGS) {
        let node = element.value();
        let mime = node.attr("type").and_then(|mime| mime.parse::<mime::Mime>().ok());
        let done = if node.name() != "script" {
            add(node.attr("src").expect("selector requires src"), mime.as_ref().and_then(media_mime))
        } else if mime.as_ref().is_some_and(|mime| mime.essence_str() == "application/ld+json") {
            let text = element.text().collect::<String>();
            serde_json::from_str(&text).is_ok_and(|value| add_json_ld(&value, &mut add))
        } else {
            false
        };
        if done {
            break;
        }
    }
    candidates
}

fn add_json_ld(value: &Value, add: &mut impl FnMut(&str, Option<Format>) -> bool) -> bool {
    if let Some(items) = value.as_array() {
        return items.iter().any(|item| add_json_ld(item, add));
    }
    let video_type = |kind: &Value| {
        matches!(kind.as_str(), Some("VideoObject" | "https://schema.org/VideoObject" | "http://schema.org/VideoObject"))
    };
    if value.get("@type").is_some_and(|kind| video_type(kind) || kind.as_array().is_some_and(|kinds| kinds.iter().any(video_type)))
        && let Some(content) = value.get("contentUrl")
    {
        let hint = value.get("encodingFormat").and_then(Value::as_str)
            .and_then(|mime| mime.parse().ok()).as_ref().and_then(media_mime);
        let mut add_item = |item: &Value| item.as_str().is_some_and(|url| add(url, hint));
        if add_item(content) || content.as_array().is_some_and(|items| items.iter().any(add_item)) {
            return true;
        }
    }
    value.get("@graph").is_some_and(|graph| add_json_ld(graph, add))
}

fn public_url(input: &str) -> Option<Url> {
    if has_unsafe_chars(input) {
        return None;
    }
    let (mut url, authority) = parse_web_url(input)?;
    if url.scheme() != "https" || !authority.is_ascii() || authority.contains('%') {
        return None;
    }
    let raw_host = if authority.ends_with(']') {
        authority
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if port != "443" {
            return None;
        }
        host
    } else {
        authority
    };
    let allowed = match url.host()? {
        Host::Ipv4(ip) => raw_host.parse::<Ipv4Addr>().ok() == Some(ip) && is_public(ip.into()),
        Host::Ipv6(ip) => is_public(ip.into()),
        Host::Domain(host) => raw_host.eq_ignore_ascii_case(host) && public_domain(host),
    };
    url.set_fragment(None);
    allowed.then_some(url)
}

fn public_domain(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty() && label.len() <= 63 && !label.starts_with('-') && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && !["localhost", "local", "youtube.com", "youtu.be", "youtube-nocookie.com", "rutube.ru"]
            .iter().any(|&root| host == root || host.strip_suffix(root).is_some_and(|prefix| prefix.ends_with('.')))
}

fn join_public_url(base: &Url, reference: &str) -> Option<Url> {
    if reference.len() > MAX_URL_BYTES || has_unsafe_chars(reference) {
        return None;
    }
    let reference = reference.trim();
    if reference.is_empty() {
        return None;
    }
    if let Some(authority_and_path) = reference.strip_prefix("//") {
        return public_url(&format!("https://{authority_and_path}"));
    }
    if reference.split(['/', '?', '#']).next()?.contains(':') {
        return public_url(reference);
    }
    // The base was checked, and this reference has no scheme or authority.
    let mut url = base.join(reference).ok()?;
    url.set_fragment(None);
    (url.as_str().len() <= MAX_URL_BYTES).then_some(url)
}

struct PublicDns(Arc<SemaphorePermit<'static>>);

impl Resolve for PublicDns {
    fn resolve(&self, name: Name) -> Resolving {
        let permit = Arc::clone(&self.0);
        Box::pin(async move {
            let addresses = tokio::task::spawn_blocking(move || {
                // getaddrinfo can outlive a cancelled request; keep its concurrency slot.
                let _permit = permit;
                let name = format!("{}.", name.as_str());
                pin_addresses((name.as_str(), 443).to_socket_addrs()?)
            }).await??;
            // The connector consumes these exact addresses; it never resolves the name again.
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

fn pin_addresses(addresses: impl IntoIterator<Item = SocketAddr>) -> io::Result<Vec<SocketAddr>> {
    let mut pins = Vec::new();
    for address in addresses {
        if !is_public(address.ip()) {
            return Err(io::Error::other("DNS answer contains a nonpublic address"));
        }
        if pins.len() < MAX_PINS && !pins.contains(&address) {
            pins.push(address);
        }
    }
    if pins.is_empty() {
        return Err(io::Error::other("DNS answer has no addresses"));
    }
    Ok(pins)
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !matches!(
            ip.octets(),
            [0 | 10 | 127 | 224..=255, ..]
                | [100, 64..=127, ..]
                | [169, 254, ..]
                | [172, 16..=31, ..]
                | [192, 0, 0 | 2, _]
                | [192, 88, 99, _]
                | [192, 168, ..]
                | [198, 18 | 19, ..]
                | [198, 51, 100, _]
                | [203, 0, 113, _]
        ),
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return is_public(ip.into());
            }
            // Global unicast only; exclude special protocols, documentation and 6to4.
            let [first, second, ..] = ip.segments();
            (0x2000..=0x3fff).contains(&first)
                && !matches!((first, second), (0x2001, 0..=0x01ff | 0x0db8) | (0x2002 | 0x3fff, _))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_only_media_markup_and_video_objects_in_order() {
        let final_page = Url::parse("https://pages.example/series/episode").unwrap();
        let candidates = media_candidates(
            r#"
                <base href="https://ignored.example/">
                <iframe src="https://player.example/index"></iframe>
                <script src="https://scripts.example/player.js">let url = 'wrong.mp4';</script>
                <picture><source src="wrong.mp4"></picture>
                <video SRC="../play?v=1&amp;sig=a">
                    <source src="//media.example/hls" type="application/x-mpegURL">
                    <source src="http://unsafe.example/cross-origin.mp4">
                    <source src="//[::ffff:127.0.0.1]/private.mp4">
                    <source src="../play?v=1&amp;sig=a" type="video/mp4">
                </video>
                <script type="application/ld+json">{broken json}</script>
                <script type="application/ld+json; charset=utf-8">
                    [
                        {"@type":"ImageObject","contentUrl":"wrong.mp4"},
                        {"@graph":[{"@type":["Thing","https://schema.org/VideoObject"],
                            "embedUrl":"wrong.mp4", "encodingFormat":"application/dash+xml",
                            "contentUrl":["./manifest","https://cdn.example/alternative.mpd"]}]}
                    ]
                </script>
            "#,
            &final_page,
        );
        let actual: Vec<_> = candidates.iter().map(|(url, hint)| (url.as_str(), *hint)).collect();
        assert_eq!(actual, [
            ("https://pages.example/play?v=1&sig=a", Some(Format::Mp4)),
            ("https://media.example/hls", Some(Format::Hls)),
            ("https://pages.example/series/manifest", Some(Format::Dash)),
            ("https://cdn.example/alternative.mpd", Some(Format::Dash)),
        ]);
    }

    #[test]
    fn limits_candidate_count() {
        let page = Url::parse("https://pages.example/").unwrap();
        let html: String = (0..10).map(|i| format!(r#"<video src="{i}.mp4"></video>"#)).collect();
        let candidates = media_candidates(&html, &page);
        assert_eq!(candidates.len(), MAX_CANDIDATES);
        assert_eq!(candidates.last().unwrap().0.path(), "/4.mp4");
    }

    #[test]
    fn enforces_public_https_for_input_and_redirects() {
        let base = public_url("https://pages.example:443/catalog/page").unwrap();
        assert_eq!(base.as_str(), "https://pages.example/catalog/page");
        assert_eq!(
            join_public_url(&base, "../asset?key=a%2Fb").unwrap().as_str(),
            "https://pages.example/asset?key=a%2Fb"
        );
        for url in [
            "https://media.example/path",
            "https://8.8.8.8/path",
            "https://[2606:4700::1111]/path",
            "https://[::ffff:8.8.8.8]/path",
            "https://notyoutube.com/path",
        ] {
            assert!(public_url(url).is_some(), "{url}");
        }
        for url in [
            "http://media.example/path",
            "https://media.example:8443/path",
            "https://media.example:/path",
            "https://media.example:0443/path",
            "https:///media.example/path",
            "https://user:pass@media.example/path",
            "https://@media.example/path",
            "https://me%64ia.example/path",
            "https://media.example\\@pages.example/path",
            "https://me\tdia.example/path",
            "https://intranet/path",
            "https://internal.local/path",
            "https://localhost/path",
            "https://127.1/path",
            "https://0x08080808/path",
            "https://169.254.169.254/path",
            "https://[::1]/path",
            "https://[::ffff:192.168.1.1]/path",
            "https://www.youtube.com/watch?v=invalid",
            "https://youtube.com./watch?v=invalid",
            "https://youtu.be/invalid",
            "https://abc.youtube-nocookie.com/invalid",
            "https://rutube.ru/video/example",
            "https://cdn.rutube.ru/embed/example",
        ] {
            assert!(public_url(url).is_none(), "{url}");
            assert!(join_public_url(&base, url).is_none(), "{url}");
        }
        for reference in ["//127.0.0.1/private", "///media.example/path", "//@media.example/path", "https:other"] {
            assert!(join_public_url(&base, reference).is_none(), "{reference}");
        }
        assert!(public_url(&format!("https://media.example/{}", "a".repeat(MAX_URL_BYTES))).is_none());
    }

    #[test]
    fn pins_only_public_dns_answers_and_rejects_mixed_answers() {
        for ip in [
            "0.1.2.3", "10.0.0.1", "100.64.0.1", "172.16.0.1", "192.0.0.1", "192.0.2.1",
            "192.88.99.1", "198.18.0.1", "198.51.100.1", "203.0.113.1", "224.0.0.1",
            "255.255.255.255", "::", "::ffff:127.0.0.1", "64:ff9b::7f00:1", "2001::1",
            "2001:db8::1", "2002:7f00:1::", "3fff::1", "fc00::1", "fe80::1", "ff00::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        let ip4 = "8.8.8.8:443".parse().unwrap();
        let ip6 = "[2606:4700::1111]:443".parse().unwrap();
        let link_local = "169.254.169.254:443".parse().unwrap();
        assert_eq!(pin_addresses([ip4, ip6, ip4]).unwrap(), [ip4, ip6]);
        assert!(pin_addresses([ip4, link_local]).is_err());
        assert!(pin_addresses([]).is_err());
        let addresses = (1..=9).map(|n| SocketAddr::from(([8, 8, 8, n], 443)));
        assert_eq!(pin_addresses(addresses.clone()).unwrap().len(), MAX_PINS);
        assert!(pin_addresses(addresses.chain([link_local])).is_err());
    }

    #[test]
    fn probe_requires_success_and_credible_media_type() {
        let probe = |status, mime: &str, hint| probe_policy(status, &mime.parse().unwrap(), hint);
        assert_eq!(probe(StatusCode::OK, "Video/MP4; codecs=avc1", None), Some(Probe::Known(Format::Mp4)));
        assert_eq!(probe(StatusCode::PARTIAL_CONTENT, "application/vnd.apple.mpegurl", None), Some(Probe::Known(Format::Hls)));
        assert_eq!(probe(StatusCode::OK, "application/dash+xml", None), Some(Probe::Known(Format::Dash)));
        assert_eq!(probe(StatusCode::OK, "application/octet-stream", Some(Format::Mp4)), Some(Probe::Signature(Format::Mp4)));
        assert_eq!(probe(StatusCode::OK, "application/xml", Some(Format::Dash)), Some(Probe::Signature(Format::Dash)));
        assert_eq!(probe(StatusCode::OK, "text/plain", Some(Format::Hls)), Some(Probe::Signature(Format::Hls)));
        assert_eq!(probe(StatusCode::OK, "application/octet-stream", None), None);
        assert_eq!(probe(StatusCode::OK, "text/html", Some(Format::Mp4)), None);
        assert_eq!(probe(StatusCode::NOT_FOUND, "video/mp4", None), None);
        assert_eq!(probe(StatusCode::NO_CONTENT, "video/mp4", None), None);
        assert!(has_signature(Format::Mp4, b"\0\0\0\x10ftypisom"));
        assert!(has_signature(Format::Hls, b"#EXTM3U\n#EXT-X-ENDLIST"));
        assert!(!has_signature(Format::Hls, b"#EXTM3UX\n"));
        assert!(has_signature(Format::Dash, b"<?xml version='1.0'?><!--vod--><MPD type='static'>"));
        for format in [Format::Mp4, Format::Dash, Format::Hls] {
            assert!(!has_signature(format, b"<!doctype html><title>Not a media file</title>"));
        }
    }

    #[tokio::test]
    async fn direct_sources_and_invalid_inputs_skip_the_network_budget() {
        static FULL: Semaphore = Semaphore::const_new(0);
        let extractor = Extractor(&FULL);
        assert!(std::ptr::eq(Extractor::default().0, Extractor::default().0));
        assert!(matches!(
            extractor.resolve("http://localhost:8080/media.mpd").await,
            Ok(Source::Dash { .. })
        ));
        assert!(matches!(
            extractor.resolve("https://youtu.be/abcDEF-123_").await,
            Ok(Source::Youtube { .. })
        ));
        assert!(matches!(
            extractor.resolve("https://rutube.ru/video/example").await,
            Err(ResolveError::Invalid)
        ));
        assert!(matches!(
            extractor.resolve("https://media.example/watch").await,
            Err(ResolveError::Busy)
        ));
    }
}
