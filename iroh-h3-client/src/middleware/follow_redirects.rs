//! FollowRedirects middleware for IrohH3Client
//!
//! Automatically follows HTTP redirects up to a configured limit.
//! - 301, 302, 303 → method changed to GET, body dropped
//! - 307, 308 → same method, body is cloned if possible, otherwise error

use crate::{
    body::Body,
    error::Error,
    middleware::{Middleware, Service},
};
use http::{Request, Response, StatusCode};

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
}

impl Middleware for FollowRedirects {
    async fn handle(
        &self,
        request: Request<Body>,
        next: &impl Service,
    ) -> Result<Response<Body>, Error> {
        let mut redirects = 0;
        let (mut parts, mut body) = request.into_parts();

        loop {
            let cloned_body = body.try_clone();

            let response = next
                .handle(Request::from_parts(parts.clone(), body))
                .await?;

            match response.status() {
                StatusCode::MOVED_PERMANENTLY
                | StatusCode::FOUND
                | StatusCode::SEE_OTHER
                | StatusCode::TEMPORARY_REDIRECT
                | StatusCode::PERMANENT_REDIRECT => {
                    redirects += 1;
                    if redirects > self.max_redirects {
                        return Err(Error::Other(format!(
                            "Too many redirects (> {})",
                            self.max_redirects
                        )));
                    }

                    // Get Location header
                    let location = response
                        .headers()
                        .get(http::header::LOCATION)
                        .ok_or_else(|| Error::Other("Redirect missing Location header".into()))?
                        .to_str()
                        .map_err(|_| Error::Other("Invalid Location header".into()))?
                        .to_string();

                    parts.uri = location.parse().map_err(http::Error::from)?;

                    match response.status() {
                        StatusCode::MOVED_PERMANENTLY
                        | StatusCode::FOUND
                        | StatusCode::SEE_OTHER => {
                            // 301/302/303 → method becomes GET, body dropped
                            parts.method = http::Method::GET;
                            parts.headers.remove(http::header::CONTENT_LENGTH);
                            parts.headers.remove(http::header::CONTENT_TYPE);
                            body = Body::empty();
                        }
                        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => {
                            // 307/308 → keep method, clone body
                            body = cloned_body.as_ref().and_then(Body::try_clone).ok_or_else(
                                || {
                                    Error::Other(
                                        "Cannot follow 307/308 redirect: body not cloneable".into(),
                                    )
                                },
                            )?;
                        }
                        _ => unreachable!(),
                    }

                    continue;
                }
                _ => return Ok(response),
            }
        }
    }
}
