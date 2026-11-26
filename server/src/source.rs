use serde::Serialize;
use url::{Host, Url};

pub(crate) const MAX_URL_BYTES: usize = 4096;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Source {
    Youtube { video_id: String },
    Mp4 { url: String },
    Hls { url: String },
    Dash { url: String },
}

impl Source {
    pub fn parse(input: &str) -> Option<Self> {
        let (url, _) = parse_web_url(input)?;
        let host = url.host()?;
        match url.scheme() {
            "https" => {}
            "http" if is_loopback(host) => {}
            _ => return None,
        }

        match url.host_str()? {
            "youtube.com" | "www.youtube.com" | "m.youtube.com" => {
                if url.port().is_some() {
                    return None;
                }
                if url.path() == "/watch" {
                    let mut ids = url.query_pairs()
                        .filter_map(|(key, value)| (key == "v").then_some(value));
                    let id = ids.next()?;
                    if ids.next().is_some() {
                        return None;
                    }
                    return youtube(&id);
                }
                let (kind, id) = url.path().strip_prefix('/')?.split_once('/')?;
                if matches!(kind, "shorts" | "live" | "embed") {
                    return youtube(id);
                }
                return None;
            }
            "youtu.be" if url.port().is_none() => return youtube(url.path().strip_prefix('/')?),
            "www.youtube-nocookie.com" if url.port().is_none() => {
                return youtube(url.path().strip_prefix("/embed/")?);
            }
            "youtu.be" | "www.youtube-nocookie.com" => return None,
            _ => {}
        }

        Format::from_path(&url).map(|format| format.source(url))
    }
}

pub(crate) fn parse_web_url(input: &str) -> Option<(Url, &str)> {
    if input.len() > MAX_URL_BYTES {
        return None;
    }
    let input = input.trim();
    let (scheme, address) = input.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") {
        return None;
    }
    let authority = address.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') || has_unsafe_chars(input) {
        return None;
    }
    let url = Url::parse(input).ok()?;
    (url.as_str().len() <= MAX_URL_BYTES).then_some((url, authority))
}

pub(crate) fn has_unsafe_chars(input: &str) -> bool {
    input.bytes().any(|b| b.is_ascii_control() || b == b'\\')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Mp4,
    Hls,
    Dash,
}

impl Format {
    pub(crate) fn from_path(url: &Url) -> Option<Self> {
        let (_, ext) = url.path().rsplit_once('.')?;
        if ext.eq_ignore_ascii_case("mp4") {
            Some(Self::Mp4)
        } else if ext.eq_ignore_ascii_case("m3u8") {
            Some(Self::Hls)
        } else if ext.eq_ignore_ascii_case("mpd") {
            Some(Self::Dash)
        } else {
            None
        }
    }

    pub(crate) fn source(self, url: Url) -> Source {
        let url = url.into();
        match self {
            Self::Mp4 => Source::Mp4 { url },
            Self::Hls => Source::Hls { url },
            Self::Dash => Source::Dash { url },
        }
    }
}

fn is_loopback(host: Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name == "localhost",
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

fn youtube(id: &str) -> Option<Source> {
    (id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| Source::Youtube { video_id: id.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_supported_youtube_forms_with_valid_ids() {
        for url in [
            "https://youtube.com/watch?v=abcDEF-123_",
            "https://www.youtube.com/shorts/abcDEF-123_?feature=share",
            "https://m.youtube.com/live/abcDEF-123_",
            "https://youtu.be/abcDEF-123_?si=value",
            "https://www.youtube.com/embed/abcDEF-123_",
            "https://www.youtube-nocookie.com/embed/abcDEF-123_",
        ] {
            assert!(matches!(Source::parse(url), Some(Source::Youtube { video_id }) if video_id == "abcDEF-123_"), "{url}");
        }
        for url in [
            "https://youtube.com.example/watch?v=abcDEF-123_",
            "https://other.youtube.com/watch?v=abcDEF-123_",
            "https://www.youtube.com:8443/watch?v=abcDEF-123_",
            "https://youtube.com./watch?v=abcDEF-123_",
            "https://www.youtube.com/watch?v=abcDEF-123_&v=abcDEF-123_",
            "https://youtu.be/abcDEF-123",
            "https://youtu.be/abcDEF-123_*",
            "https://youtu.be/abcDEF.123_",
            "https://youtu.be/abcDEF-123_/extra",
        ] {
            assert!(Source::parse(url).is_none(), "{url}");
        }
    }

    #[test]
    fn accepts_direct_formats_over_https_and_http_loopback_without_credentials() {
        let signed = "https://media.example/show.MP4?signature=a%2Fb";
        assert!(matches!(Source::parse(signed), Some(Source::Mp4 { url }) if url == signed));
        let dash = "https://media.example/show.MPD?signature=a%2Fb";
        assert!(matches!(Source::parse(dash), Some(Source::Dash { url }) if url == dash));
        for url in [
            "http://localhost:8081/live.m3u8",
            "http://127.0.0.2/live.m3u8",
            "http://[::1]/live.m3u8",
        ] {
            assert!(matches!(Source::parse(url), Some(Source::Hls { .. })), "{url}");
        }
        for url in [
            "https://media.example/page?url=sample.mp4",
            "https://user:pass@media.example/video.mp4",
            "https://@media.example/video.mp4",
            "http://localhost.example/live.m3u8",
            "http://10.0.0.1/video.mp4",
            "file:///video.mp4",
            "https:///media.example/video.mp4",
        ] {
            assert!(Source::parse(url).is_none(), "{url}");
        }
    }
}
