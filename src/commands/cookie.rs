//! Cookie management commands.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::session::PageSession;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn cdp_value(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

pub struct SetCookie<'a> {
    pub name: &'a str,
    pub value: &'a str,
    pub domain: &'a str,
    pub path: &'a str,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: Option<SameSite>,
}

pub fn set(sess: &mut PageSession, cookie: &SetCookie<'_>) -> Result<()> {
    let result = sess.call("Network.setCookie", set_cookie_params(cookie))?;
    if result["success"].as_bool() == Some(false) {
        return Err(crate::hint::hint_error(
            format!(
                "failed to set cookie `{}` for {}{}",
                cookie.name, cookie.domain, cookie.path
            ),
            "check that the domain and SameSite/Secure combination is accepted by Chrome",
            None,
        ));
    }
    Ok(())
}

pub fn list(sess: &mut PageSession) -> Result<()> {
    let url = current_url(sess)?;
    for line in format_cookie_list(&cookies_for_urls(sess, &[url])?) {
        println!("{line}");
    }
    Ok(())
}

pub fn get(sess: &mut PageSession, name: &str) -> Result<()> {
    let url = current_url(sess)?;
    let cookies = cookies_for_urls(sess, std::slice::from_ref(&url))?;
    if let Some(value) = find_cookie_value(&cookies, name) {
        println!("{value}");
        return Ok(());
    }
    Err(crate::hint::hint_error(
        format!("cookie {name:?} not found for {url}"),
        "list visible cookies with `rdny cookie list`",
        None,
    ))
}

pub fn delete(sess: &mut PageSession, name: &str, domain: &str, path: &str) -> Result<()> {
    let cookies = all_cookies(sess)?;
    if !cookie_exists_for_delete(&cookies, name, domain, path) {
        return Err(crate::hint::hint_error(
            format!("cookie {name:?} not found for {domain}{path}"),
            "check the cookie domain and path with `rdny cookie list`",
            None,
        ));
    }
    sess.call(
        "Network.deleteCookies",
        json!({ "name": name, "domain": domain, "path": path }),
    )?;
    Ok(())
}

fn current_url(sess: &mut PageSession) -> Result<String> {
    sess.eval("location.href")?
        .as_str()
        .map(str::to_string)
        .context("location.href is not a string")
}

fn cookies_for_urls(sess: &mut PageSession, urls: &[String]) -> Result<Vec<Value>> {
    let result = sess.call("Network.getCookies", json!({ "urls": urls }))?;
    extract_cookies(result)
}

fn all_cookies(sess: &mut PageSession) -> Result<Vec<Value>> {
    extract_cookies(sess.call("Network.getCookies", json!({}))?)
}

fn extract_cookies(result: Value) -> Result<Vec<Value>> {
    result["cookies"]
        .as_array()
        .cloned()
        .context("Network.getCookies response missing cookies")
}

pub fn set_cookie_params(cookie: &SetCookie<'_>) -> Value {
    let mut params = json!({
        "name": cookie.name,
        "value": cookie.value,
        "domain": cookie.domain,
        "path": cookie.path,
        "secure": cookie.secure,
        "httpOnly": cookie.http_only,
    });
    if let Some(same_site) = cookie.same_site {
        params["sameSite"] = json!(same_site.cdp_value());
    }
    params
}

pub fn format_cookie_list(cookies: &[Value]) -> Vec<String> {
    let mut lines: Vec<_> = cookies
        .iter()
        .filter_map(|cookie| {
            Some(format!(
                "{}={}",
                cookie["name"].as_str()?,
                cookie["value"].as_str()?
            ))
        })
        .collect();
    lines.sort_by(|a, b| {
        let an = a.split_once('=').map_or(a.as_str(), |(name, _)| name);
        let bn = b.split_once('=').map_or(b.as_str(), |(name, _)| name);
        an.cmp(bn)
    });
    lines
}

pub fn find_cookie_value(cookies: &[Value], name: &str) -> Option<String> {
    cookies.iter().find_map(|cookie| {
        (cookie["name"].as_str()? == name).then(|| cookie["value"].as_str().map(str::to_string))?
    })
}

pub fn cookie_exists_for_delete(cookies: &[Value], name: &str, domain: &str, path: &str) -> bool {
    cookies.iter().any(|cookie| {
        cookie["name"].as_str() == Some(name)
            && cookie["path"].as_str() == Some(path)
            && cookie["domain"]
                .as_str()
                .is_some_and(|cookie_domain| domains_equal(cookie_domain, domain))
    })
}

fn domains_equal(left: &str, right: &str) -> bool {
    left.trim_start_matches('.') == right.trim_start_matches('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_cookie_list_sorted_by_name() {
        let cookies = vec![
            json!({ "name": "z", "value": "last" }),
            json!({ "name": "a", "value": "first" }),
        ];
        assert_eq!(format_cookie_list(&cookies), vec!["a=first", "z=last"]);
    }

    #[test]
    fn finds_cookie_value_by_exact_name() {
        let cookies = vec![json!({ "name": "sid", "value": "abc" })];
        assert_eq!(find_cookie_value(&cookies, "sid"), Some("abc".to_string()));
        assert_eq!(find_cookie_value(&cookies, "SID"), None);
    }

    #[test]
    fn matches_delete_cookie_with_leading_dot_domain_equivalence() {
        let cookies = vec![json!({ "name": "sid", "domain": ".example.com", "path": "/" })];
        assert!(cookie_exists_for_delete(
            &cookies,
            "sid",
            "example.com",
            "/"
        ));
        assert!(cookie_exists_for_delete(
            &cookies,
            "sid",
            ".example.com",
            "/"
        ));
        assert!(!cookie_exists_for_delete(&cookies, "sid", "other.com", "/"));
        assert!(!cookie_exists_for_delete(
            &cookies,
            "sid",
            "example.com",
            "/app"
        ));
    }

    #[test]
    fn set_cookie_params_omit_absent_optionals() {
        let params = set_cookie_params(&SetCookie {
            name: "n",
            value: "v",
            domain: "example.com",
            path: "/",
            secure: false,
            http_only: false,
            same_site: None,
        });
        assert_eq!(params["secure"], false);
        assert_eq!(params["httpOnly"], false);
        assert!(params.get("sameSite").is_none());

        let params = set_cookie_params(&SetCookie {
            name: "n",
            value: "v",
            domain: "example.com",
            path: "/",
            secure: true,
            http_only: true,
            same_site: Some(SameSite::Lax),
        });
        assert_eq!(params["sameSite"], "Lax");
    }
}
