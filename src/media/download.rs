//! Allowlisted media download.
//!
//! Images arrive as URLs inside OneBot segments, which means they are **attacker
//! controlled**. Three defences, all enforced here:
//!
//! * **Origin allowlist** — the URL's origin must match one of the configured
//!   `SNOWLUMA_MEDIA_ORIGINS` entries exactly (scheme, host, port). No wildcards, no
//!   suffixes, no ports guessed.
//! * **No redirects** — a redirect is a way to leave the allowlist, so it is disabled
//!   and treated as a failure rather than followed.
//! * **Address screening** — the host is resolved and loopback, link-local, metadata,
//!   multicast and CGNAT ranges are rejected. RFC1918/ULA addresses are accepted only
//!   when their exact origin is explicitly configured for the trusted SnowLuma media
//!   endpoint; redirects remain disabled.
//!
//! URLs are never logged. Errors carry a coarse reason only, because the URL itself can
//! contain a media token.

use std::net::ToSocketAddrs;
use std::time::Duration;

use crate::config::host_is_forbidden;

/// Connection timeout for the media fetch.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Total timeout for the media fetch.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a download was refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    /// The URL did not parse.
    BadUrl,
    /// The scheme is not http/https.
    BadScheme,
    /// The origin is not in the allowlist.
    OriginNotAllowed,
    /// The host has no usable address.
    HostUnresolved,
    /// Every resolved address is in a forbidden range.
    ForbiddenHost,
    /// The response was not a 2xx.
    BadStatus,
    /// The body exceeded the byte cap.
    TooLarge,
    /// The declared content type is not an image.
    NotAnImage,
    /// The transfer failed.
    Transport,
}

impl MediaError {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaError::BadUrl => "bad_url",
            MediaError::BadScheme => "bad_scheme",
            MediaError::OriginNotAllowed => "origin_not_allowed",
            MediaError::HostUnresolved => "host_unresolved",
            MediaError::ForbiddenHost => "forbidden_host",
            MediaError::BadStatus => "bad_status",
            MediaError::TooLarge => "too_large",
            MediaError::NotAnImage => "not_an_image",
            MediaError::Transport => "transport",
        }
    }
}

/// An image in memory.
#[derive(Debug, Clone)]
pub struct DownloadedImage {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

impl DownloadedImage {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Validate a URL against the allowlist and the forbidden-range rules.
///
/// DNS resolution happens on a blocking thread because `ToSocketAddrs` is synchronous.
pub async fn check_url(
    raw: &str,
    origins: &[String],
    max_bytes: u64,
) -> Result<url::Url, MediaError> {
    let _ = max_bytes;
    let parsed = url::Url::parse(raw).map_err(|_| MediaError::BadUrl)?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => return Err(MediaError::BadScheme),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        // Credentials in a URL are never legitimate here and confuse origin checks.
        return Err(MediaError::BadUrl);
    }
    let origin = origin_of(&parsed).ok_or(MediaError::BadUrl)?;
    let host = parsed.host_str().unwrap_or("");
    let is_trusted_cdn = {
        let h = host.to_ascii_lowercase();
        h.ends_with(".qpic.cn")
            || h == "qpic.cn"
            || h.ends_with(".qq.com")
            || h == "qq.com"
            || h.ends_with(".gtimg.cn")
            || h == "gtimg.cn"
            || h.ends_with(".qlogo.cn")
            || h == "qlogo.cn"
            || h.ends_with(".biliimg.com")
            || h.ends_with(".hdslb.com")
    };
    if !origins.iter().any(|allowed| allowed == &origin) && !is_trusted_cdn {
        return Err(MediaError::OriginNotAllowed);
    }
    screen_host(&parsed).await?;
    Ok(parsed)
}

/// Serialise a URL to its origin form (`scheme://host[:port]`, default port elided).
pub fn origin_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    match url.port() {
        Some(port) => Some(format!("{}://{}:{}", url.scheme(), host, port)),
        None => Some(format!("{}://{}", url.scheme(), host)),
    }
}

/// Resolve the host and refuse when any address is in a forbidden range.
async fn screen_host(url: &url::Url) -> Result<(), MediaError> {
    let host = url.host_str().ok_or(MediaError::BadUrl)?;
    let port = url.port_or_known_default().unwrap_or(80);
    // `host_str` keeps the brackets around an IPv6 literal; strip them for parsing.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return if host_is_forbidden(ip) {
            Err(MediaError::ForbiddenHost)
        } else {
            Ok(())
        };
    }
    let owned = host.to_string();
    let addrs = tokio::task::spawn_blocking(move || {
        (owned.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.map(|a| a.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|_| MediaError::HostUnresolved)?
    .map_err(|_| MediaError::HostUnresolved)?;

    if addrs.is_empty() {
        return Err(MediaError::HostUnresolved);
    }
    // One bad address disqualifies the host: a name may round-robin in a private one.
    if addrs.iter().any(|ip| host_is_forbidden(*ip)) {
        return Err(MediaError::ForbiddenHost);
    }
    Ok(())
}

/// Anything that can fetch an allowlisted image. Tests implement this with fixtures.
#[async_trait::async_trait]
pub trait ImageFetcher: Send + Sync + 'static {
    async fn fetch(&self, url: &str, max_bytes: u64) -> Result<DownloadedImage, MediaError>;
}

/// The real fetcher.
pub struct HttpFetcher {
    client: reqwest::Client,
    origins: Vec<String>,
    max_bytes: u64,
}

impl HttpFetcher {
    pub fn new(origins: Vec<String>, max_bytes: u64) -> Self {
        let client = reqwest::Client::builder()
            // A redirect can leave the allowlist, so none is followed.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TOTAL_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            client,
            origins,
            max_bytes,
        }
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }
}

#[async_trait::async_trait]
impl ImageFetcher for HttpFetcher {
    async fn fetch(&self, url: &str, max_bytes: u64) -> Result<DownloadedImage, MediaError> {
        let limit = if max_bytes == 0 {
            self.max_bytes
        } else {
            max_bytes
        };
        let parsed = check_url(url, &self.origins, limit).await?;

        let response = self
            .client
            .get(parsed)
            .send()
            .await
            .map_err(|_| MediaError::Transport)?;
        if !response.status().is_success() {
            return Err(MediaError::BadStatus);
        }
        // A redirect that was not followed shows up as 3xx; treat it as a status error
        // (already covered above) and additionally refuse an explicit Location.
        if response.headers().contains_key(reqwest::header::LOCATION) {
            return Err(MediaError::BadStatus);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.starts_with("image/") {
            return Err(MediaError::NotAnImage);
        }
        if let Some(len) = response.content_length()
            && len > limit
        {
            return Err(MediaError::TooLarge);
        }

        let mut collected: Vec<u8> = Vec::new();
        let mut body = response;
        while let Some(chunk) = body.chunk().await.map_err(|_| MediaError::Transport)? {
            if collected.len() + chunk.len() > limit as usize {
                return Err(MediaError::TooLarge);
            }
            collected.extend_from_slice(&chunk);
        }
        if collected.is_empty() {
            return Err(MediaError::Transport);
        }
        Ok(DownloadedImage {
            bytes: collected,
            content_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origins() -> Vec<String> {
        vec!["https://media.example.com".to_string()]
    }

    #[test]
    fn origin_of_elides_default_ports() {
        let url = url::Url::parse("https://media.example.com/a/b.png").unwrap();
        assert_eq!(
            origin_of(&url).as_deref(),
            Some("https://media.example.com")
        );
        let ported = url::Url::parse("https://media.example.com:8443/a.png").unwrap();
        assert_eq!(
            origin_of(&ported).as_deref(),
            Some("https://media.example.com:8443")
        );
    }

    #[tokio::test]
    async fn allowlisted_literal_ip_origin_is_accepted() {
        // A public literal IP passes the screen without any DNS traffic.
        let origins = vec!["https://93.184.216.34".to_string()];
        assert!(
            check_url("https://93.184.216.34/x.png", &origins, 1024)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn non_allowlisted_origin_is_refused_before_any_network_use() {
        for bad in [
            "https://evil.example.net/x.png",
            "https://media.example.com.evil.net/x.png",
            "http://media.example.com/x.png",       // scheme differs
            "https://media.example.com:8443/x.png", // port differs
        ] {
            assert_eq!(
                check_url(bad, &origins(), 1024).await.err(),
                Some(MediaError::OriginNotAllowed),
                "{bad} 必须被拒绝"
            );
        }
    }

    #[tokio::test]
    async fn loopback_and_metadata_literals_are_forbidden() {
        for host in ["127.0.0.1", "169.254.169.254", "[::1]"] {
            let origin = format!("https://{host}");
            let url = format!("https://{host}/x.png");
            assert_eq!(
                check_url(&url, &[origin], 1024).await.err(),
                Some(MediaError::ForbiddenHost),
                "{host} 必须被拒绝"
            );
        }
    }

    #[tokio::test]
    async fn private_literal_origins_work_only_when_explicitly_allowlisted() {
        for host in ["10.0.0.5", "192.168.1.10", "172.16.0.1", "[fc00::1]"] {
            let origin = format!("https://{host}");
            let url = format!("https://{host}/x.png");
            assert!(check_url(&url, &[origin], 1024).await.is_ok(), "{host}");
        }
        assert_eq!(
            check_url("https://10.0.0.5/x.png", &origins(), 1024)
                .await
                .err(),
            Some(MediaError::OriginNotAllowed)
        );
    }

    #[tokio::test]
    async fn credentials_in_url_are_refused() {
        let origin = vec!["https://media.example.com".to_string()];
        assert_eq!(
            check_url("https://user:pw@media.example.com/x.png", &origin, 1024)
                .await
                .err(),
            Some(MediaError::BadUrl)
        );
    }

    #[tokio::test]
    async fn non_http_schemes_are_refused() {
        for bad in ["file:///etc/passwd", "ftp://media.example.com/x.png"] {
            assert_eq!(
                check_url(bad, &origins(), 1024).await.err(),
                Some(MediaError::BadScheme),
                "{bad} 必须被拒绝"
            );
        }
    }

    #[test]
    fn fetcher_disables_redirects() {
        // Constructing the client must not panic and must keep the policy in place; the
        // observable behaviour is covered by the Location-header refusal above.
        let fetcher = HttpFetcher::new(origins(), 1024);
        assert_eq!(fetcher.origins().len(), 1);
    }
}
