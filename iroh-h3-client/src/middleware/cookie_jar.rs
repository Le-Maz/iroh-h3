//! Cookie middleware for the HTTP/3 client.
//!
//! This module provides [`CookieJar`], a simple, thread-safe, per-peer cookie
//! store used by the `IrohH3Client` middleware system.  
//!
//! # Overview
//!
//! The [`CookieJar`] middleware:
//!
//! - Automatically attaches stored cookies to outgoing requests  
//!   (`Cookie:` header).
//! - Extracts cookies from incoming responses  
//!   (`Set-Cookie:` headers).
//! - Stores cookies _per peer_ using the peer’s [`EndpointId`].
//! - Uses the [`cookie`] crate to perform correct RFC-compliant parsing.
//!
//! [`cookie`]: https://docs.rs/cookie
//! [`EndpointId`]: iroh::EndpointId

use cookie::Cookie;
use dashmap::DashMap;
use http::{
    HeaderValue,
    header::{COOKIE, SET_COOKIE},
};
use iroh::EndpointId;
use tracing::{debug, instrument, trace, warn};

use crate::{
    body::Body,
    connection_manager::peer_id,
    error::Error,
    middleware::{Middleware, Service},
};

/// A simple per-peer cookie jar.
///
/// Stores and retrieves cookies using the peer's [`EndpointId`] as the key.
/// Automatically:
/// - Adds cookies to outgoing requests
/// - Extracts `Set-Cookie` headers from responses
pub struct CookieJar {
    cookies: DashMap<EndpointId, DashMap<String, Cookie<'static>>>,
}

impl CookieJar {
    /// Constructs the cookie jar
    pub fn new() -> Self {
        Self {
            cookies: DashMap::new(),
        }
    }

    /// Returns all cookies for a given peer.
    ///
    /// Returned cookies are cloned because the underlying DashMap guard
    /// cannot be held across API boundaries.
    pub fn get_cookies_for(&self, id: &EndpointId) -> Vec<Cookie<'static>> {
        match self.cookies.get(id) {
            Some(map) => map.iter().map(|entry| entry.value().clone()).collect(),
            None => Vec::new(),
        }
    }

    /// Inserts or updates cookies for the given peer.
    ///
    /// Any cookie with the same name overwrites the existing one.
    pub fn update_cookies_for<I>(&self, id: EndpointId, cookies: I)
    where
        I: IntoIterator<Item = Cookie<'static>>,
    {
        let map = self.cookies.entry(id).or_default();
        for cookie in cookies {
            let name = cookie.name().to_string();
            map.insert(name, cookie);
        }
    }

    /// Build the Cookie header value for a peer, if any cookies exist.
    fn build_cookie_header(&self, peer_id: &EndpointId) -> Option<HeaderValue> {
        let cookies = self.get_cookies_for(peer_id);

        if cookies.is_empty() {
            return None;
        }

        let header = cookies
            .iter()
            .map(|c| c.encoded().to_string())
            .collect::<Vec<_>>()
            .join("; ");

        match HeaderValue::from_str(&header) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(error = %e, "failed to build Cookie header");
                None
            }
        }
    }

    /// Attach cookies to the outgoing request (if any exist).
    fn attach_request_cookies(&self, peer_id: &EndpointId, req: &mut http::Request<Body>) {
        if let Some(header) = self.build_cookie_header(peer_id) {
            debug!(?peer_id, cookie_header = %header.to_str().unwrap_or("<invalid>"),
                "attaching cookies to request");
            req.headers_mut().insert(COOKIE, header);
        } else {
            trace!("no cookies to attach for this peer");
        }
    }

    /// Parse all Set-Cookie headers from the response.
    fn parse_response_cookies(&self, res: &http::Response<Body>) -> Vec<Cookie<'static>> {
        let mut out = Vec::new();

        for val in res.headers().get_all(SET_COOKIE).iter() {
            match val.to_str() {
                Ok(header_str) => match Cookie::parse_encoded(header_str) {
                    Ok(cookie) => out.push(cookie.into_owned()),
                    Err(err) => {
                        warn!(header_value = %header_str, error = %err,
                              "failed to parse Set-Cookie header");
                    }
                },
                Err(_) => {
                    warn!(header_value = ?val,
                          "invalid Set-Cookie header (non-UTF8)");
                }
            }
        }

        out
    }

    /// Store parsed response cookies.
    fn store_new_cookies(&self, peer_id: EndpointId, new_cookies: Vec<Cookie<'static>>) {
        if !new_cookies.is_empty() {
            debug!(count = new_cookies.len(), ?peer_id, "storing new cookies");
            self.update_cookies_for(peer_id, new_cookies);
        }
    }
}

impl Middleware for CookieJar {
    #[instrument(skip(self, next, request), fields(uri = %request.uri()))]
    async fn handle(
        &self,
        mut request: http::Request<Body>,
        next: &impl Service,
    ) -> Result<http::Response<Body>, Error> {
        let peer_id = peer_id(request.uri())?;
        debug!(?peer_id, "handling request with cookie jar");

        // 1. Attach cookies (extracted)
        self.attach_request_cookies(&peer_id, &mut request);

        // 2. Forward to next middleware/service
        let response = next.handle(request).await?;

        // 3. Parse cookies (extracted)
        let parsed = self.parse_response_cookies(&response);

        // 4. Store them (extracted)
        self.store_new_cookies(peer_id, parsed);

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use cookie::Cookie;
    use http::{Request, Response};

    fn random_peer() -> EndpointId {
        // any stable ID is fine for unit tests
        EndpointId::from_bytes(&[1u8; 32]).unwrap()
    }

    // -------------------------------
    // Tests
    // -------------------------------

    #[test]
    fn test_update_and_get_cookies_for() {
        let jar = CookieJar::new();
        let peer = random_peer();

        let c1 = Cookie::build(("a", "1")).build().into_owned();
        let c2 = Cookie::build(("b", "2")).build().into_owned();

        jar.update_cookies_for(peer, vec![c1.clone(), c2.clone()]);

        let got = jar.get_cookies_for(&peer);

        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|c| c.name() == "a" && c.value() == "1"));
        assert!(got.iter().any(|c| c.name() == "b" && c.value() == "2"));
    }

    #[test]
    fn test_build_cookie_header_empty() {
        let jar = CookieJar::new();
        let peer = random_peer();

        assert!(jar.build_cookie_header(&peer).is_none());
    }

    #[test]
    fn test_build_cookie_header_non_empty() {
        let jar = CookieJar::new();
        let peer = random_peer();

        jar.update_cookies_for(
            peer,
            vec![
                Cookie::build(("a", "1")).build().into_owned(),
                Cookie::build(("b", "2")).build().into_owned(),
            ],
        );

        let header = jar.build_cookie_header(&peer).unwrap();
        let s = header.to_str().unwrap();

        assert!(s.contains("a=1"));
        assert!(s.contains("b=2"));
    }

    #[test]
    fn test_attach_request_cookies() {
        let jar = CookieJar::new();
        let peer = random_peer();

        jar.update_cookies_for(peer, vec![Cookie::build(("x", "y")).build().into_owned()]);

        let mut req = Request::new(Body::default());
        jar.attach_request_cookies(&peer, &mut req);

        let h = req
            .headers()
            .get(http::header::COOKIE)
            .expect("cookie header expected");

        assert_eq!(h.to_str().unwrap(), "x=y");
    }

    #[test]
    fn test_parse_response_cookies() {
        let jar = CookieJar::new();

        let mut resp = Response::new(Body::default());
        resp.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_static("foo=bar; Path=/"));
        resp.headers_mut()
            .append(SET_COOKIE, HeaderValue::from_static("abc=xyz; HttpOnly"));

        let out = jar.parse_response_cookies(&resp);

        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|c| c.name() == "foo" && c.value() == "bar"));
        assert!(out.iter().any(|c| c.name() == "abc" && c.value() == "xyz"));
    }

    #[test]
    fn test_store_new_cookies() {
        let jar = CookieJar::new();
        let peer = random_peer();

        let new = vec![
            Cookie::build(("a", "1")).build().into_owned(),
            Cookie::build(("b", "2")).build().into_owned(),
        ];

        jar.store_new_cookies(peer, new);

        let got = jar.get_cookies_for(&peer);

        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|c| c.name() == "a"));
        assert!(got.iter().any(|c| c.name() == "b"));
    }
}
