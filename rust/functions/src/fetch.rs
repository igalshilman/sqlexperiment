//! `fetch`: the HTTP requests programs make, `fetch(url, init)` in
//! JavaScript, as a host binding. The host sends them, under a
//! [`FetchPolicy`] the deployment sets: a program reaches only the origins it
//! allows, and never holds the secrets it adds.
//!
//! A request is the guest's own `{url, method, headers, body}`, read from its
//! heap when it is sent (and again after a crash: a request is sent at least
//! once). The response, `{status, statusText, url, redirected, headers,
//! body}`, is delivered to the guest and saved with its heap. A body that is
//! not UTF-8 crosses as `bodyBase64` instead.
//!
//! What a program cannot do through it:
//!
//! - Reach an origin the policy does not allow, by its URL or by a redirect:
//!   each hop is checked.
//! - Reach a private network (loopback, link-local such as cloud metadata,
//!   private ranges), even through an allowed name: the addresses a name
//!   resolves to are checked, unless the policy allows private networks.
//!   Environment proxies are not used, so that the check sees the real
//!   destination.
//! - Read a secret: the host adds it to requests to its origin only, also
//!   after a redirect, and a header the program sets under its name is
//!   replaced.
//! - Share state with another run: the host keeps no cookies, no cache and no
//!   credentials between requests. A program that wants cookies keeps them
//!   itself, in its own heap.
//! - Leak a URL through a redirect: no `Referer` is sent, and the program's
//!   `authorization` and `cookie` headers are dropped when a redirect leaves
//!   the origin.
//!
//! A response larger than the policy's limit fails the request, and so does
//! a request that takes longer than its timeout.
use crate::{Binding, GuestError, HostCall};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use futures::future::BoxFuture;
use reqwest::{
    Method, StatusCode, Url,
    dns::{Name, Resolve, Resolving},
    header::{self, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

/// The Claude API, which [`FetchPolicy::from_env`] allows.
const CLAUDE: &str = "https://api.anthropic.com";

/// How many redirects a request follows.
const MAX_REDIRECTS: usize = 10;

/// Headers the host sets, or that only a connection may: a program cannot.
const FORBIDDEN_HEADERS: [&str; 10] = [
    "connection",
    "content-length",
    "expect",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Where programs may send requests, and what the host adds to them.
/// [`FetchPolicy::new`] allows nothing.
pub struct FetchPolicy {
    origins: BTreeSet<String>,
    public: bool,
    private_networks: bool,
    secrets: BTreeMap<String, HeaderMap>,
    max_response: usize,
    timeout: Duration,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl FetchPolicy {
    /// A policy that allows no request: responses up to 10 MiB, requests of
    /// up to 5 minutes.
    pub fn new() -> Self {
        Self {
            origins: BTreeSet::new(),
            public: false,
            private_networks: false,
            secrets: BTreeMap::new(),
            max_response: 10 << 20,
            timeout: Duration::from_secs(300),
        }
    }

    /// The policy a deployment sets by its environment: the Claude API, with
    /// ANTHROPIC_API_KEY as its key (if set), and the origins listed in
    /// FUNCTIONS_FETCH_ALLOW, separated by commas.
    pub fn from_env() -> Self {
        let mut policy = Self::new().allow(CLAUDE);
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            policy = policy.secret(CLAUDE, "x-api-key", &key);
        }
        let allowed = std::env::var("FUNCTIONS_FETCH_ALLOW").unwrap_or_default();
        for origin in allowed.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            policy = policy.allow(origin);
        }
        policy
    }

    /// Allow requests to `origin`, such as `https://api.example.com`.
    pub fn allow(mut self, origin: &str) -> Self {
        self.origins.insert(origin_of(origin));
        self
    }

    /// Allow requests to any origin on the public internet.
    pub fn allow_public(mut self) -> Self {
        self.public = true;
        self
    }

    /// Allow allowed origins to be on private networks (loopback, link-local,
    /// private ranges): for local development and tests.
    pub fn allow_private_networks(mut self) -> Self {
        self.private_networks = true;
        self
    }

    /// Add `header: value` to every request to `origin`, which this allows. A
    /// program never sees it, and cannot replace it.
    pub fn secret(mut self, origin: &str, header: &str, value: &str) -> Self {
        let origin = origin_of(origin);
        let name = HeaderName::try_from(header).expect("A secret's header name");
        let mut value = HeaderValue::try_from(value).expect("A secret's header value");
        value.set_sensitive(true);
        self.secrets
            .entry(origin.clone())
            .or_default()
            .insert(name, value);
        self.origins.insert(origin);
        self
    }

    /// Fail a request whose response body is larger than `bytes`.
    pub fn max_response(mut self, bytes: usize) -> Self {
        self.max_response = bytes;
        self
    }

    /// Fail a request that takes longer than `timeout`, redirects included.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn allows(&self, url: &Url) -> bool {
        let origin = url.origin().ascii_serialization();
        if self.origins.contains(&origin) || self.public {
            // An address in the URL is checked here; a name, as it resolves.
            return match url.host() {
                Some(url::Host::Ipv4(ip)) => self.private_networks || public(IpAddr::V4(ip)),
                Some(url::Host::Ipv6(ip)) => self.private_networks || public(IpAddr::V6(ip)),
                _ => true,
            };
        }
        false
    }
}

/// `origin`'s ASCII serialization, as URLs are compared with it.
fn origin_of(origin: &str) -> String {
    let url = Url::parse(origin).unwrap_or_else(|error| panic!("Origin {origin:?}: {error}"));
    assert!(
        matches!(url.scheme(), "http" | "https"),
        "Origin {origin:?} is not HTTP"
    );
    url.origin().ascii_serialization()
}

/// The binding: [`FetchPolicy`]'s requests, sent.
#[derive(Clone)]
pub struct Fetch {
    http: reqwest::Client,
    policy: Arc<FetchPolicy>,
}

impl Fetch {
    pub fn new(policy: FetchPolicy) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .dns_resolver(Arc::new(Resolver {
                private_networks: policy.private_networks,
            }))
            .build()
            .expect("An HTTP client");
        Self {
            http,
            policy: Arc::new(policy),
        }
    }

    async fn fetch(self, call: HostCall) -> Result<Value, GuestError> {
        let request: Request = serde_json::from_value(call.input)
            .map_err(|error| type_error(format!("Invalid fetch request: {error}")))?;
        let mut url = Url::parse(&request.url)
            .map_err(|error| type_error(format!("Invalid URL {:?}: {error}", request.url)))?;
        let mut method = Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
            .map_err(|_| type_error(format!("Invalid method {:?}", request.method)))?;
        if matches!(method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
            return Err(type_error(format!("Method {method} is not allowed")));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &request.headers {
            let name = HeaderName::try_from(name.as_str())
                .map_err(|_| type_error(format!("Invalid header name {name:?}")))?;
            if FORBIDDEN_HEADERS.contains(&name.as_str()) || name.as_str().starts_with("proxy-") {
                return Err(type_error(format!("Header {name} is set by the host")));
            }
            let value = HeaderValue::try_from(value.as_str())
                .map_err(|_| type_error(format!("Invalid value for header {name}")))?;
            headers.append(name, value);
        }
        let mut body = match (request.body, request.body_base64, request.json) {
            (Some(text), None, None) => Some(bytes::Bytes::from(text)),
            (None, Some(encoded), None) => Some(
                BASE64
                    .decode(encoded)
                    .map_err(|_| type_error("Invalid request body"))?
                    .into(),
            ),
            (None, None, Some(json)) => Some(serde_json::to_vec(&json).unwrap().into()),
            (None, None, None) => None,
            _ => return Err(type_error("A request has one body")),
        };

        let mut redirects = 0;
        loop {
            if url.scheme() != "http" && url.scheme() != "https" {
                return Err(type_error(format!("Cannot fetch a {} URL", url.scheme())));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(type_error("A URL with credentials cannot be fetched"));
            }
            if !self.policy.allows(&url) {
                return Err(type_error(format!(
                    "Requests to {} are not allowed",
                    url.origin().ascii_serialization()
                )));
            }
            let mut sent = headers.clone();
            if let Some(secrets) = self.policy.secrets.get(&url.origin().ascii_serialization()) {
                for (name, value) in secrets {
                    sent.insert(name, value.clone());
                }
            }
            let mut builder = self.http.request(method.clone(), url.clone()).headers(sent);
            if let Some(body) = &body {
                builder = builder.body(body.clone());
            }
            let mut response = builder.send().await.map_err(network_error)?;
            let status = response.status();
            let location = response.headers().get(header::LOCATION).cloned();
            if let Some(location) = location.filter(|_| status.is_redirection())
                && request.redirect != "manual"
            {
                if request.redirect == "error" {
                    return Err(type_error(format!("{url} redirects")));
                }
                redirects += 1;
                if redirects > MAX_REDIRECTS {
                    return Err(type_error("Too many redirects"));
                }
                let next = (location.to_str().ok())
                    .and_then(|location| url.join(location).ok())
                    .ok_or_else(|| type_error("Invalid redirect location"))?;
                // As browsers do: a 303, or a POST's 301 or 302, becomes a GET.
                if (status == StatusCode::SEE_OTHER && method != Method::HEAD)
                    || (matches!(status.as_u16(), 301 | 302) && method == Method::POST)
                {
                    method = Method::GET;
                    body = None;
                    headers.remove(header::CONTENT_TYPE);
                }
                if next.origin() != url.origin() {
                    headers.remove(header::AUTHORIZATION);
                    headers.remove(header::COOKIE);
                }
                url = next;
                continue;
            }
            let mut fields = Vec::new();
            for (name, value) in response.headers() {
                fields.push(json!([
                    name.as_str(),
                    String::from_utf8_lossy(value.as_bytes())
                ]));
            }
            let mut content = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_error)? {
                if content.len() + chunk.len() > self.policy.max_response {
                    return Err(GuestError::new(
                        "RangeError",
                        format!(
                            "The response is larger than {} bytes",
                            self.policy.max_response
                        ),
                    ));
                }
                content.extend_from_slice(&chunk);
            }
            let mut answer = json!({
                "status": status.as_u16(),
                "statusText": status.canonical_reason().unwrap_or(""),
                "url": url.as_str(),
                "redirected": redirects > 0,
                "headers": fields,
            });
            match String::from_utf8(content) {
                Ok(text) => answer["body"] = text.into(),
                Err(error) => answer["bodyBase64"] = BASE64.encode(error.into_bytes()).into(),
            }
            return Ok(answer);
        }
    }
}

impl Binding for Fetch {
    /// One HTTP request, with its redirects: its response.
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>> {
        let timeout = self.policy.timeout;
        let fetch = self.clone().fetch(call);
        Box::pin(async move {
            (tokio::time::timeout(timeout, fetch).await)
                .unwrap_or_else(|_| Err(type_error("fetch failed: the request timed out")))
        })
    }
}

/// A request, as the guest sends it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Request {
    url: String,
    method: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
    body: Option<String>,
    body_base64: Option<String>,
    json: Option<Value>,
    #[serde(default = "follow")]
    redirect: String,
}

fn follow() -> String {
    "follow".into()
}

fn type_error(message: impl Into<String>) -> GuestError {
    GuestError::new("TypeError", message)
}

/// A network failure, as `fetch` reports it: a `TypeError`. A timeout says so.
fn network_error(error: reqwest::Error) -> GuestError {
    if error.is_timeout() {
        return type_error("fetch failed: the request timed out");
    }
    let mut message = format!("fetch failed: {error}");
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    type_error(message)
}

/// Resolves names, and keeps the public addresses only, unless private
/// networks are allowed: a name cannot lead a program to the host's network.
struct Resolver {
    private_networks: bool,
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let private_networks = self.private_networks;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|address| private_networks || public(address.ip()))
                .collect();
            if addresses.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addresses.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send>)
        })
    }
}

/// Whether `ip` is on the public internet.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let segments = ip.segments();
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed an IPv4 address.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return public_v4(Ipv4Addr::from(
                    ((segments[6] as u32) << 16) | segments[7] as u32,
                ));
            }
            if segments[0] == 0x2002 {
                return public_v4(Ipv4Addr::from(
                    ((segments[1] as u32) << 16) | segments[2] as u32,
                ));
            }
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || segments[0] & 0xfe00 == 0xfc00 // unique local
                || segments[0] & 0xffc0 == 0xfe80 // link-local
                || segments[0] & 0xffc0 == 0xfec0 // site-local
                || segments[0] == 0x2001 && segments[1] == 0x0db8 // documentation
                || segments[..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || a == 0
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a >= 240 // reserved
        || a == 100 && (64..128).contains(&b) // shared (carrier-grade NAT)
        || a == 192 && b == 0 && ip.octets()[2] == 0 // IETF protocol assignments
        || a == 198 && (18..20).contains(&b)) // benchmarking
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "1.1.1.1",
            "160.79.104.10",
            "2607:6bc0::10",
            "::ffff:1.1.1.1",
        ] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn policies_allow_their_origins() {
        let policy = FetchPolicy::new().allow("https://api.example.com/");
        let allows = |url: &str| policy.allows(&Url::parse(url).unwrap());
        assert!(allows("https://api.example.com/v1/x?q=1"));
        assert!(allows("https://API.example.com:443/"));
        assert!(!allows("http://api.example.com/"));
        assert!(!allows("https://api.example.com:8443/"));
        assert!(!allows("https://example.com/"));
        assert!(!allows("https://api.example.com.evil.com/"));

        let public = FetchPolicy::new().allow_public();
        assert!(public.allows(&Url::parse("https://anything.com").unwrap()));
        assert!(!public.allows(&Url::parse("http://169.254.169.254/").unwrap()));
        assert!(!public.allows(&Url::parse("http://[::1]:9070/").unwrap()));
    }
}
