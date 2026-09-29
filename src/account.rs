//! Qobuz account sign-in: OAuth code exchange and the loopback listener that
//! catches the browser's redirect.

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::routing::get;
use axum::Router;
use tokio::sync::mpsc;

use crate::api::{ApiClient, UserInfo};
use crate::auth::Credentials;

/// Trade an OAuth authorisation code for stored-ready credentials. `api`
/// only provides the endpoint; it needs no token.
pub async fn exchange_code(api: &ApiClient, code: &str) -> Result<(Credentials, UserInfo)> {
    let mut api = api.clone();
    let (token, user_id) = api.oauth_exchange(code).await.context("Qobuz refused the authorisation code")?;
    api.set_user_token(&token);
    let info = api.user_info().await.context("signed in, but the account details are unreadable")?;
    let creds = Credentials {
        user_id: if info.id != 0 { info.id } else { user_id },
        email: info.email.clone(),
        display_name: info.display_name.clone(),
        token,
    };
    Ok((creds, info))
}

/// Answer `/login/callback` on `listener`, forwarding each code received.
pub fn serve_callback(listener: tokio::net::TcpListener, code_tx: mpsc::UnboundedSender<String>) -> tokio::task::JoinHandle<()> {
    let app = Router::new().route(
        "/login/callback",
        get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
            let code_tx = code_tx.clone();
            async move {
                match q.get("code_autorisation").or_else(|| q.get("code")) {
                    Some(code) => {
                        let _ = code_tx.send(code.clone());
                        axum::response::Html(
                            "<h2>Signed in to Qobuz.</h2><p>You can close this tab.</p>",
                        )
                    }
                    None => axum::response::Html("<h2>No sign-in code in this address.</h2>"),
                }
            }
        }),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}

/// Accept a full redirect URL, a query string, or a bare code.
pub fn extract_code(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    for key in ["code_autorisation=", "code="] {
        if let Some(i) = input.find(key) {
            let rest = &input[i + key.len()..];
            let code = rest.split(['&', '#', ' ']).next().unwrap_or("");
            return (!code.is_empty()).then(|| urlencoding::decode(code).map_or(code.to_string(), |c| c.into_owned()));
        }
    }
    // A bare code: no URL syntax at all.
    (!input.contains(['/', '?', '=', ' '])).then(|| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_extracted_from_urls_and_bare_input() {
        assert_eq!(
            extract_code("http://localhost:8473/login/callback?code_autorisation=abc123&x=1").as_deref(),
            Some("abc123")
        );
        assert_eq!(extract_code("  https://play.qobuz.com/?code_autorisation=a%2Bb ").as_deref(), Some("a+b"));
        assert_eq!(extract_code("?code=xyz").as_deref(), Some("xyz"));
        assert_eq!(extract_code("rawCode42").as_deref(), Some("rawCode42"));
        assert_eq!(extract_code("https://www.qobuz.com/signin"), None);
        assert_eq!(extract_code(""), None);
    }
}
