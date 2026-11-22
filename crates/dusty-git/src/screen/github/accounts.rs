//! Host states in the forge tabs: one
//! notice row under the list title while the host is signed out, missing its CLI, rate limited,
//! unreachable or unknown, and the accounts popover (`@`) listing every host this session has
//! talked to. `r` in the popover forgets credentials, transport choices, detection and pauses,
//! then reloads.

use super::super::render::{ellipsize, section, surface};
use super::{Form, Key, Workspace};
use crate::editor::input::{KeyCode, KeyEvent};
use crate::forge::host_state::{self, Account, HostState, Tone};
use crate::forge::request::ForgeError;
use crate::forge::transport::HostKey;
use crate::ui::{Frame, Style};
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Accounts {
    /// The host the last host-level error was about, and its state.
    pub(super) state: Option<(String, HostState)>,
    /// The last reply that reached the forge, for "cached 2m ago".
    last_success: Option<Instant>,
    pub(super) open: bool,
}

impl Accounts {
    /// A reply arrived: a host-level error becomes the notice; anything the forge answered
    /// (even "not found") clears it.
    pub(super) fn observe(&mut self, host: &str, error: Option<&ForgeError>) {
        let Some(error) = error else {
            self.state = None;
            self.last_success = Some(Instant::now());
            return;
        };
        if matches!(error, ForgeError::Cancelled) {
            return;
        }
        // The registry knows the host and its forge when the workspace does not yet (the very
        // first read failed before any repository was known).
        let account = (!host.is_empty())
            .then(|| host_state::account(&HostKey::new(host)))
            .flatten()
            .or_else(host_state::latest);
        let kind = account.as_ref().and_then(|account| account.kind);
        match HostState::from_error(kind, error) {
            Some(state) => {
                let host = if host.is_empty() {
                    account
                        .map(|account| account.host.to_string())
                        .unwrap_or_default()
                } else {
                    host.to_owned()
                };
                self.state = Some((host, state));
            }
            None => {
                self.state = None;
                self.last_success = Some(Instant::now());
            }
        }
    }

    /// The notice row, or `None` while the host is fine.
    pub(super) fn notice(&self, showing_cached: bool) -> Option<(String, Tone)> {
        let (host, state) = self.state.as_ref()?;
        let host = if host.is_empty() { "the forge" } else { host };
        let cached = self
            .last_success
            .filter(|_| showing_cached)
            .map(|at| at.elapsed());
        state.notice(host, cached)
    }

    /// Popover rows: every host this process has observed, the current one first.
    fn lines(&self, current: &str) -> Vec<(String, Tone)> {
        let mut accounts = host_state::accounts();
        if let Some((host, state)) = &self.state
            && !accounts
                .iter()
                .any(|account| account.host.to_string() == host.as_str())
        {
            accounts.push(Account::new(HostKey::new(host), None, state.clone()));
        }
        accounts.sort_by_key(|account| {
            (
                account.host.to_string() != current,
                account.host.to_string(),
            )
        });
        let mut lines: Vec<(String, Tone)> = accounts.iter().flat_map(Account::lines).collect();
        if lines.is_empty() {
            lines.push(("No accounts yet".into(), Tone::Muted));
        }
        lines
    }
}

fn style(tone: Tone) -> Style {
    match tone {
        Tone::Normal => Style::Default,
        Tone::Muted => Style::Muted,
        Tone::Warning => Style::Warning,
        Tone::Error => Style::Error,
    }
}

fn anonymous_notice(state: &HostState, host: &str) -> Option<(Vec<String>, Tone)> {
    let (text, tone) = state.notice(host, None)?;
    let shortcut = format!("{} sign in", super::states::SIGN_IN_KEY);
    if state.sign_in_advice().is_some() {
        let chip = text.strip_suffix(" · @ sign in").unwrap_or(&text);
        let mut parts = chip.split(" · ").map(str::to_owned);
        let mut notice = vec![parts.next().unwrap_or_default(), shortcut];
        notice.extend(parts);
        Some((notice, tone))
    } else {
        let chip = text.split(" · sign in: ").next().unwrap_or(&text);
        Some((vec![chip.to_owned(), shortcut], tone))
    }
}

impl Workspace {
    /// The host notice row: a host-level error, else the anonymous read-only chip
    /// (`○ Anonymous · read-only · 52/60 requests left · sign in: gh auth login`). Anonymous
    /// reads succeed, so no error sets that state; the registry knows it.
    pub(super) fn host_notice(&self, showing_cached: bool) -> Option<(String, Tone)> {
        self.host_notice_parts(showing_cached)
            .map(|(parts, tone)| (parts.join(" · "), tone))
    }

    /// The notice as parts, so a narrow pane drops whole ones (`states::fit`): the screen's
    /// own words and keys for the states it can act on, the registry's for the rest.
    pub(super) fn host_notice_parts(&self, showing_cached: bool) -> Option<(Vec<String>, Tone)> {
        let cached = showing_cached
            .then(|| {
                self.shown_age()
                    .or_else(|| self.accounts.last_success.map(|at| at.elapsed()))
            })
            .flatten();
        if let Some((host, state)) = &self.accounts.state {
            if state.read_only() {
                return anonymous_notice(state, host);
            }
            return super::states::host_notice(state, host, cached).or_else(|| {
                self.accounts
                    .notice(showing_cached)
                    .map(|(text, tone)| (vec![text], tone))
            });
        }
        let state = self.host_state().filter(HostState::read_only)?;
        if let HostState::Anonymous { .. } = state {
            return anonymous_notice(&state, &self.host);
        }
        super::states::host_notice(&state, &self.host, None).or_else(|| {
            state
                .notice(&self.host, None)
                .map(|(text, tone)| (vec![text], tone))
        })
    }

    /// The instance hides even reads from anonymous visitors: show its sign-in instructions
    /// in the full-pane empty state instead of an error dump.
    pub(super) fn sign_in_hints(&self) -> Option<(String, Vec<String>)> {
        let from_error = self
            .accounts
            .state
            .as_ref()
            .and_then(|(host, state)| match state {
                HostState::SignInRequired { hints, .. } => Some((host.clone(), hints.clone())),
                _ => None,
            });
        from_error.or_else(|| match self.host_state()? {
            HostState::SignInRequired { hints, .. } => Some((self.host.clone(), hints)),
            _ => None,
        })
    }

    /// Why `key` holds nothing and nothing is coming: the host refused the repository read, so
    /// no list read was sent. A headline and its hint replace the "Loading…" a pane would
    /// otherwise show forever under the notice row; `None` while the read is due or running.
    pub(super) fn blocked(&self, key: &Key) -> Option<(&'static str, &'static str)> {
        if self.cache.contains_key(key)
            || self.pending(key)
            || self.pending(&Key::Repository)
            || !matches!(self.cache.get(&Key::Repository), Some(Err(_)))
        {
            return None;
        }
        let state = self
            .accounts
            .state
            .as_ref()
            .map(|(_, state)| state.clone())
            .or_else(|| self.host_state());
        // The CLI by name when it isn't there to sign in with (`gh isn't installed`).
        let missing = |cli: crate::forge::transport::Cli| match cli {
            crate::forge::transport::Cli::Gh => (
                "gh isn't installed",
                "install it or sign in with a token (@)",
            ),
            crate::forge::transport::Cli::Glab => (
                "glab isn't installed",
                "install it or sign in with a token (@)",
            ),
            crate::forge::transport::Cli::Tea => (
                "tea isn't installed",
                "install it or sign in with a token (@)",
            ),
        };
        Some(match state {
            Some(HostState::SignedOut { cli, .. })
                if !super::states::installed(cli)
                    && !crate::forge::credential::refused(&self.host) =>
            {
                missing(cli)
            }
            Some(HostState::CliMissing { cli, .. }) => missing(cli),
            Some(HostState::SignedOut { .. } | HostState::SignInRequired { .. }) => {
                ("Sign-in required", "@ sign in")
            }
            Some(HostState::CliTooOld { cli, .. }) => match cli {
                crate::forge::transport::Cli::Gh => ("gh is too old", "@ accounts"),
                crate::forge::transport::Cli::Glab => ("glab is too old", "@ accounts"),
                crate::forge::transport::Cli::Tea => ("tea is too old", "@ accounts"),
            },
            Some(HostState::RateLimited { .. }) => ("Rate limited", "R retry once it resets"),
            Some(HostState::UnknownProvider) => ("Unknown site", "@ accounts"),
            Some(HostState::Unreachable { .. }) => ("Can't reach the forge", "R retry"),
            _ => ("Couldn't load this view", "R retry"),
        })
    }

    /// Keys while the popover is open (it is modal), and `@` to open it from any forge
    /// screen (`a` is assignees in a reader and attempts in a run).
    pub(super) fn accounts_key(&mut self, key: &KeyEvent) -> Option<Result<Option<Form>, String>> {
        if key.modifiers.control || key.modifiers.alt {
            return self.accounts.open.then_some(Ok(None));
        }
        if !self.accounts.open {
            if key.code == KeyCode::Char(super::states::SIGN_IN_KEY) {
                self.accounts.open = true;
                return Some(Ok(None));
            }
            return None;
        }
        match key.code {
            KeyCode::Escape | KeyCode::Char('q' | super::states::SIGN_IN_KEY) => {
                self.accounts.open = false
            }
            KeyCode::Char('r') => {
                if self.writing() {
                    self.accounts.open = false;
                    return Some(Err("Wait for the running change to finish first.".into()));
                }
                host_state::recheck();
                self.recheck_account();
            }
            _ => {}
        }
        Some(Ok(None))
    }
}

/// The notice over the list pane's state row, and the popover over everything but forms.
pub(super) fn paint(frame: &mut Frame, workspace: &Workspace, left: u16, width: u16, height: u16) {
    let showing_cached = workspace.cache.values().any(Result::is_ok);
    if left > 4
        && let Some((parts, tone)) = workspace.host_notice_parts(showing_cached)
    {
        let text = super::states::fit(&parts, usize::from(left - 4));
        // Pages 1–2: over the state/count row; page 3: over the first workflow row.
        let row = 3;
        let focused = workspace.focus == 0;
        for x in 1..left - 1 {
            frame.grid.set_cell(x, row, " ", surface(focused, None));
        }
        frame.grid.put_text_clipped(
            2,
            row,
            &ellipsize(&text, usize::from(left - 4)),
            style(tone),
            left - 2,
        );
    }
    if !workspace.accounts.open {
        return;
    }
    let mut lines = workspace.accounts.lines(&workspace.host);
    // How to sign in here, in this forge's own tool or page.
    let state = workspace.host_state();
    let ready = matches!(state, Some(HostState::Ready { .. }));
    let has_advice = state.as_ref().and_then(HostState::sign_in_advice).is_some()
        || workspace
            .accounts
            .state
            .as_ref()
            .is_some_and(|(host, state)| {
                host == &workspace.host && state.sign_in_advice().is_some()
            });
    if !ready && !has_advice && !workspace.host.is_empty() {
        let kind = workspace.forge_kind();
        let site = workspace.advertised_site_base();
        let sign_in = super::states::sign_in_how(kind, &workspace.host, site.as_deref());
        lines.push((String::new(), Tone::Muted));
        lines.push((format!("Sign in to {}", workspace.host), Tone::Normal));
        // Without the CLI, its login can't be the first way in: install it, or use a token.
        if let Some(cli) = super::states::missing_cli(kind) {
            lines.push((
                format!(
                    "  {cli} isn't installed · install it, then {}",
                    sign_in.trim_start_matches("run ")
                ),
                Tone::Muted,
            ));
        } else {
            lines.push((format!("  {sign_in}"), Tone::Muted));
        }
        lines.push((
            format!(
                "  or set {}",
                super::states::token_example(kind, &workspace.host)
            ),
            Tone::Muted,
        ));
    }
    lines.push((String::new(), Tone::Muted));
    lines.push(("r re-check · Esc close".into(), Tone::Muted));
    let widest = lines
        .iter()
        .map(|(text, _)| text.width())
        .max()
        .unwrap_or(0) as u16;
    let w = (widest + 4)
        .clamp(36, width.saturating_sub(4).max(36))
        .min(width);
    let lines = wrap_advice(lines, usize::from(w.saturating_sub(4)));
    let h = (lines.len() as u16 + 2).min(height.saturating_sub(4));
    let x = width.saturating_sub(w + 1);
    let y = 2;
    section(&mut frame.grid, x, y, w, h, "Accounts", true);
    for (index, (text, tone)) in lines
        .iter()
        .enumerate()
        .take(usize::from(h.saturating_sub(2)))
    {
        frame.grid.put_text_clipped(
            x + 2,
            y + 1 + index as u16,
            &ellipsize(text, usize::from(w.saturating_sub(4))),
            style(*tone),
            x + w - 2,
        );
    }
}

fn wrap_advice(lines: Vec<(String, Tone)>, width: usize) -> Vec<(String, Tone)> {
    let mut wrapped = Vec::new();
    for (line, tone) in lines {
        let text = line.trim_start();
        if crate::forge::credential::is_sign_in_advice(text)
            || crate::forge::credential::is_token_example(text)
        {
            wrapped.extend(
                super::prose::wrap_words(text, width.saturating_sub(2))
                    .into_iter()
                    .map(|part| (format!("  {}", part.trim_start()), tone)),
            );
        } else {
            wrapped.push((line, tone));
        }
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::super::{Key, Op, Request, Workspace};

    #[test]
    fn recheck_retires_same_host_reads_but_refuses_a_queued_write() {
        let mut workspace = Workspace::new(std::path::PathBuf::new());
        workspace.host = "forge.test:8443".into();
        workspace.repo = "o/r".into();
        workspace.account = crate::forge::transport::AccountNamespace::signed_in(
            crate::forge::transport::HostKey::new(&workspace.host),
            "alice",
        );
        let previous = (
            workspace.host.clone(),
            workspace.repo.clone(),
            workspace.scope_epoch,
        );
        workspace
            .drafts
            .insert("comment".into(), vec!["Alice's draft".into()]);
        let key = Key::Inbox(workspace.host.clone(), 1, "open".into(), "@me".into());
        let write = workspace.write_op(
            workspace.target(),
            crate::forge::request::Write::MarkAllRead,
        );
        workspace.queue.push_back(Request {
            key: Key::Write,
            op: write,
        });
        workspace.accounts.open = true;
        let recheck =
            crate::editor::input::KeyEvent::plain(crate::editor::input::KeyCode::Char('r'));
        assert!(matches!(workspace.accounts_key(&recheck), Some(Err(_))));
        assert!(workspace.accept_scope(&key, &previous));
        workspace.queue.clear();
        workspace.accounts.open = true;
        assert!(matches!(workspace.accounts_key(&recheck), Some(Ok(None))));
        assert!(!workspace.accept_scope(&key, &previous));
        let read = Read::Viewer;
        let old_account = crate::forge::transport::AccountNamespace::signed_in(
            crate::forge::transport::HostKey::new(&workspace.host),
            "alice",
        );
        workspace.active.answer_for_test(
            Request {
                key: key.clone(),
                op: Op::Read(workspace.target(), read.clone()),
            },
            previous,
            crate::forge::ForgeReply::Fetched(
                read,
                Some(crate::forge::transport::HostKey::new(&workspace.host)),
                Ok(crate::forge::provider::Fetched::payload(
                    crate::forge::request::Payload::Text("Alice".into()),
                )),
                old_account,
            ),
        );
        workspace.poll();
        assert!(workspace.value(&key).is_none());
        assert_eq!(
            workspace
                .account
                .as_ref()
                .and_then(crate::forge::transport::AccountNamespace::login),
            Some("alice")
        );
        assert_eq!(workspace.drafts["comment"], ["Alice's draft"]);
    }
    #[test]
    fn a_parked_comment_survives_a_same_login_recheck_but_not_a_switch() {
        use super::super::Action;
        use crate::forge::transport::{AccountNamespace, HostKey};
        let mut ws = Workspace::new(std::path::PathBuf::new());
        ws.host = "forge.test:8443".into();
        ws.repo = "o/r".into();
        ws.page = 1;
        // The comment belongs to the issue open in the reader.
        ws.opened = Some((1, 7));
        let host = HostKey::new(&ws.host);
        let alice = AccountNamespace::signed_in(host.clone(), "alice").unwrap();
        let bob = AccountNamespace::signed_in(host, "bob").unwrap();
        assert!(ws.accept_read_account(true, Some(alice.clone()), None, true));
        let action = Action::Comment(1, 7);
        ws.save_draft(&action, vec!["Keep this comment".into()]);
        ws.accounts.open = true;
        assert!(matches!(
            ws.accounts_key(&crate::editor::input::KeyEvent::character('r')),
            Some(Ok(None))
        ));
        assert!(ws.accept_read_account(true, Some(alice), None, true));
        let form = ws.activate(action.clone()).unwrap().unwrap();
        assert_eq!(form.fields[0].input.input, "Keep this comment");
        assert!(!ws.accept_read_account(true, Some(bob), None, true));
        assert!(
            ws.opened.is_none(),
            "the account switch clears the old reader"
        );
        // Reopen the issue as Bob; a cleared reader is not a valid comment target.
        ws.open_link(1, 7);
        let form = ws.activate(action).unwrap().unwrap();
        assert!(form.fields[0].input.input.is_empty());
    }

    use crate::forge::ForgeTarget;
    use crate::forge::host_state::HostState;
    use crate::forge::request::Read;
    use std::time::Instant;

    #[test]
    fn sign_in_advice_keeps_the_shortcut_and_wraps_without_commands() {
        use crate::forge::host_state::{Account, Tone};
        use crate::forge::transport::HostKey;

        let host = "git.accounts-fixture.test";
        let remedy = "Your saved sign-in and Dusty's API address don't match. Set api_base to https://other.test/tools/code/api/v1 in Dusty's forge settings.";
        let direct =
            "To use your environment token, switch to direct mode (DUSTY_FORGE_TRANSPORT=http).";
        let token = "GITEA_SERVER_TOKEN=<token> GITEA_SERVER_URL=https://git.accounts-fixture.test";
        let advice = format!(
            "{remedy}\nOr set both variables and use direct mode (DUSTY_FORGE_TRANSPORT=http): {token}\n{direct}"
        );
        let mut workspace = Workspace::new(std::path::PathBuf::new());
        workspace.host = host.into();
        let words = workspace.explain(&format!("Sign in to read logs · {advice}"));
        assert_eq!(words.headline, "Sign in to read logs");
        assert!(words.hint.starts_with("@ sign in"));
        assert!(words.hint.contains(token));
        workspace.accounts.state = Some((
            host.into(),
            HostState::Anonymous {
                remaining: Some(52),
                limit: Some(60),
                reset: None,
                hint: advice.clone(),
            },
        ));
        let (parts, _) = workspace.host_notice_parts(false).unwrap();
        let notice = super::super::states::fit(&parts, 32);
        assert!(notice.contains("@ sign in"), "{notice}");
        assert!(!notice.contains("api_base"), "{notice}");

        let state = HostState::SignInRequired {
            provider: None,
            hints: vec![
                advice,
                "tea login add --url https://git.example.test".into(),
            ],
        };
        workspace.accounts.state = Some((host.into(), state.clone()));
        workspace.accounts.open = true;
        let (width, height) = (53, 28);
        let mut frame = crate::ui::Frame {
            grid: crate::ui::CellGrid::new(width, height),
            cursor: None,
        };
        super::paint(&mut frame, &workspace, 35, width, height);
        let popover = (0..height)
            .map(|y| frame.grid.row_text(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(popover.contains("Accounts"));
        assert!(popover.contains("https://other.test/tools/code/api/v1"));
        assert!(popover.contains("GITEA_SERVER_TOKEN=<token>"));
        assert!(!popover.contains("$ ") && !popover.contains("  run tea"));
        let rows = super::super::paint::record_rows(&workspace, 45, workspace.icons);
        let rendered = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Set api_base"), "{rendered}");
        assert!(
            rendered.contains("\nTo use your environment token"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("$ ") && !rendered.contains("tea login add"),
            "{rendered}"
        );

        let account = Account::new(HostKey::new(host), None, state);
        let lines = super::wrap_advice(account.lines(), 45);
        assert!(lines.iter().any(
            |(line, tone)| line.starts_with("  To use your environment token")
                && *tone == Tone::Muted
        ));
        assert!(lines.iter().any(|(line, _)| line.contains("api/v1")));
        assert!(rendered.contains("GITEA_SERVER_TOKEN=<token>"));
        assert!(
            lines
                .iter()
                .any(|(line, _)| line.contains("GITEA_SERVER_TOKEN=<token>"))
        );
    }

    #[test]
    fn enterprise_token_instructions_keep_the_selector_at_narrow_widths() {
        use crate::forge::host_state::Tone;
        use crate::forge::provider::ProviderKind;
        use unicode_width::UnicodeWidthStr;

        let host = "git.example.test";
        let kind = ProviderKind::GitHub;
        let example = super::super::states::token_example(Some(kind), host);
        let mut workspace = Workspace::new(std::path::PathBuf::new());
        workspace.accounts.state = Some((
            host.into(),
            HostState::SignInRequired {
                provider: Some(kind),
                hints: crate::forge::credential::sign_in_hints(
                    kind,
                    &crate::forge::transport::HostKey::new(host),
                ),
            },
        ));
        let rows = super::super::paint::record_rows(&workspace, 34, workspace.icons);
        let popover = super::wrap_advice(vec![(format!("  or set {example}"), Tone::Muted)], 34);
        for assignment in ["GH_ENTERPRISE_TOKEN=<token>", "GH_HOST=git.example.test"] {
            assert!(
                rows.iter()
                    .any(|row| row.text.contains(assignment) && row.text.width() <= 34)
            );
            assert!(
                popover
                    .iter()
                    .any(|(line, _)| line.contains(assignment) && line.width() <= 34)
            );
        }
    }

    #[test]
    fn a_refused_repository_read_turns_the_lists_loading_into_the_hosts_state() {
        let mut workspace = Workspace::new(std::path::PathBuf::new());
        let list = Key::List(1, "open".into(), String::new());
        // Nothing asked yet: still loading.
        assert_eq!(workspace.blocked(&list), None);
        workspace
            .cache
            .insert(Key::Repository, Err("Not signed in to github.com".into()));
        workspace.accounts.state = Some((
            "github.com".into(),
            HostState::RateLimited {
                until: Instant::now(),
            },
        ));
        assert_eq!(
            workspace.blocked(&list).map(|(headline, _)| headline),
            Some("Rate limited")
        );
        // A retry in flight is loading again.
        workspace.queue.push_back(Request {
            key: Key::Repository,
            op: Op::Read(ForgeTarget::remote(), Read::Repository),
        });
        assert_eq!(workspace.blocked(&list), None);
    }
}
