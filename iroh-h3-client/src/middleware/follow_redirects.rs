//! FollowRedirects middleware for IrohH3Client
//!
//! Automatically follows HTTP redirects up to a configured limit.
//! - 301, 302, 303 → method changed to GET, body dropped
//! - 307, 308 → same method, body is cloned if possible

use std::{mem::take, ops::ControlFlow};

use crate::{
    body::Body,
    error::Error,
    middleware::{Middleware, Service},
};
use http::{Request, Response, StatusCode, request::Parts};
use tracing::{debug, instrument, warn};

/// Middleware that automatically follows HTTP redirects.
pub struct FollowRedirects {
    /// Maximum number of redirects to follow before returning an error.
    pub max_redirects: usize,
}

impl FollowRedirects {
    /// Construct the middleware
    pub fn new(max_redirects: usize) -> Self {
        Self { max_redirects }
    }

    fn resolve_redirect(base: &http::Uri, location: &str) -> Result<http::Uri, Error> {
        // Absolute URI? Only accept it if it has both scheme and authority.
        if let Ok(uri) = location.parse::<http::Uri>() {
            if uri.scheme().is_some() && uri.authority().is_some() {
                return Ok(uri);
            }
        }

        // Otherwise treat it as relative
        let base_scheme = base
            .scheme()
            .ok_or_else(|| Error::Other("Base URI missing scheme".into()))?;
        let base_authority = base
            .authority()
            .ok_or_else(|| Error::Other("Base URI missing authority".into()))?;

        // If it starts with '/', it's an absolute path -> replace path and query
        if location.starts_with('/') {
            return format!(
                "{scheme}://{auth}{loc}",
                scheme = base_scheme,
                auth = base_authority,
                loc = location
            )
            .parse::<http::Uri>()
            .map_err(|e| Error::Other(format!("Invalid redirect URI: {e}")));
        }

        // Path-relative redirect ("foo", "../bar")
        let mut base_path = base
            .path()
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .unwrap_or("");

        if !base_path.starts_with('/') {
            base_path = "/";
        }

        let combined = format!(
            "{scheme}://{auth}{path}/{loc}",
            scheme = base_scheme,
            auth = base_authority,
            path = base_path.trim_end_matches('/'),
            loc = location,
        );

        combined
            .parse::<http::Uri>()
            .map_err(|e| Error::Other(format!("Invalid relative redirect URI: {e}")))
    }

    /// Perform a single redirect step.
    ///
    /// This is where you want per-redirect spans.
    #[instrument(
        skip(self, parts, body_slot, redirects, next),
        fields(
            redirects = *redirects,
        )
    )]
    async fn redirect_step(
        &self,
        parts: &mut Parts,
        body_slot: &mut Body,
        redirects: &mut usize,
        next: &impl Service,
    ) -> Result<ControlFlow<Response<Body>>, Error> {
        // Try to clone, else move out of slot
        let body = body_slot.try_clone().unwrap_or_else(|| take(body_slot));

        debug!("sending request");

        let response = next
            .handle(Request::from_parts(parts.clone(), body))
            .await?;

        let status = response.status();
        debug!(%status, "received response");

        match status {
            StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT => {
                *redirects += 1;

                debug!(redirects = *redirects, %status, "redirect received");

                if *redirects > self.max_redirects {
                    warn!(
                        redirects = *redirects,
                        max = self.max_redirects,
                        "too many redirects"
                    );
                    return Err(Error::Other(format!(
                        "Too many redirects (> {})",
                        self.max_redirects
                    )));
                }

                // Extract Location
                let location = response
                    .headers()
                    .get(http::header::LOCATION)
                    .ok_or_else(|| Error::Other("Redirect missing Location header".into()))?
                    .to_str()
                    .map_err(|_| Error::Other("Invalid Location header".into()))?
                    .to_string();

                debug!(%location, "following redirect");

                parts.uri = Self::resolve_redirect(&parts.uri, &location)?;

                match status {
                    StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND | StatusCode::SEE_OTHER => {
                        // 301/302/303 → method becomes GET, body dropped
                        debug!("redirect requires GET; dropping body");

                        parts.method = http::Method::GET;
                        parts.headers.remove(http::header::CONTENT_LENGTH);
                        parts.headers.remove(http::header::CONTENT_TYPE);
                        *body_slot = Body::empty();
                    }

                    StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                        // 307/308 → keep method
                        debug!("redirect preserves method; keeping body");
                        // body_slot already contains the correct body because try_clone or take()
                    }

                    _ => unreachable!(),
                }

                Ok(ControlFlow::Continue(()))
            }

            _ => {
                debug!(%status, "final response");
                Ok(ControlFlow::Break(response))
            }
        }
    }
}

impl Middleware for FollowRedirects {
    #[instrument(
        skip(self, next, request),
        fields(
            method = %request.method(),
            uri = %request.uri(),
            max_redirects = self.max_redirects
        )
    )]
    async fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> Result<Response<Body>, Error> {
        let mut redirects = 0;
        let (mut parts, mut body) = request.into_parts();

        loop {
            match self
                .redirect_step(&mut parts, &mut body, &mut redirects, next)
                .await?
            {
                ControlFlow::Continue(_) => continue,
                ControlFlow::Break(response) => return Ok(response),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Uri;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn absolute_redirect_is_used_directly() {
        let base = uri("https://example.com/foo/bar");
        let loc = "https://other.com/new/path";

        let resolved = FollowRedirects::resolve_redirect(&base, loc).unwrap();
        assert_eq!(resolved, uri("https://other.com/new/path"));
    }

    #[test]
    fn absolute_path_redirect_replaces_path_and_query() {
        let base = uri("https://example.com/foo/bar?x=1");
        let loc = "/new/path";

        let resolved = FollowRedirects::resolve_redirect(&base, loc).unwrap();
        assert_eq!(resolved, uri("https://example.com/new/path"));
    }

    #[test]
    fn relative_redirect_appends_to_directory() {
        let base = uri("https://example.com/foo/bar");
        let loc = "baz";

        let resolved = FollowRedirects::resolve_redirect(&base, loc).unwrap();
        assert_eq!(resolved, uri("https://example.com/foo/baz"));
    }

    #[test]
    fn parent_directory_relative_redirect() {
        let base = uri("https://example.com/foo/bar");
        let loc = "../up";

        let resolved = FollowRedirects::resolve_redirect(&base, loc).unwrap();
        assert_eq!(resolved, uri("https://example.com/foo/../up"));
        // NOTE: Uri does not normalize paths; this is correct.
    }

    #[test]
    fn missing_scheme_in_base_uri_is_error() {
        let base = uri("//example.com/foo"); // scheme missing
        let loc = "/redir";

        let err = FollowRedirects::resolve_redirect(&base, loc).unwrap_err();
        assert!(matches!(err, Error::Other(_)));
    }
}
