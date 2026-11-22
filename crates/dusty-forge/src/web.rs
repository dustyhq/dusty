//! The web address of a Git remote.

use crate::transport::HostKey;

/// Convert Git transports to HTTPS; keep HTTP(S) schemes and web ports intact.
pub fn web_base(remote: &str) -> Option<reqwest::Url> {
    remote_web_url(remote).map(|(url, _)| url)
}

/// The repository URL of a remote on the routing authority. For SSH only,
/// accept the advertised hostname as well and ignore the Git transport port.
pub fn matching_remote_url(
    remote: &str,
    host: &str,
    ssh_host_alias: Option<&HostKey>,
) -> Option<reqwest::Url> {
    let (url, scheme) = remote_web_url(remote)?;
    let expected = HostKey::parse_authority(host)?;
    let found = HostKey::from_url(&url)?;
    let matches = if scheme == "ssh" {
        found.host == expected.host || ssh_host_alias.is_some_and(|alias| alias.host == found.host)
    } else {
        found == expected
    };
    matches.then_some(url)
}

fn remote_web_url(remote: &str) -> Option<(reqwest::Url, &str)> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    let (scheme, authority, path) = if let Some((scheme, rest)) = remote.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh" | "git") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        (scheme, authority, path)
    } else {
        let (authority, path) = remote.split_once(':')?;
        ("ssh", authority, path)
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() || path.is_empty() {
        return None;
    }
    let is_web = matches!(scheme, "http" | "https");
    let web_scheme = if is_web { scheme } else { "https" };
    let mut base = reqwest::Url::parse(&format!("{web_scheme}://{authority}/{path}")).ok()?;
    if !is_web {
        // A Git transport port is not the web port.
        base.set_port(None).ok()?;
    }
    Some((base, scheme))
}
