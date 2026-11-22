//! Explicit fork-to-upstream PR creation, using the same bounded forge worker.

use crate::transport::HostKey;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct CreatePullRequest {
    pub host: String,
    pub base_repository: String,
    pub base_branch: String,
    pub head_repository: String,
    pub head_branch: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub push: Option<PullRequestPush>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PullRequestPush {
    pub remote: String,
    pub commit: String,
    /// The forge's advertised host at review time, accepted only for SSH remotes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_host_alias: Option<HostKey>,
}

pub fn valid_repository(name: &str) -> bool {
    let parts = name.split('/').collect::<Vec<_>>();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && !matches!(*part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

pub fn repository_from_url(
    url: &str,
    host: &str,
    ssh_host_alias: Option<&HostKey>,
) -> Option<String> {
    let url = crate::matching_remote_url(url, host, ssh_host_alias)?;
    let name = url.path().trim_matches('/');
    valid_repository(name).then(|| name.into())
}

/// Push only the reviewed commit to the reviewed remote, refusing if either moved since review.
pub(super) fn push_reviewed(
    commands: &super::ForgeCommands<'_>,
    root: &std::path::Path,
    request: &CreatePullRequest,
) -> Result<(), String> {
    let Some(push) = &request.push else {
        return Ok(());
    };
    let reference = format!("refs/heads/{}", request.head_branch);
    let current = commands.run(root, "git", &["rev-parse", "--verify", &reference])?;
    if current.trim() != push.commit {
        return Err("The local branch changed. Review the PR again before pushing".into());
    }
    let urls = commands.run(
        root,
        "git",
        &["remote", "get-url", "--push", "--all", &push.remote],
    )?;
    let urls = urls.lines().collect::<Vec<_>>();
    let [url] = urls.as_slice() else {
        return Err("Choose a remote with exactly one push URL".into());
    };
    if repository_from_url(url, &request.host, push.ssh_host_alias.as_ref()).as_deref()
        != Some(&request.head_repository)
    {
        return Err("The push remote no longer points to the selected head repository".into());
    }
    // Push only the reviewed commit and branch, not configured refspecs or other branches.
    commands
        .run(
            root,
            "git",
            &["push", "--", url, &format!("{}:{reference}", push.commit)],
        )
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_push_remotes_match_the_host_without_the_web_port() {
        let host = "git.example.test:8443";
        for remote in [
            "git@git.example.test:o/r.git",
            "ssh://git@git.example.test:2222/o/r.git",
        ] {
            assert_eq!(
                repository_from_url(remote, host, None).as_deref(),
                Some("o/r")
            );
        }
        assert!(repository_from_url("git@elsewhere.test:o/r.git", host, None).is_none());
    }

    #[test]
    fn pull_request_remotes_match_host_and_port() {
        let host = "git.example.test:8443";
        assert_eq!(
            repository_from_url("https://git.example.test:8443/o/r.git", host, None).as_deref(),
            Some("o/r"),
        );
        assert!(repository_from_url("https://git.example.test:8444/o/r.git", host, None).is_none());
        assert!(repository_from_url("https://git.example.test/o/r.git", host, None).is_none());
    }

    #[test]
    fn the_advertised_hostname_is_an_ssh_only_alias() {
        let host = "git.internal.example:8443";
        let alias = HostKey::new("forge.example:9443");
        for remote in [
            "git@forge.example:o/r.git",
            "ssh://git@forge.example:2222/o/r.git",
            "git@git.internal.example:o/r.git",
            "https://git.internal.example:8443/o/r.git",
        ] {
            assert_eq!(
                repository_from_url(remote, host, Some(&alias)).as_deref(),
                Some("o/r"),
                "{remote}",
            );
        }
        for remote in [
            "git@elsewhere.example:o/r.git",
            "https://forge.example:9443/o/r.git",
            "https://git.internal.example:9443/o/r.git",
            "https://git.internal.example/o/r.git",
        ] {
            assert!(
                repository_from_url(remote, host, Some(&alias)).is_none(),
                "{remote}"
            );
        }
    }
}
