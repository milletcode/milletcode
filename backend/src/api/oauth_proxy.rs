use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

use crate::AppState;

/// /oauth2 与 /oauth2/ 的空路径兜底：{*path} 通配不匹配空段，这两个变体
/// 曾落到 SPA fallback 匿名返回前端入口页骨架（信息泄露）。显式 404。
pub async fn oauth_root_not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Not found")
}

/// Reverse proxy handler for OAuth2 Proxy
/// Proxies all /oauth2/* requests to the configured oauth2_proxy.proxy_domain
pub async fn oauth_proxy_handler(
    State(state): State<Arc<AppState>>,
    req: Request,
) -> impl IntoResponse {
    let auth = &state.config_manager.config().auth;

    // Get OAuth2 Proxy domain from config
    let proxy_domain = match &auth.oauth2_proxy.proxy_domain {
        Some(domain) if !domain.is_empty() => domain,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "OAuth2 Proxy domain not configured",
            )
                .into_response();
        }
    };

    // Build target URL with protocol following
    let original_uri = req.uri().clone();
    let domain = proxy_domain.trim_end_matches('/');

    // Determine scheme: follow client protocol or use config-specified scheme
    let scheme = if domain.starts_with("http://") {
        "http".to_string()
    } else if domain.starts_with("https://") {
        "https".to_string()
    } else {
        // No scheme in config, follow client protocol
        // Check X-Forwarded-Proto header first (for reverse proxy scenarios)
        let client_scheme = req.headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .or_else(|| original_uri.scheme_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "https".to_string()); // Default to https
        client_scheme
    };

    // Strip scheme from domain if present
    let clean_domain = if domain.starts_with("http://") {
        domain.trim_start_matches("http://")
    } else if domain.starts_with("https://") {
        domain.trim_start_matches("https://")
    } else {
        domain
    };

    let target_url = format!("{}://{}{}", scheme, clean_domain, original_uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(""));

    // Create HTTP client with optional custom SSL certificate
    let mut client_builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none()); // Don't follow redirects, pass them through

    // If oauth2_proxy.ssl_pem is configured, add it as a custom root certificate
    if let Some(pem_path) = &auth.oauth2_proxy.ssl_pem {
        // Expand ~ to home directory
        let expanded_path = if pem_path.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(&pem_path[2..]).to_string_lossy().to_string()
            } else {
                pem_path.clone()
            }
        } else {
            pem_path.clone()
        };

        match tokio::fs::read_to_string(&expanded_path).await {
            Ok(pem_content) => match reqwest::Certificate::from_pem(pem_content.as_bytes()) {
                Ok(cert) => {
                    client_builder = client_builder.add_root_certificate(cert);
                }
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Failed to parse SSL certificate: {}", e),
                    )
                        .into_response();
                }
            },
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read SSL certificate file: {}", e),
                )
                    .into_response();
            }
        }
    } else {
        // No custom cert, accept invalid certs for self-signed/internal CAs
        client_builder = client_builder.danger_accept_invalid_certs(true);
    }

    let client = match client_builder.build() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create HTTP client: {}", e),
            )
                .into_response();
        }
    };

    // Prepare headers to forward
    let mut proxy_headers = reqwest::header::HeaderMap::new();

    // Copy original headers (except Host which we'll set explicitly)
    for (name, value) in req.headers().iter() {
        // Skip headers that shouldn't be forwarded
        if name == header::HOST || name == header::CONNECTION || name == header::CONTENT_LENGTH {
            continue;
        }
        if let Ok(val_str) = value.to_str() {
            if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()) {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(val_str) {
                    proxy_headers.insert(header_name, header_value);
                }
            }
        }
    }

    // Set required OAuth2 Proxy headers
    if let Some(host) = req.headers().get(header::HOST) {
        if let Ok(host_str) = host.to_str() {
            proxy_headers.insert(reqwest::header::HOST, host_str.parse().unwrap());
        }
    }

    // X-Real-IP: client's IP
    let client_ip = req
        .headers()
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    proxy_headers.insert("X-Real-IP", client_ip.parse().unwrap());

    // X-Scheme: original request scheme
    let scheme = if req.uri().scheme_str() == Some("https") || req.headers().get("X-Forwarded-Proto").and_then(|v| v.to_str().ok()) == Some("https") {
        "https"
    } else {
        "http"
    };
    proxy_headers.insert("X-Scheme", scheme.parse().unwrap());

    // X-Auth-Request-Redirect: the original request URI
    let redirect_uri = original_uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    proxy_headers.insert("X-Auth-Request-Redirect", redirect_uri.parse().unwrap());

    // Get request method
    let method = match req.method().as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        _ => reqwest::Method::GET,
    };

    // Build the proxy request
    let mut proxy_req = client.request(method, &target_url).headers(proxy_headers);

    // Add body for POST/PUT/PATCH requests
    if matches!(req.method().as_str(), "POST" | "PUT" | "PATCH") {
        // Extract body as bytes
        let body_bytes = match axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to read request body: {}", e),
                )
                    .into_response();
            }
        };
        proxy_req = proxy_req.body(body_bytes);
    }

    // Send the request
    let proxy_response = match proxy_req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to proxy request to OAuth2 Proxy: {}", e),
            )
                .into_response();
        }
    };

    // Convert response to Axum response
    let status = StatusCode::from_u16(proxy_response.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let mut response_headers = axum::http::HeaderMap::new();

    // Copy response headers
    for (name, value) in proxy_response.headers().iter() {
        if name == reqwest::header::TRANSFER_ENCODING || name == reqwest::header::CONTENT_LENGTH {
            continue;
        }
        if let Ok(val_str) = value.to_str() {
            if let Ok(header_name) = axum::http::HeaderName::from_bytes(name.as_str().as_bytes()) {
                if let Ok(header_value) = axum::http::HeaderValue::from_str(val_str) {
                    response_headers.insert(header_name, header_value);
                }
            }
        }
    }

    // Read response body
    let body_bytes = match proxy_response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("Failed to read OAuth2 Proxy response: {}", e),
            )
                .into_response();
        }
    };

    let body = Body::from(body_bytes.to_vec());

    let mut builder = Response::builder().status(status);

    // Copy response headers
    for (name, value) in response_headers.iter() {
        builder = builder.header(name, value);
    }

    builder
        .body(body)
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Failed to build response"))
                .unwrap()
        })
        .into_response()
}
