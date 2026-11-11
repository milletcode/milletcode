use axum::{
    extract::{Request, State},
    http::{header, HeaderValue},
    response::{IntoResponse, Redirect, Response},
};
use std::sync::Arc;

use crate::AppState;

/// Expire every cookie the browser sent, so a logout also drops cookies that
/// live on a parent domain (SSO cookies set for .example.com survive a plain
/// host-only clear). Returns raw Set-Cookie values.
///
/// Path attr variants: "/" and "/p" (the reverse-proxy token cookie is planted
/// with Path=/p, and browsers only match a clear to the exact same path).
/// Domain attr variants: host-only (no Domain attr) and, when `cookie_domain`
/// is configured, the parent-domain form. All are emitted because the browser
/// stores the variants independently and any may exist.
pub fn clear_cookie_values(cookie_header: Option<&str>, cookie_domain: Option<&str>) -> Vec<String> {
    let Some(cookies) = cookie_header else {
        return Vec::new();
    };
    let mut names: Vec<String> = cookies
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .map(|(name, _)| name.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort();
    names.dedup();

    let mut out = Vec::new();
    for name in names {
        out.push(format!("{name}=; Path=/; Max-Age=0"));
        out.push(format!("{name}=; Path=/p; Max-Age=0"));
        if let Some(domain) = cookie_domain {
            let domain = domain.trim().trim_start_matches('.');
            if !domain.is_empty() {
                out.push(format!("{name}=; Path=/; Max-Age=0; Domain={domain}"));
                out.push(format!("{name}=; Path=/p; Max-Age=0; Domain={domain}"));
            }
        }
    }
    out
}

/// GET /api/logout — reserved logout endpoint.
///
/// Currently: expires every dispatched cookie (including HttpOnly ones and
/// parent-domain SSO cookies via the configured auth.cookie_domain), then
/// redirects to `/`. Intentionally an extension point — future integrations
/// (device login checks, token revocation) hook in here.
pub async fn logout(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let auth = &state.config_manager.config().auth;

    // oauth2-proxy owns the identity cookie on its own domain — it can only
    // be cleared by oauth2-proxy itself. Its sign_out endpoint is reachable
    // through our /oauth2 reverse proxy and honors ?rd= for the bounce-back.
    if auth.effective_mode() == "oauth2_proxy" {
        return Redirect::to("/oauth2/sign_out?rd=%2F").into_response();
    }

    // plain 模式：会话只在本进程内存里，revoke + 清 cookie + 回登录页。
    if auth.effective_mode() == "plain" {
        if let Some(sid) = req
            .headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(crate::auth::plain_session_cookie_value)
        {
            state.plain_sessions.revoke(sid);
        }
        let mut resp = Redirect::to("/login").into_response();
        let clear = format!(
            "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
            crate::auth::PLAIN_SESSION_COOKIE
        );
        if let Ok(v) = clear.parse() {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        return resp;
    }

    let cookie_header = req.headers().get(header::COOKIE).and_then(|v| v.to_str().ok());
    let clear = clear_cookie_values(cookie_header, auth.oauth2_proxy.cookie_domain.as_deref());

    let mut resp = Redirect::to("/").into_response();
    {
        let headers = resp.headers_mut();
        for value in clear {
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.append(header::SET_COOKIE, v);
            }
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cookie_header() {
        assert!(clear_cookie_values(None, None).is_empty());
        assert!(clear_cookie_values(None, Some("example.com")).is_empty());
    }

    #[test]
    fn host_only_clear_without_domain() {
        let out = clear_cookie_values(Some("session=abc; other=x"), None);
        assert_eq!(out.len(), 4);
        assert!(out.contains(&"session=; Path=/; Max-Age=0".to_string()));
        assert!(out.contains(&"session=; Path=/p; Max-Age=0".to_string()));
        assert!(out.contains(&"other=; Path=/; Max-Age=0".to_string()));
        assert!(out.contains(&"other=; Path=/p; Max-Age=0".to_string()));
        assert!(out.iter().all(|v| !v.contains("Domain")));
    }

    #[test]
    fn proxy_path_cookie_cleared() {
        // _pwt is planted with Path=/p — a Path=/ clear never matches it.
        let out = clear_cookie_values(Some("_pwt=tok"), None);
        assert!(out.contains(&"_pwt=; Path=/; Max-Age=0".to_string()));
        assert!(out.contains(&"_pwt=; Path=/p; Max-Age=0".to_string()));
    }

    #[test]
    fn domain_variant_added_when_configured() {
        let out = clear_cookie_values(Some("_oauth2_proxy=tok"), Some("example.com"));
        assert_eq!(out.len(), 4);
        assert!(out.contains(&"_oauth2_proxy=; Path=/; Max-Age=0".to_string()));
        assert!(out.contains(&"_oauth2_proxy=; Path=/p; Max-Age=0".to_string()));
        assert!(out.contains(&"_oauth2_proxy=; Path=/; Max-Age=0; Domain=example.com".to_string()));
        assert!(out.contains(&"_oauth2_proxy=; Path=/p; Max-Age=0; Domain=example.com".to_string()));
    }

    #[test]
    fn leading_dot_and_whitespace_stripped() {
        let out = clear_cookie_values(Some(" a=1 ;b=2"), Some(".example.com"));
        assert!(out.contains(&"a=; Path=/; Max-Age=0; Domain=example.com".to_string()));
        assert!(out.contains(&"b=; Path=/; Max-Age=0; Domain=example.com".to_string()));
    }

    #[test]
    fn duplicate_names_cleared_once() {
        let out = clear_cookie_values(Some("a=1; a=2"), None);
        assert_eq!(
            out,
            vec![
                "a=; Path=/; Max-Age=0".to_string(),
                "a=; Path=/p; Max-Age=0".to_string(),
            ]
        );
    }

    #[test]
    fn empty_domain_config_ignored() {
        let out = clear_cookie_values(Some("a=1"), Some("  "));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|v| !v.contains("Domain")));
    }
}
