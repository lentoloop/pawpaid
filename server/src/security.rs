//! Outermost runtime response boundary, including errors and unmatched routes.
use axum::{
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json, Router,
};
use contracts::{ApiError, ErrorCode};

// Built assets and fonts are same-origin. The existing sign-in page uses React style attributes;
// permit those only, while style elements and scripts remain same-origin.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

pub fn harden(app: Router) -> Router {
    harden_for_mode(app, false)
}

pub fn harden_for_mode(app: Router, development: bool) -> Router {
    let hide_errors = !cfg!(debug_assertions) || !development;
    app.layer(axum::middleware::from_fn_with_state(
        hide_errors,
        response_boundary,
    ))
}

async fn response_boundary(
    State(hide_errors): State<bool>,
    request: Request,
    next: Next,
) -> Response {
    // A handler panic cannot turn into a debug response or terminate the listener.
    let mut response = match tokio::spawn(next.run(request)).await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let status = response.status();
    if status == StatusCode::INTERNAL_SERVER_ERROR || (hide_errors && status.is_server_error()) {
        let code = response
            .extensions()
            .get::<ErrorCode>()
            .copied()
            .unwrap_or(ErrorCode::AdapterUnavailable);
        response = (status, Json(ApiError::new(code, "internal server error"))).into_response();
    }
    for (name, value) in [
        ("content-security-policy", CSP),
        (
            "strict-transport-security",
            "max-age=31536000; includeSubDomains",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::get,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn headers_cover_success_rejections_fallback_and_internal_errors() {
        let app = harden(
            Router::new()
                .route("/ok", get(|| async { "ok" }))
                .route("/bad", get(|| async { StatusCode::BAD_REQUEST }))
                .route("/limited", get(|| async { StatusCode::TOO_MANY_REQUESTS }))
                .route(
                    "/error",
                    get(|| async {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "synthetic private diagnostic",
                        )
                    }),
                )
                .route(
                    "/unavailable",
                    get(|| async {
                        crate::http::HttpError(ApiError::new(
                            ErrorCode::StorageUnavailable,
                            "synthetic private diagnostic",
                        ))
                    }),
                ),
        );
        for (path, status) in [
            ("/ok", 200),
            ("/bad", 400),
            ("/limited", 429),
            ("/absent", 404),
            ("/error", 500),
            ("/unavailable", 503),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["content-security-policy"], CSP);
            assert_eq!(
                response.headers()["strict-transport-security"],
                "max-age=31536000; includeSubDomains"
            );
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
            assert_eq!(response.headers()["x-frame-options"], "DENY");
            if status == 500 || status == 503 {
                assert_eq!(response.headers()["content-type"], "application/json");
                let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
                let error: ApiError = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(error.message, "internal server error");
                assert_eq!(
                    error.code,
                    if status == 503 {
                        ErrorCode::StorageUnavailable
                    } else {
                        ErrorCode::AdapterUnavailable
                    }
                );
                assert!(!std::str::from_utf8(&bytes)
                    .unwrap()
                    .contains("synthetic private"));
            }
        }
    }
    #[tokio::test]
    async fn panicking_handler_returns_generic_500_with_headers() {
        let app = harden(Router::new().route(
            "/panic",
            get(|| async {
                panic!("synthetic private diagnostic");
                #[allow(unreachable_code)]
                "unreachable"
            }),
        ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/panic")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()["content-security-policy"], CSP);
        assert_eq!(response.headers()["content-type"], "application/json");
        let error: ApiError =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
        assert_eq!(error.message, "internal server error");
        assert_eq!(error.code, ErrorCode::AdapterUnavailable);
    }
}
