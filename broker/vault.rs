use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use boreal_common::{catalog::Catalog, os};

type Grants = BTreeMap<String, BTreeSet<String>>;
const SUFFIX: &str = "-vault";

pub fn reserved_queue(queue: &str) -> bool {
    queue.ends_with(SUFFIX)
}

#[derive(Default)]
pub struct Policy {
    owners: BTreeMap<String, Grants>,
}

impl Policy {
    pub fn load(path: &Path, catalog: &Catalog) -> Result<Self, String> {
        let bytes =
            os::read_owned_file(path, Some(0), 0o222, 256 * 1024).map_err(|err| err.detail())?;
        let entries = serde_json::from_slice::<BTreeMap<String, Grants>>(&bytes)
            .map_err(|err| format!("invalid vault policy: {err}"))?;
        Self::new(entries, catalog)
    }

    pub fn new(entries: BTreeMap<String, Grants>, catalog: &Catalog) -> Result<Self, String> {
        let regular: BTreeSet<_> = catalog
            .users()
            .iter()
            .flat_map(|(owner, user)| {
                user.workers()
                    .keys()
                    .map(move |name| format!("{owner}-{name}"))
            })
            .collect();
        for (owner, grants) in &entries {
            let user = catalog.require_user(owner).map_err(|err| err.detail())?;
            let queue = format!("{owner}-vault");
            if regular.contains(&queue) || grants.is_empty() {
                return Err(format!(
                    "vault {queue} collides with a worker or has no grants"
                ));
            }
            for (worker, operations) in grants {
                if !user.workers().contains_key(worker)
                    || operations.is_empty()
                    || !operations.iter().all(|op| valid_operation(op))
                {
                    return Err(format!("invalid vault grant for {owner}/{worker}"));
                }
            }
        }
        Ok(Self { owners: entries })
    }

    pub fn owners(&self) -> impl Iterator<Item = &str> {
        self.owners.keys().map(String::as_str)
    }

    pub fn contains(&self, queue: &str) -> bool {
        queue
            .strip_suffix(SUFFIX)
            .is_some_and(|owner| self.has_vault(owner))
    }

    pub fn has_vault(&self, owner: &str) -> bool {
        self.owners.contains_key(owner)
    }

    pub fn allows(&self, owner: &str, from: &str, to: &str, operation: &str) -> bool {
        to.strip_suffix(SUFFIX) == Some(owner)
            && from
                .strip_prefix(owner)
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|worker| self.owners.get(owner)?.get(worker))
                .is_some_and(|operations| operations.contains(operation))
    }
}

fn valid_operation(op: &str) -> bool {
    !op.is_empty()
        && op.len() <= 128
        && op
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':'))
}
