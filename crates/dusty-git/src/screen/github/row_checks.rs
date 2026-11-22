//! The check mark on Gitea's and Forgejo's pull request rows (r15 gitea-forgejo): their list
//! API carries no CI state, so once the list is painted the rows on screen are asked for their
//! head's combined status, lazily, on the warm worker (one batch, at most
//! `provider::parallel::WIDTH` requests at once). A known state stays on its row while it is
//! asked again (stale-while-revalidate): after `TTL`, or as soon as the row's `updatedAt`
//! changes (a push or an edit). A row the forge did not answer for is not asked again before
//! `TTL` either, so a failing forge is not hammered.
use super::*;
use crate::forge::model::CiState;
use crate::forge::provider::ProviderKind;
use crate::forge::records::ItemRecord;
use crate::forge::request::Payload;
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// How long a row's mark is trusted before the forge is asked again.
const TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(super) struct Rows {
    /// Pull request numbers on screen at the last paint of the list.
    visible: RefCell<Vec<u64>>,
    by_repository: HashMap<String, RepositoryRows>,
}

#[derive(Default)]
struct RepositoryRows {
    /// Per number: the state (None: no checks), the row's `updatedAt` when asked, and when.
    known: HashMap<u64, (Option<CiState>, String, Instant)>,
    /// Per number: the `updatedAt` it was last asked with, and when.
    asked: HashMap<u64, (String, Instant)>,
}

impl Rows {
    /// The list's paint: these rows are on screen.
    pub(super) fn show(&self, numbers: Vec<u64>) {
        *self.visible.borrow_mut() = numbers;
    }

    /// The row's checks with what the forge said, when the list row carries none itself.
    pub(super) fn overlay(&self, repo: &str, record: &mut ItemRecord) {
        if record.checks.total() > 0 {
            return;
        }
        if let Some((Some(state), ..)) = self
            .by_repository
            .get(repo)
            .and_then(|rows| rows.known.get(&record.number))
        {
            record.checks.add(*state);
        }
    }
}

impl Workspace {
    pub(super) fn checkout_pr_list_repository(&self) -> Option<&str> {
        (self.page == 2 && !self.inbox.enabled).then(|| self.list_repository())
    }

    fn row_checks_wanted(&self) -> bool {
        self.checkout_pr_list_repository().is_some()
            && matches!(
                self.forge_kind(),
                Some(ProviderKind::Gitea | ProviderKind::Forgejo)
            )
    }

    /// Take the warm worker's answers and ask for the visible rows still unknown or stale.
    pub(super) fn poll_row_checks(&mut self) -> bool {
        // The disk and warm reply guards have already checked the list repository.
        let repo = self.list_repository().to_owned();
        let mut changed = false;
        let answered = self
            .cache
            .keys()
            .filter(|key| matches!(key, Key::RowChecks(_)))
            .cloned()
            .collect::<Vec<_>>();
        for key in answered {
            // From disk (a restart): shown, and asked again as if its budget ran out.
            let verified = self
                .fresh
                .meta
                .remove(&key)
                .is_some_and(|meta| meta.verified);
            let seen = if verified {
                Instant::now()
            } else {
                Instant::now().checked_sub(TTL).unwrap_or_else(Instant::now)
            };
            let Some(Ok(reply)) = self.cache.remove(&key) else {
                continue;
            };
            let Key::RowChecks(numbers) = &key else {
                continue;
            };
            let Ok(Payload::Fingerprints(prints)) = serde_json::from_value::<Payload>(reply.value)
            else {
                continue;
            };
            let rows = self
                .fresh
                .rows
                .by_repository
                .entry(repo.clone())
                .or_default();
            for number in numbers {
                let updated = rows
                    .asked
                    .get(number)
                    .map(|(updated, _)| updated.clone())
                    .unwrap_or_default();
                let state = prints
                    .iter()
                    .find(|(each, _)| each == number)
                    .and_then(|(_, print)| print.rollup);
                if rows.known.get(number).map(|(known, ..)| *known) != Some(state) {
                    changed = true;
                }
                rows.known.insert(*number, (state, updated, seen));
            }
        }
        if !self.row_checks_wanted() || self.warm_has(|key| matches!(key, Key::RowChecks(_))) {
            return changed;
        }
        let updated = self
            .items()
            .iter()
            .map(|item| (number(item, "number"), string(item, "updatedAt").to_owned()))
            .collect::<HashMap<_, _>>();
        let rows = &self.fresh.rows;
        let visible = rows.visible.borrow().clone();
        let saved = rows.by_repository.get(&repo);
        let ask = visible
            .into_iter()
            .filter(|number| {
                let Some(now) = updated.get(number) else {
                    return false;
                };
                let fresh = |seen: &String, at: &Instant| seen == now && at.elapsed() < TTL;
                let known = saved
                    .and_then(|rows| rows.known.get(number))
                    .is_some_and(|(_, seen, at)| fresh(seen, at));
                let asked = saved
                    .and_then(|rows| rows.asked.get(number))
                    .is_some_and(|(seen, at)| fresh(seen, at));
                !known && !asked
            })
            .collect::<Vec<_>>();
        if ask.is_empty() {
            return changed;
        }
        let rows = self.fresh.rows.by_repository.entry(repo).or_default();
        for number in &ask {
            rows.asked
                .insert(*number, (updated[number].clone(), Instant::now()));
        }
        let key = Key::RowChecks(ask.clone());
        // These numbers come from the list, even when its reader shows a foreign link.
        let op = Op::Read(self.list_target(), Read::Fingerprints(ask));
        // A restart paints the marks the last session saw while the forge is asked.
        changed |= self.load_disk(&key, &op);
        let request = self.request(key, op);
        self.fresh.push_warm(request);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_known_state_marks_a_row_without_checks_only() {
        let mut rows = Rows::default();
        let repo = "alpha/one";
        rows.by_repository
            .entry(repo.into())
            .or_default()
            .known
            .insert(7, (Some(CiState::Failed), String::new(), Instant::now()));
        let row = json!({ "number": 7 });
        let mut bare = ItemRecord::from_row(&row);
        rows.overlay(repo, &mut bare);
        assert_eq!(bare.ci(), Some(CiState::Failed));
        let mut other_repo = ItemRecord::from_row(&row);
        rows.overlay("beta/two", &mut other_repo);
        assert_eq!(other_repo.ci(), None);
        let mut own = ItemRecord::from_row(&json!({
            "number": 7,
            "statusCheckRollup": [{ "__typename": "StatusContext", "state": "SUCCESS" }],
        }));
        let before = own.ci();
        rows.overlay(repo, &mut own);
        assert_eq!(own.ci(), before);
        let mut other = ItemRecord::from_row(&json!({ "number": 8 }));
        rows.overlay(repo, &mut other);
        assert_eq!(other.ci(), None);
    }
}
