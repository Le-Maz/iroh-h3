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
}

impl Middleware for CookieJar {
    async fn handle(
        &self,
        mut request: http::Request<Body>,
        next: &impl Service,
    ) -> Result<http::Response<Body>, Error> {
        let peer_id = peer_id(request.uri())?;

        // ---- 1. Attach cookies to outgoing request ----

        if let Some(map) = self.cookies.get(&peer_id) {
            if !map.is_empty() {
                // Encode all cookies: key=value pairs
                let cookie_header = map
                    .iter()
                    .map(|entry| entry.value().encoded().to_string())
                    .collect::<Vec<_>>()
                    .join("; ");

                request.headers_mut().insert(
                    COOKIE,
                    HeaderValue::from_str(&cookie_header).map_err(http::Error::from)?,
                );
            }
        }

        // ---- 2. Forward the request ----

        let response = next.handle(request).await?;

        // ---- 3. Parse Set-Cookie headers ----

        let set_cookie_values = response.headers().get_all(SET_COOKIE);

        for val in set_cookie_values.iter() {
            if let Ok(header_str) = val.to_str() {
                if let Ok(cookie) = Cookie::parse_encoded(header_str) {
                    // Convert to owned `'static` cookie
                    let owned: Cookie<'static> = cookie.into_owned();

                    let name = owned.name().to_string();

                    self.cookies.entry(peer_id).or_default().insert(name, owned);
                }
            }
        }

        Ok(response)
    }
}
