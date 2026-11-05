//! Authorized redirect-origin matching for runtime OAuth.

use tokio_postgres::GenericClient;
use url::Url;
use uuid::Uuid;

use crate::AuthError;

const RESERVED_URI_SCHEMES: &str = include_str!("reserved_uri_schemes.txt");

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedOrigin {
    pub service: String,
    pub params: Vec<String>,
}

/// Loads the application's OAuth redirect-origin rows.
///
/// # Errors
///
/// Returns PostgreSQL failures unchanged.
pub async fn authorized_origins<C>(
    client: &C,
    app_id: Uuid,
) -> Result<Vec<AuthorizedOrigin>, AuthError>
where
    C: GenericClient + Sync,
{
    Ok(client
        .query(
            "SELECT service, params FROM app_authorized_redirect_origins WHERE app_id = $1",
            &[&app_id],
        )
        .await?
        .into_iter()
        .map(|row| AuthorizedOrigin {
            service: row.get(0),
            params: row.get(1),
        })
        .collect())
}

#[must_use]
pub fn is_authorized_origin(
    origins: &[AuthorizedOrigin],
    candidate: &str,
    use_shared_credentials: bool,
) -> bool {
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    origins.iter().any(|origin| matches_origin(origin, &url))
        || (use_shared_credentials && shared_credentials_default(&url))
}

fn matches_origin(origin: &AuthorizedOrigin, url: &Url) -> bool {
    let Some(raw_host) = url.host_str() else {
        return origin.service == "custom-scheme"
            && origin
                .params
                .first()
                .is_some_and(|scheme| is_custom_scheme(url.scheme()) && scheme == url.scheme());
    };
    let host = url
        .port()
        .map_or_else(|| raw_host.to_owned(), |port| format!("{raw_host}:{port}"));
    match origin.service.as_str() {
        "generic" => origin.params.first() == Some(&host) && web_scheme_allowed(url),
        "netlify" => {
            url.scheme() == "https"
                && origin
                    .params
                    .first()
                    .is_some_and(|site| netlify_site(&host).as_deref() == Some(site))
        }
        "vercel" => match origin.params.as_slice() {
            [suffix, project] => {
                url.scheme() == "https" && vercel_host_matches(raw_host, suffix, project)
            }
            _ => false,
        },
        "custom-scheme" => origin
            .params
            .first()
            .is_some_and(|scheme| is_custom_scheme(url.scheme()) && scheme == url.scheme()),
        _ => false,
    }
}

fn netlify_site(host: &str) -> Option<String> {
    if !(host.ends_with(".netlify.app") || host.ends_with(".netlify.live")) {
        return None;
    }
    let name = host
        .strip_suffix(".netlify.app")
        .or_else(|| host.strip_suffix(".netlify.live"))?;
    Some(
        name.rsplit_once("--")
            .map_or(name, |(_, site)| site)
            .to_owned(),
    )
}

fn vercel_host_matches(host: &str, suffix: &str, project: &str) -> bool {
    let suffix = suffix.trim_start_matches('.');
    let Some(rest) = host.strip_prefix(project) else {
        return false;
    };
    let Some(deployment) = rest.strip_suffix(&format!(".{suffix}")) else {
        return false;
    };
    deployment.is_empty()
        || (deployment.len() > 1
            && deployment.starts_with('-')
            && deployment[1..]
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
}

fn web_scheme_allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1")),
        _ => false,
    }
}

fn shared_credentials_default(url: &Url) -> bool {
    (matches!(url.scheme(), "http" | "https")
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")))
        || url.scheme() == "exp"
}

/// Returns whether a URI scheme is eligible for an application-owned custom
/// redirect origin.
#[must_use]
pub fn is_custom_scheme(scheme: &str) -> bool {
    !RESERVED_URI_SCHEMES
        .lines()
        .any(|reserved| reserved == scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_services_and_shared_defaults() {
        let origins = vec![
            AuthorizedOrigin {
                service: "generic".to_owned(),
                params: vec!["app.example.com:444".to_owned()],
            },
            AuthorizedOrigin {
                service: "netlify".to_owned(),
                params: vec!["my-site".to_owned()],
            },
            AuthorizedOrigin {
                service: "vercel".to_owned(),
                params: vec!["vercel.app".to_owned(), "project".to_owned()],
            },
            AuthorizedOrigin {
                service: "custom-scheme".to_owned(),
                params: vec!["myapp".to_owned()],
            },
        ];
        assert!(is_authorized_origin(
            &origins,
            "https://app.example.com:444/callback",
            false
        ));
        assert!(is_authorized_origin(
            &origins,
            "https://deploy-1--my-site.netlify.app/callback",
            false
        ));
        assert!(is_authorized_origin(
            &origins,
            "https://project-git-main.vercel.app/callback",
            false
        ));
        assert!(is_authorized_origin(
            &origins,
            "https://project-git-main-team.vercel.app/callback",
            false
        ));
        assert!(!is_authorized_origin(
            &origins,
            "https://projectevil.vercel.app/callback",
            false
        ));
        assert!(!is_authorized_origin(
            &origins,
            "http://project-git-main.vercel.app/callback",
            false
        ));
        assert!(!is_authorized_origin(
            &origins,
            "http://app.example.com:444/callback",
            false
        ));
        assert!(is_authorized_origin(&origins, "myapp://callback", false));
        assert!(!is_authorized_origin(
            &[],
            "https://localhost/callback",
            false
        ));
        assert!(is_authorized_origin(
            &[],
            "http://localhost:3000/callback",
            true
        ));
        assert!(is_authorized_origin(&[], "exp://127.0.0.1/callback", true));
        assert!(!is_custom_scheme("com-eventbrite-attendee"));
        assert!(!is_custom_scheme("z39.50r"));
        assert!(is_custom_scheme("my-company-app"));
    }
}
