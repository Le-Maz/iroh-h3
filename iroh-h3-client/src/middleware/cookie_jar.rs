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

        // ---- 1. Attach cookies to outgoing request ----
        if let Some(map) = self.cookies.get(&peer_id) {
            if !map.is_empty() {
                let cookie_header = map
                    .iter()
                    .map(|entry| entry.value().encoded().to_string())
                    .collect::<Vec<_>>()
                    .join("; ");

                debug!(cookie_header = %cookie_header, "attaching cookies to request");

                request.headers_mut().insert(
                    COOKIE,
                    HeaderValue::from_str(&cookie_header).map_err(http::Error::from)?,
                );
            } else {
                trace!("no cookies to attach for this peer");
            }
        } else {
            trace!("no cookie map exists yet for this peer");
        }

        // ---- 2. Forward the request ----
        debug!("forwarding request to next middleware/service");
        let response = next.handle(request).await?;

        // ---- 3. Parse Set-Cookie headers ----
        let set_cookie_values = response.headers().get_all(SET_COOKIE);

        for val in set_cookie_values.iter() {
            if let Ok(header_str) = val.to_str() {
                match Cookie::parse_encoded(header_str) {
                    Ok(cookie) => {
                        let owned: Cookie<'static> = cookie.into_owned();
                        let name = owned.name().to_string();
                        debug!(cookie_name = %name, "storing cookie from response");

                        self.cookies.entry(peer_id).or_default().insert(name, owned);
                    }
                    Err(err) => {
                        warn!(header_value = %header_str, error = %err, "failed to parse Set-Cookie header");
                    }
                }
            } else {
                warn!(header_value = ?val, "invalid Set-Cookie header (non-UTF8)");
            }
        }

        Ok(response)
    }
}
