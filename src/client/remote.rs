//! Fetches an image from a public `https` URL so it can be re-hosted on
//! `HackMD`, the usual step when a note still links images on a third-party
//! host such as imgur.
//!
//! The server, not the agent, makes this request, and whatever it fetches is
//! published under a public link. So the fetch is confined to public hosts:
//! `https` on its default port, no credentials in the URL, and every address
//! the host resolves to must be public. The client's only resolver answers
//! with the addresses just checked, and only for the host they were checked
//! for, so a second DNS answer cannot swap in a private one. Each redirect
//! target is checked the same way. The caller still checks the bytes are an
//! image before uploading them.
//!
//! An address check has two blind spots. It cannot see translation past this
//! machine: on an IPv6-only network whose NAT64 gateway uses its own prefix
//! inside `2000::/3`, a translated private IPv4 address looks public, and
//! such a network has to filter that at its egress. Nor can it see whom a
//! public host serves: one that answers only this machine's network, by
//! source address, passes, and its image is republished like any other.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    header,
};
use thiserror::Error;
use tokio::{sync::Semaphore, time::Instant};
use url::Url;

use super::{BodyError, HackmdClient, read_body_capped, readback::transfer_allowance};
use crate::{config::Config, reply::ErrorKind};

/// Redirects followed before giving up; CDNs use one or two.
const MAX_REDIRECTS: usize = 5;

/// System lookups running at once. The resolver blocks and cannot be
/// cancelled, so a lookup the deadline gave up on keeps its thread until the
/// resolver returns; without a bound, a run of slow names would fill the
/// blocking pool the file tools also run on.
static LOOKUPS: Semaphore = Semaphore::const_new(4);

/// What a fetch may reach. Only the test constructors of [`Config`] choose
/// [`Policy::Loopback`], which admits the plain-HTTP loopback fixture.
#[derive(Clone, Copy, Debug)]
enum Policy {
    Public,
    Loopback,
}

impl Policy {
    /// Whether only `https` on its default port is fetched, rather than
    /// `http` or `https` anywhere.
    const fn web_only(self) -> bool {
        matches!(self, Self::Public)
    }

    fn admits(self, ip: IpAddr) -> bool {
        match self {
            Self::Public => is_public(ip),
            Self::Loopback => ip.is_loopback(),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RemoteImageError {
    #[error("image_url is not a valid absolute URL")]
    InvalidUrl,
    #[error("image_url must use https on its default port and carry no user name or password")]
    NotHttps,
    #[error(
        "image_url redirected to {target}, which is refused: only https on its default port, without a user name or password, is fetched"
    )]
    RedirectRefused { target: String },
    #[error(
        "{host} resolves to a private, loopback, or reserved address; only public hosts are fetched"
    )]
    NotPublic { host: String },
    #[error("{host} does not resolve: there is no such host, or DNS is unavailable")]
    NoSuchHost { host: String },
    #[error("{host} did not resolve in time")]
    Unresolved { host: String },
    #[error("too many host lookups are still pending; retry shortly")]
    LookupsBusy,
    #[error("image_url redirected more than {MAX_REDIRECTS} times")]
    TooManyRedirects,
    #[error("the image host answered HTTP {status} without a usable Location")]
    BadRedirect { status: u16 },
    #[error("the image host answered HTTP {status}")]
    Status { status: u16 },
    #[error(
        "{host} presented a TLS certificate that was refused, so it cannot be fetched securely"
    )]
    Untrusted { host: String },
    #[error("fetching image_url timed out")]
    TimedOut,
    #[error("could not connect to the image host")]
    ConnectFailed,
    #[error("the connection to the image host failed")]
    ConnectionFailed,
    #[error("the HTTP client for image_url could not be built")]
    ClientBuild,
    /// Past the limit the caller gave [`RemoteImage::read`], which knows it.
    #[error("the image at image_url is larger than allowed")]
    TooLarge,
}

impl crate::reply::ToolError for RemoteImageError {
    fn kind(&self) -> ErrorKind {
        match self {
            // A failed lookup cannot tell a name that does not exist from a
            // resolver that is down, and only the second is worth a retry: an
            // offline agent told every link is wrong would drop the images.
            Self::NoSuchHost { .. }
            | Self::Unresolved { .. }
            | Self::LookupsBusy
            | Self::TimedOut
            | Self::ConnectFailed
            | Self::ConnectionFailed => ErrorKind::Network,

            // A 4xx means the URL is wrong or the image is gone: asking again
            // will not help, so it is the caller's to fix. Only throttling and
            // server failures are worth a later retry.
            Self::Status { status: 429 } => ErrorKind::RateLimited,
            Self::Status { status } if *status >= 500 => ErrorKind::Upstream,
            Self::BadRedirect { .. } => ErrorKind::Upstream,
            Self::InvalidUrl
            | Self::NotHttps
            | Self::RedirectRefused { .. }
            | Self::NotPublic { .. }
            | Self::TooManyRedirects
            | Self::Status { .. }
            | Self::Untrusted { .. } => ErrorKind::InvalidInput,
            Self::ClientBuild => ErrorKind::Internal,
            Self::TooLarge => ErrorKind::TooLarge,
        }
    }
}

/// Whether `ip` is reachable from the public internet: not loopback, private,
/// link-local, shared (CGNAT), documentation, benchmarking, multicast, or
/// otherwise reserved. `IpAddr::is_global` would say this, but is unstable.
/// IPv6 is admitted by allowlist, global unicast `2000::/3` only, because a
/// denylist misses ranges such as deprecated site-local `fec0::/10` that some
/// networks still route internally.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99) // deprecated 6to4 relay anycast
                || (a == 198 && (18..20).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let [s0, s1, ..] = v6.segments();
            (s0 & 0xe000) == 0x2000
                && !(s0 == 0x2001 && (s1 & 0xfe00) == 0) // 2001::/23, IETF protocol assignments
                && !(s0 == 0x2001 && s1 == 0x0db8) // documentation
                && s0 != 0x2002 // 6to4, which embeds an arbitrary IPv4 address
                && !(s0 == 0x3fff && (s1 & 0xf000) == 0) // 3fff::/20, documentation
        }
    }
}

/// What `policy` decides from the URL alone: scheme, port, and credentials.
/// An `http` or `https` URL always has a host, so the lookup needs no check
/// of its own.
fn check_url(url: &Url, policy: Policy) -> Result<(), RemoteImageError> {
    let scheme_allowed = if policy.web_only() {
        url.scheme() == "https" && url.port().is_none()
    } else {
        matches!(url.scheme(), "http" | "https")
    };
    if !scheme_allowed || !url.username().is_empty() || url.password().is_some() {
        return Err(RemoteImageError::NotHttps);
    }
    Ok(())
}

/// `url`'s scheme, host, and port, without its credentials or path, to name
/// a redirect target in a message. Not `Url::origin`, which turns a refused
/// `file:` or `data:` target into an unhelpful `null`.
fn origin_of(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// Resolves the host of a URL [`check_url`] passed and checks every address
/// against `policy`. Returns the host and the addresses to pin. Waiting for a
/// free lookup and the lookup itself share one deadline, and a timeout says
/// which of the two it was.
async fn resolve(
    url: &Url,
    policy: Policy,
    deadline: Duration,
) -> Result<(String, Vec<SocketAddr>), RemoteImageError> {
    let host = url.host_str().unwrap_or_default().to_owned();
    let until = Instant::now() + deadline;
    let permit = tokio::time::timeout_at(until, LOOKUPS.acquire())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(RemoteImageError::LookupsBusy)?;
    let target = url.clone();
    let lookup = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        target.socket_addrs(|| None)
    });
    let addrs = match tokio::time::timeout_at(until, lookup).await {
        Ok(Ok(Ok(addrs))) if !addrs.is_empty() => addrs,
        Ok(Ok(_)) => return Err(RemoteImageError::NoSuchHost { host }),
        Ok(Err(_)) | Err(_) => return Err(RemoteImageError::Unresolved { host }),
    };
    if !addrs.iter().all(|addr| policy.admits(addr.ip())) {
        return Err(RemoteImageError::NotPublic { host });
    }
    Ok((host, addrs))
}

/// The fetch client's resolver: it answers only for hosts [`resolve`]
/// checked, with the addresses it checked, and a host checked again replaces
/// its entry. Any other name fails, so no lookup reqwest makes on its own can
/// fall through to the system resolver. Fetches share it safely, since every
/// entry passed the same check.
///
/// A pin is needed only until its connection is made, so past
/// [`Pinned::MAX_HOSTS`] one other is dropped for each new host rather than
/// all kept for the life of the server. A concurrent fetch whose pin was
/// dropped before it connected fails to connect, never connects unchecked;
/// pooled connections are unaffected.
#[derive(Debug, Default)]
struct Pinned(Mutex<HashMap<String, Vec<SocketAddr>>>);

impl Pinned {
    const MAX_HOSTS: usize = 64;

    fn set(&self, host: String, addrs: Vec<SocketAddr>) {
        if let Ok(mut pins) = self.0.lock() {
            if pins.len() >= Self::MAX_HOSTS
                && !pins.contains_key(&host)
                && let Some(other) = pins.keys().next().cloned()
            {
                pins.remove(&other);
            }
            pins.insert(host, addrs);
        }
    }
}

impl Resolve for Pinned {
    fn resolve(&self, name: Name) -> Resolving {
        let addrs = self
            .0
            .lock()
            .ok()
            .and_then(|pins| pins.get(&name.as_str().to_ascii_lowercase()).cloned());
        Box::pin(async move {
            addrs
                .map(|addrs| Box::new(addrs.into_iter()) as Addrs)
                .ok_or_else(|| "host was not checked before connecting".into())
        })
    }
}

/// The image-fetch client and the pins it connects through. It is separate
/// from the API client, which carries the token, and is built once per
/// [`HackmdClient`], on first use: building one loads the system's root
/// certificates, and keeping it keeps connections to an image host across
/// uploads. Timeouts are per stage, in [`HackmdClient::open_image`] and
/// [`RemoteImage::read`]: reqwest's total timeout would cut a slow body off.
#[derive(Debug)]
pub(super) struct Fetcher {
    http: reqwest::Client,
    pinned: Arc<Pinned>,
}

impl Fetcher {
    fn new(config: &Config, policy: Policy) -> Result<Self, RemoteImageError> {
        let pinned = Arc::new(Pinned::default());
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .redirect(reqwest::redirect::Policy::none())
            // `check_url` already refuses anything else on every hop; this
            // holds even if a later change skips it.
            .https_only(policy.web_only())
            // A proxy would resolve the host itself and bypass the pin.
            .no_proxy()
            .user_agent(format!("hackmd-mcp/{}", crate::VERSION))
            .dns_resolver(Arc::clone(&pinned))
            .build()
            .map_err(|_| RemoteImageError::ClientBuild)?;
        Ok(Self { http, pinned })
    }
}

/// An `image_url` that passed every check needing no I/O, so a URL that
/// could never be fetched costs nothing else.
pub(crate) struct ImageUrl(Url);

impl ImageUrl {
    pub(crate) fn url(&self) -> &Url {
        &self.0
    }
}

/// A fetched image whose headers have arrived and whose body is unread.
pub(crate) struct RemoteImage {
    response: reqwest::Response,
    /// The length the headers declared, kept as it was: reading the body
    /// changes what the response reports.
    declared: Option<u64>,
    stall: Duration,
    /// The start of the body, read by [`RemoteImage::head`].
    head: Vec<u8>,
}

impl RemoteImage {
    pub(crate) fn declared_len(&self) -> Option<u64> {
        self.declared
    }

    /// The first `len` bytes of the body, or all of it if shorter, so its
    /// type can be checked before the rest is downloaded. [`RemoteImage::read`]
    /// still returns the whole body. A host that takes longer than one stall
    /// to send them is timed out.
    pub(crate) async fn head(&mut self, len: usize) -> Result<&[u8], RemoteImageError> {
        let until = Instant::now() + self.stall;
        while self.head.len() < len {
            let next = tokio::time::timeout_at(until, self.response.chunk())
                .await
                .map_err(|_| RemoteImageError::TimedOut)?;
            match next.map_err(|_| RemoteImageError::ConnectionFailed)? {
                Some(chunk) => self.head.extend_from_slice(&chunk),
                None => break,
            }
        }
        Ok(&self.head[..self.head.len().min(len)])
    }

    /// Reads the body, refusing more than `limit` bytes. A stall ends the
    /// read; a slow but steady transfer does not, down to the slowest rate the
    /// client accepts anywhere.
    pub(crate) async fn read(self, limit: u64) -> Result<Vec<u8>, RemoteImageError> {
        // Checked here against the whole declared length, since the bytes
        // `head` read are already part of it.
        if self.declared.is_some_and(|declared| declared > limit) {
            return Err(RemoteImageError::TooLarge);
        }
        let cap = usize::try_from(limit).unwrap_or(usize::MAX);
        let ceiling = self.stall + transfer_allowance(cap);
        let read = read_body_capped(self.response, self.head, cap, self.stall);
        match tokio::time::timeout(ceiling, read).await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(BodyError::TooLarge)) => Err(RemoteImageError::TooLarge),
            Ok(Err(BodyError::Transport(_))) => Err(RemoteImageError::ConnectionFailed),
            Ok(Err(BodyError::Stalled)) | Err(_) => Err(RemoteImageError::TimedOut),
        }
    }
}

impl HackmdClient {
    const fn image_policy(&self) -> Policy {
        if self.config.loopback_images() {
            Policy::Loopback
        } else {
            Policy::Public
        }
    }

    fn fetcher(&self) -> Result<&Fetcher, RemoteImageError> {
        if let Some(fetcher) = self.images.get() {
            return Ok(fetcher);
        }
        let fetcher = Fetcher::new(&self.config, self.image_policy())?;
        Ok(self.images.get_or_init(|| fetcher))
    }

    /// Checks `link` without any I/O.
    pub(crate) fn check_image_url(&self, link: &str) -> Result<ImageUrl, RemoteImageError> {
        let url = Url::parse(link).map_err(|_| RemoteImageError::InvalidUrl)?;
        check_url(&url, self.image_policy())?;
        Ok(ImageUrl(url))
    }

    /// Requests `link` and returns once an image's headers arrive, following
    /// redirects only to hosts that pass the same checks.
    pub(crate) async fn open_image(
        &self,
        link: &ImageUrl,
    ) -> Result<RemoteImage, RemoteImageError> {
        let policy = self.image_policy();
        let fetcher = self.fetcher()?;
        let stall = self.config.request_timeout();

        // One deadline from the first lookup to the image's headers, across
        // every hop, so a chain of slow redirects costs no more than one slow
        // response.
        let until = Instant::now() + stall;
        let mut url = link.0.clone();
        for hop in 0..=MAX_REDIRECTS {
            if hop > 0 {
                check_url(&url, policy).map_err(|_| RemoteImageError::RedirectRefused {
                    target: origin_of(&url),
                })?;
            }

            // A lookup cut short by the chain's deadline is a timeout, not a
            // host that does not resolve.
            let (host, addrs) = tokio::time::timeout_at(
                until,
                resolve(&url, policy, self.config.connect_timeout()),
            )
            .await
            .map_err(|_| RemoteImageError::TimedOut)??;
            fetcher.pinned.set(host.clone(), addrs);
            let response = tokio::time::timeout_at(until, fetcher.http.get(url.clone()).send())
                .await
                .map_err(|_| RemoteImageError::TimedOut)?
                .map_err(|error| transport(&error, &host))?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let bad_redirect = || RemoteImageError::BadRedirect {
                    status: status.as_u16(),
                };
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(bad_redirect)?;
                url = url.join(location).map_err(|_| bad_redirect())?;
                continue;
            }

            // Any 2xx carrying a whole body, so a 203 from a transforming proxy
            // passes; a 204 has no body and a 206 only part of one.
            if !status.is_success() || matches!(status.as_u16(), 204 | 206) {
                return Err(RemoteImageError::Status {
                    status: status.as_u16(),
                });
            }
            return Ok(RemoteImage {
                declared: response.content_length(),
                response,
                stall,
                head: Vec::new(),
            });
        }
        Err(RemoteImageError::TooManyRedirects)
    }
}

fn transport(error: &reqwest::Error, host: &str) -> RemoteImageError {
    if error.is_timeout() {
        RemoteImageError::TimedOut
    } else if names_certificate(error) {
        RemoteImageError::Untrusted {
            host: host.to_owned(),
        }
    } else if error.is_connect() {
        RemoteImageError::ConnectFailed
    } else {
        RemoteImageError::ConnectionFailed
    }
}

/// Whether a certificate failure is anywhere beneath `error`. reqwest
/// reports one as a connect error and does not expose the TLS library's
/// types, so the text is the only signal: rustls says "invalid peer
/// certificate". Retrying would never fix it, unlike a refused connection.
/// reqwest's own text names the URL, which may hold the same words, so the
/// search starts below it.
fn names_certificate(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(error.source(), |error| error.source())
        .any(|error| error.to_string().contains("invalid peer certificate"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::dns::Resolve;
    use url::Url;

    use super::{
        Fetcher, MAX_REDIRECTS, Pinned, Policy, RemoteImage, RemoteImageError, check_url,
        is_public, names_certificate, resolve,
    };
    use crate::{
        client::HackmdClient,
        config::Config,
        fixture::{FIXTURE_TOKEN, Scenario, SequenceServer, spawn_raw_body},
        reply::{ErrorKind, ToolError},
    };

    fn image(path: &str) -> Scenario {
        Scenario::new("GET", path, 200, "fixture-image")
    }

    fn redirect(path: &str, to: &str) -> Scenario {
        Scenario::new("GET", path, 302, "").response_header("location", to)
    }

    /// Opens `link` the way an upload does: checked, then fetched.
    async fn open(client: &HackmdClient, link: &str) -> Result<RemoteImage, RemoteImageError> {
        client.open_image(&client.check_image_url(link)?).await
    }

    async fn open_err(fixture: &SequenceServer, path: &str) -> RemoteImageError {
        open(&fixture.client(), &format!("{}{path}", fixture.origin()))
            .await
            .err()
            .expect("the fetch should fail")
    }

    #[test]
    fn only_public_addresses_are_public() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.0.8",
            "192.88.99.2",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "2001:db8::1",
            "64:ff9b::a00:1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "fec0::1",
            "100::1",
            "2001::1",
            "2001:2::1",
            "2001:1ff:ffff:ffff:ffff:ffff:ffff:ffff",
            "2002:a00:1::1",
            "3fff::1",
            "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            assert!(
                !is_public(private.parse().expect("address parses")),
                "{private}"
            );
        }
        for public in [
            "1.1.1.1",
            "151.101.1.140",
            "192.88.98.1",
            "2001:200::1",
            "2001:4860::8888",
            "2003::1",
            "2606:4700::1111",
            "3ffe:ffff::1",
            "3fff:1000::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                is_public(public.parse().expect("address parses")),
                "{public}"
            );
        }
    }

    /// The scheme, port, and credentials are refused from the URL alone;
    /// a private host only once it resolves.
    #[tokio::test]
    async fn refuses_what_is_not_a_public_https_url() {
        for link in [
            "http://i.imgur.com/x.png",
            "file:///etc/passwd",
            "https://user:pw@example.com/x.png",
            "https://i.imgur.com:8443/x.png",
        ] {
            let url = Url::parse(link).expect("url parses");
            assert!(
                matches!(
                    check_url(&url, Policy::Public),
                    Err(RemoteImageError::NotHttps)
                ),
                "{link}"
            );
        }
        for link in [
            "https://127.0.0.1/x.png",
            "https://[::1]/x.png",
            "https://[fec0::1]/x.png",
            "https://localhost/x.png",
        ] {
            let url = Url::parse(link).expect("url parses");
            assert!(check_url(&url, Policy::Public).is_ok(), "{link}");
            let error = resolve(&url, Policy::Public, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(
                matches!(error, RemoteImageError::NotPublic { .. }),
                "{link}: {error:?}"
            );
        }
        // An explicit default port is no port at all.
        let url = Url::parse("https://i.imgur.com:443/x.png").expect("url parses");
        assert!(check_url(&url, Policy::Public).is_ok());
    }

    #[test]
    fn only_a_fault_that_may_pass_is_worth_a_retry() {
        let kind = |status| RemoteImageError::Status { status }.kind();
        assert_eq!(kind(404), ErrorKind::InvalidInput);
        assert_eq!(kind(403), ErrorKind::InvalidInput);
        assert_eq!(kind(429), ErrorKind::RateLimited);
        assert_eq!(kind(503), ErrorKind::Upstream);
        let host = || "example.com".to_owned();
        for (error, expected) in [
            (RemoteImageError::TooManyRedirects, ErrorKind::InvalidInput),
            (
                RemoteImageError::BadRedirect { status: 302 },
                ErrorKind::Upstream,
            ),
            (
                RemoteImageError::NoSuchHost { host: host() },
                ErrorKind::Network,
            ),
            (RemoteImageError::LookupsBusy, ErrorKind::Network),
            (
                RemoteImageError::Untrusted { host: host() },
                ErrorKind::InvalidInput,
            ),
        ] {
            assert_eq!(error.kind(), expected, "{error:?}");
        }
    }

    /// A name that does not exist reads as a lookup failure. Ignored by
    /// default: it asks the system resolver, which some networks answer for
    /// every name.
    #[tokio::test]
    #[ignore = "depends on the system resolver"]
    async fn a_host_that_does_not_exist_does_not_resolve() {
        let url = Url::parse("https://no-such-host.invalid/x.png").expect("url parses");
        let error = resolve(&url, Policy::Public, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(error, RemoteImageError::NoSuchHost { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_certificate_failure_is_found_anywhere_in_the_chain() {
        /// Worded as reqwest words it, URL included.
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "error sending request for url (https://cdn.example/invalid peer certificate.png)",
                )
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let expired = Outer(std::io::Error::other("invalid peer certificate: Expired"));
        assert!(names_certificate(&expired));
        let refused = Outer(std::io::Error::other("connection refused"));
        assert!(!names_certificate(&refused));
    }

    /// The resolver answers only for hosts checked, so a name reqwest looks
    /// up on its own never reaches the system resolver.
    #[tokio::test]
    async fn the_pinned_resolver_answers_only_for_the_checked_host() {
        let pinned = Pinned::default();
        let name = |host: &str| host.parse().expect("name parses");
        assert!(pinned.resolve(name("example.com")).await.is_err());

        let addr = "93.184.215.14:443".parse().expect("address parses");
        pinned.set("example.com".to_owned(), vec![addr]);
        let addrs: Vec<_> = pinned
            .resolve(name("EXAMPLE.com"))
            .await
            .expect("the checked host resolves")
            .collect();
        assert_eq!(addrs, [addr]);
        assert!(pinned.resolve(name("other.example")).await.is_err());

        // Past the bound, one host goes for each new one and the newest stays.
        for index in 0..Pinned::MAX_HOSTS {
            pinned.set(format!("host{index}.example"), vec![addr]);
        }
        let pins = pinned.0.lock().expect("pins lock");
        assert_eq!(pins.len(), Pinned::MAX_HOSTS);
        assert!(pins.contains_key(&format!("host{}.example", Pinned::MAX_HOSTS - 1)));
    }

    /// The fetch client connects where the pin says: `image.invalid` exists
    /// nowhere else, so only the pin can make this request land.
    #[tokio::test]
    async fn the_fetch_client_connects_only_through_the_pin() {
        let fixture = SequenceServer::spawn_scenarios([image("/x.png")]);
        let config = Config::for_loopback_test(&fixture.api_url, None);
        let fetcher = Fetcher::new(&config, Policy::Loopback).expect("client builds");
        let link = format!("http://image.invalid:{}/x.png", fixture.addr().port());
        assert!(
            fetcher.http.get(&link).send().await.is_err(),
            "nothing is pinned"
        );
        fetcher
            .pinned
            .set("image.invalid".to_owned(), vec![fixture.addr()]);
        let response = fetcher
            .http
            .get(&link)
            .send()
            .await
            .expect("the pin connects");
        assert_eq!(response.status(), 200);
        fixture.finish();
    }

    /// The fetch follows redirects, sends no token, and reads the image.
    #[tokio::test]
    async fn follows_redirects_without_the_token() {
        let fixture = SequenceServer::spawn_scenarios([
            redirect("/a.png", "/b.png"),
            redirect("/b.png", "{origin}/c.png"),
            image("/c.png"),
        ]);
        let bytes = open(&fixture.client(), &format!("{}/a.png", fixture.origin()))
            .await
            .expect("the image opens")
            .read(1024)
            .await
            .expect("the image reads");
        assert_eq!(bytes, b"fixture-image");
        for request in fixture.finish() {
            let request = request.to_ascii_lowercase();
            assert!(!request.contains("authorization:"), "{request}");
            assert!(!request.contains(FIXTURE_TOKEN), "{request}");
            assert!(request.contains("user-agent: hackmd-mcp/"), "{request}");
        }
    }

    /// Redirects share one deadline: two hops that each answer in time
    /// still time out together.
    #[tokio::test]
    async fn redirects_share_one_deadline() {
        let fixture = SequenceServer::spawn_scenarios([
            redirect("/a.png", "/b.png").delay(Duration::from_millis(1000)),
            image("/b.png").delay(Duration::from_millis(1000)),
        ]);
        let client = fixture.client_with_timeout(Duration::from_millis(1500));

        // Each hop alone answers within the bound, so only a shared deadline
        // times this chain out. The first hop leaves half a second to spare, so
        // a slow runner still reaches the second, which `finish` checks.
        let error = open(&client, &format!("{}/a.png", fixture.origin()))
            .await
            .err()
            .expect("the chain should time out");
        assert!(matches!(error, RemoteImageError::TimedOut), "{error:?}");
        fixture.finish();
    }

    /// The bytes `head` read count toward the cap: a declared length that,
    /// with them, is over it is refused before the rest is waited for.
    #[tokio::test]
    async fn a_declared_length_counts_the_bytes_already_read() {
        let server = spawn_raw_body(
            Some(16),
            vec![
                (Duration::ZERO, b"GIF89a1234".to_vec()),
                (Duration::from_secs(3), b"567890".to_vec()),
            ],
        );
        let mut opened = open(
            &server.client(Duration::from_secs(5)),
            &format!("{}/x", server.origin),
        )
        .await
        .expect("the image opens");
        opened.head(4).await.expect("the signature arrives");
        let read = tokio::time::timeout(Duration::from_secs(1), opened.read(12)).await;
        assert!(
            matches!(read, Ok(Err(RemoteImageError::TooLarge))),
            "refused without waiting for the rest"
        );
    }

    /// The signature shares one deadline: a host that sends it a byte at a
    /// time, each in time, still times out.
    #[tokio::test]
    async fn a_signature_sent_slowly_times_out() {
        let chunks = b"GIF89a"
            .iter()
            .map(|byte| (Duration::from_millis(150), vec![*byte]))
            .collect();
        let server = spawn_raw_body(None, chunks);
        let mut opened = open(
            &server.client(Duration::from_millis(400)),
            &format!("{}/x", server.origin),
        )
        .await
        .expect("the image opens");
        let error = opened.head(12).await.expect_err("the signature times out");
        assert!(matches!(error, RemoteImageError::TimedOut), "{error:?}");
    }

    #[tokio::test]
    async fn stops_after_the_redirect_limit() {
        let fixture = SequenceServer::spawn_scenarios([
            redirect("/0", "/1"),
            redirect("/1", "/2"),
            redirect("/2", "/3"),
            redirect("/3", "/4"),
            redirect("/4", "/5"),
            redirect("/5", "/6"),
        ]);
        let error = open_err(&fixture, "/0").await;
        assert!(
            matches!(error, RemoteImageError::TooManyRedirects),
            "{error:?}"
        );
        assert_eq!(fixture.finish().len(), MAX_REDIRECTS + 1);
    }

    /// Each redirect target is checked again before it is requested, and a
    /// refusal names the target, not the caller's URL, without its
    /// credentials.
    #[tokio::test]
    async fn a_redirect_to_a_refused_target_is_not_followed() {
        let fixture =
            SequenceServer::spawn_scenarios([redirect("/x.png", "http://10.0.0.1/x.png")]);
        let error = open_err(&fixture, "/x.png").await;
        assert!(
            matches!(error, RemoteImageError::NotPublic { .. }),
            "{error:?}"
        );
        fixture.finish();

        let fixture = SequenceServer::spawn_scenarios([redirect(
            "/x.png",
            "ftp://user:secret@files.example:21/x.png",
        )]);
        let error = open_err(&fixture, "/x.png").await;
        assert!(
            matches!(error, RemoteImageError::RedirectRefused { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("ftp://files.example"), "{message}");
        assert!(!message.contains("secret"), "{message}");
        fixture.finish();
    }

    #[tokio::test]
    async fn a_redirect_without_a_location_is_the_hosts_fault() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new("GET", "/x.png", 302, "")]);
        let error = open_err(&fixture, "/x.png").await;
        assert!(
            matches!(error, RemoteImageError::BadRedirect { status: 302 }),
            "{error:?}"
        );
        fixture.finish();
    }

    /// Any 2xx with a whole body is an image to read; a missing image, an
    /// empty reply, and a partial one are not.
    #[tokio::test]
    async fn only_a_whole_body_is_read() {
        let fixture =
            SequenceServer::spawn_scenarios([Scenario::new("GET", "/x.png", 203, "fixture-image")]);
        open(&fixture.client(), &format!("{}/x.png", fixture.origin()))
            .await
            .expect("a 203 carries the whole body");
        fixture.finish();

        for status in [204, 206, 404] {
            let fixture =
                SequenceServer::spawn_scenarios([Scenario::new("GET", "/x.png", status, "")]);
            let error = open_err(&fixture, "/x.png").await;
            assert!(
                matches!(error, RemoteImageError::Status { status: got } if got == status),
                "{status}: {error:?}"
            );
            fixture.finish();
        }
    }

    /// The declared length is visible before the body is read, and the read
    /// refuses a body over its limit.
    #[tokio::test]
    async fn the_length_is_known_before_the_read_and_capped_by_it() {
        let fixture = SequenceServer::spawn_scenarios([image("/x.png")]);
        let opened = open(&fixture.client(), &format!("{}/x.png", fixture.origin()))
            .await
            .expect("the image opens");
        assert_eq!(opened.declared_len(), Some(13));
        let error = opened.read(4).await.unwrap_err();
        assert!(matches!(error, RemoteImageError::TooLarge), "{error:?}");
        fixture.finish();
    }

    /// With no declared length, the cap is enforced while streaming.
    #[tokio::test]
    async fn an_undeclared_body_is_capped_while_it_streams() {
        let server = spawn_raw_body(
            None,
            vec![
                (Duration::ZERO, b"0123".to_vec()),
                (Duration::from_millis(20), b"4567".to_vec()),
            ],
        );
        let opened = open(
            &server.client(Duration::from_secs(5)),
            &format!("{}/x", server.origin),
        )
        .await
        .expect("the image opens");
        assert_eq!(opened.declared_len(), None);
        let error = opened.read(6).await.unwrap_err();
        assert!(matches!(error, RemoteImageError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn a_host_that_never_answers_times_out() {
        let fixture =
            SequenceServer::spawn_scenarios([image("/x.png").delay(Duration::from_millis(150))]);
        let client = fixture.client_with_timeout(Duration::from_millis(50));
        let error = open(&client, &format!("{}/x.png", fixture.origin()))
            .await
            .err()
            .expect("the request times out");
        assert!(matches!(error, RemoteImageError::TimedOut), "{error:?}");
        fixture.finish();
    }

    /// A body that stops arriving ends the read at the stall bound.
    #[tokio::test]
    async fn a_body_that_stalls_times_out() {
        let server = spawn_raw_body(
            None,
            vec![
                (Duration::ZERO, b"01".to_vec()),
                (Duration::from_secs(3), b"23".to_vec()),
            ],
        );
        let opened = open(
            &server.client(Duration::from_millis(200)),
            &format!("{}/x", server.origin),
        )
        .await
        .expect("the image opens");
        let error = opened.read(1024).await.unwrap_err();
        assert!(matches!(error, RemoteImageError::TimedOut), "{error:?}");
    }

    /// A body that never stalls but arrives slower than the slowest rate
    /// accepted still ends: every gap here is a quarter of the stall bound,
    /// yet the whole takes five times it.
    #[tokio::test]
    async fn a_body_that_trickles_hits_the_ceiling() {
        let chunks = (0..20)
            .map(|_| (Duration::from_millis(50), b"x".to_vec()))
            .collect();
        let server = spawn_raw_body(None, chunks);
        let opened = open(
            &server.client(Duration::from_millis(200)),
            &format!("{}/x", server.origin),
        )
        .await
        .expect("the image opens");
        let error = opened.read(1024).await.unwrap_err();
        assert!(matches!(error, RemoteImageError::TimedOut), "{error:?}");
    }
}
