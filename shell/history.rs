use std::collections::VecDeque;
use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use boreal_common::{catalog, input, os::O_NOFOLLOW, secret};
use reedline::{
    FileBackedHistory, History, HistoryItem, HistoryItemId, HistorySessionId, SearchQuery,
};

use crate::command;

const HISTORY_FILE: &str = "boreal.history";
const CAPACITY: usize = 2000;
const MAX_SELECTOR_BYTES: usize = 255;
const MAX_ENTRY_BYTES: usize = 1100;
const MAX_RECALL_BYTES: usize = 16 * 1024;
const MAX_FILE_BYTES: usize = CAPACITY * (MAX_ENTRY_BYTES + 1);
const FILE_MODE: u32 = 0o600;
// Linux (both Boreal architectures); don't block opening an unexpected special file.
const O_NONBLOCK: i32 = 0o4000;

pub fn open(home: &Path, uid: u32) -> io::Result<Box<dyn History>> {
    let home = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(home)?;
    let meta = home.metadata()?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o022 != 0 {
        return Err(unsafe_path());
    }
    let mut history = SelectorHistory::new(Some(Disk { home, uid }));
    for entry in history.sync_disk()? {
        history
            .inner
            .save(HistoryItem::from_command_line(entry))
            .expect("in-memory history accepts canonical selectors");
    }
    Ok(Box::new(history))
}

pub fn memory() -> Box<dyn History> {
    Box::new(SelectorHistory::new(None))
}

fn name(value: &str) -> Option<&str> {
    (value.len() <= MAX_SELECTOR_BYTES
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')))
    .then_some(value)
}

fn workflow_id(value: &str) -> Option<&str> {
    (value.len() <= MAX_SELECTOR_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
        && catalog::validate_workflow_id(value).is_ok())
    .then_some(value)
}

fn canonical(line: &str) -> Option<String> {
    if line.len() > input::MAX_BYTES + MAX_ENTRY_BYTES
        || line.bytes().any(|b| b.is_ascii_control() && b != b'\t')
    {
        return None;
    }
    let words = shlex::split(line)?;
    let (cmd, args) = words.split_first()?;
    let cmd = cmd.as_str();
    match (cmd, args) {
        ("workers" | "permissions", []) => return Some(cmd.to_owned()),
        ("permissions", [user]) => return Some(format!("permissions {}", name(user)?)),
        ("secrets", [action]) if action == "list" => return Some("secrets list".into()),
        ("secrets", [action, key]) if matches!(action.as_str(), "set" | "remove") => {
            secret::validate_name(key).ok()?;
            return Some(format!("secrets {action} {key}"));
        }
        ("run" | "list" | "status" | "result" | "view" | "cancel", _) => {}
        _ => return None,
    }

    let run = cmd == "run";
    let required_id = matches!(cmd, "status" | "result" | "view" | "cancel");
    let mut selector = None;
    let mut workflow = None;
    let mut id = None;
    let mut args = args.iter().map(String::as_str);
    while let Some(word) = args.next() {
        if word == "--" {
            break;
        }
        let (flag, attached) = word
            .split_once('=')
            .map_or((word, None), |(flag, value)| (flag, Some(value)));
        match flag {
            "--no-ask" if attached.is_none() => {}
            "--workflow" if run && workflow.is_none() => {
                workflow = Some(name(attached.or_else(|| args.next())?)?);
            }
            "--id" if run && id.is_none() => {
                id = Some(workflow_id(attached.or_else(|| args.next())?)?);
            }
            "--input-json" if run => {
                // Continue only if the value is one complete JSON word (or the
                // stdin marker). Otherwise later words could be pieces of it.
                let Some(value) = attached.or_else(|| args.next()) else {
                    break;
                };
                if value != "-" && input::parse(value).is_err() {
                    break;
                }
            }
            "--owner" | "--workflow" | "--id" | "--input-json" => return None,
            _ if word.starts_with('-') => break,
            _ if selector.is_none() => {
                if run || required_id {
                    let (target, owner) = command::split_selector(word).ok()?;
                    if run {
                        name(target)?;
                    } else {
                        workflow_id(target)?;
                    }
                    if let Some(owner) = owner {
                        name(owner)?;
                    }
                } else {
                    name(word)?;
                }
                selector = Some(word);
            }
            _ if run => break,
            _ => return None,
        }
    }

    let mut entry = cmd.to_owned();
    if let Some(selector) = selector {
        entry.push(' ');
        entry.push_str(selector);
    }
    if run {
        if let Some(id) = id {
            entry.push_str(" --id ");
            entry.push_str(id);
        }
        if let Some(workflow) = workflow {
            entry.push_str(" --workflow ");
            entry.push_str(workflow);
        }
    }
    Some(entry)
}

fn migrate_owner(line: &str) -> Option<String> {
    let (base, owner) = line.rsplit_once(" --owner ")?;
    let owner = name(owner)?;
    if canonical(base).as_deref() != Some(base) {
        return None;
    }
    if base == "list" {
        return Some(format!("list {owner}"));
    }
    let (cmd, args) = base.split_once(' ')?;
    let (target, tail) = match cmd {
        "run" => args
            .split_once(' ')
            .map_or((args, ""), |(word, _)| (word, &args[word.len()..])),
        "status" | "result" | "view" | "cancel" => (args, ""),
        _ => return None,
    };
    if target.starts_with('-') || target.contains('/') {
        return None;
    }
    Some(format!("{cmd} {target}/{owner}{tail}"))
}

fn unsafe_path() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "unsafe history path")
}

fn oversized() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "history exceeds size limit")
}

struct Disk {
    home: File,
    uid: u32,
}

impl Disk {
    fn check(&self, meta: &Metadata) -> io::Result<()> {
        if !meta.is_file()
            || meta.uid() != self.uid
            || meta.mode() & 0o7777 != FILE_MODE
            || meta.nlink() != 1
        {
            return Err(unsafe_path());
        }
        Ok(())
    }

    fn lock(&self) -> io::Result<File> {
        let home = self.home.metadata()?;
        if home.uid() != self.uid || home.mode() & 0o022 != 0 {
            return Err(unsafe_path());
        }
        // Resolve relative to the verified, open directory rather than looking
        // up the home pathname again. Dropping the returned file releases its lock.
        let path = format!("/proc/self/fd/{}/{HISTORY_FILE}", self.home.as_raw_fd());
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(FILE_MODE)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK);
        let file = match options.clone().create_new(true).open(&path) {
            Ok(file) => {
                // Creation never grants more than 0600, regardless of umask.
                if file.metadata()?.mode() & 0o7777 != FILE_MODE {
                    file.set_permissions(Permissions::from_mode(FILE_MODE))?;
                }
                file
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                self.check(&fs::symlink_metadata(&path)?)?;
                options.open(&path)?
            }
            Err(err) => return Err(err),
        };

        self.check(&file.metadata()?)?;
        file.lock()?;
        let meta = file.metadata()?;
        self.check(&meta)?;
        let current = fs::symlink_metadata(path)?;
        if (meta.dev(), meta.ino()) != (current.dev(), current.ino()) {
            return Err(unsafe_path());
        }
        Ok(file)
    }
}

fn push_entry(entries: &mut VecDeque<String>, entry: String) -> bool {
    if entries.back() == Some(&entry) {
        return false;
    }
    if entries.len() == CAPACITY {
        entries.pop_front();
    }
    entries.push_back(entry);
    true
}

fn read_entries(file: &mut File) -> io::Result<(VecDeque<String>, bool)> {
    let size = file.metadata()?.len();
    if size > MAX_FILE_BYTES as u64 {
        return Err(oversized());
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(oversized());
    }

    let mut entries = VecDeque::new();
    let mut dirty = false;
    for raw in bytes.split_inclusive(|b| *b == b'\n') {
        let Some(line) = raw
            .strip_suffix(b"\n")
            .filter(|line| line.len() <= MAX_ENTRY_BYTES)
            .and_then(|line| std::str::from_utf8(line).ok())
        else {
            dirty = true;
            continue;
        };
        // Never recall raw noncanonical text or a partial final write.
        let entry = if canonical(line).as_deref() == Some(line) {
            line.to_owned()
        } else if let Some(migrated) = migrate_owner(line) {
            dirty = true;
            migrated
        } else {
            dirty = true;
            continue;
        };
        dirty |= entries.len() == CAPACITY;
        dirty |= !push_entry(&mut entries, entry);
    }
    Ok((entries, dirty))
}

fn write_entries(file: &mut File, entries: &VecDeque<String>) -> io::Result<()> {
    let mut bytes = Vec::new();
    for entry in entries {
        if entry.len() > MAX_ENTRY_BYTES || bytes.len() + entry.len() + 1 > MAX_FILE_BYTES {
            return Err(oversized());
        }
        bytes.extend_from_slice(entry.as_bytes());
        bytes.push(b'\n');
    }
    // Preserve the locked inode so overlapping sessions lock the same file.
    file.rewind()?;
    file.write_all(&bytes)?;
    file.set_len(bytes.len() as u64)
}

struct SelectorHistory {
    inner: FileBackedHistory,
    pending: VecDeque<String>,
    disk: Option<Disk>,
}

impl SelectorHistory {
    fn new(disk: Option<Disk>) -> Self {
        Self {
            inner: FileBackedHistory::new(CAPACITY).expect("fixed history capacity"),
            pending: VecDeque::new(),
            disk,
        }
    }

    fn sync_disk(&mut self) -> io::Result<VecDeque<String>> {
        let Some(disk) = &self.disk else {
            return Ok(VecDeque::new());
        };
        let mut file = disk.lock()?;
        let (mut entries, mut dirty) = read_entries(&mut file)?;
        // Merge only new local selectors, not the snapshot taken at startup.
        for entry in &self.pending {
            dirty |= push_entry(&mut entries, entry.clone());
        }
        if dirty {
            write_entries(&mut file, &entries)?;
        }
        self.pending.clear();
        Ok(entries)
    }
}

impl History for SelectorHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        let Some(selector) = canonical(&item.command_line) else {
            return Ok(HistoryItem::from_command_line(""));
        };
        // The editor never writes to disk. Preserve the entered run line for
        // recall, but cap the per-entry memory cost in a multi-user lab.
        let recall = if (selector == "run" || selector.starts_with("run "))
            && item.command_line.len() <= MAX_RECALL_BYTES
        {
            item.command_line
        } else {
            selector.clone()
        };
        let saved = self.inner.save(HistoryItem::from_command_line(recall))?;
        if self.disk.is_some() && saved.id.is_some() {
            push_entry(&mut self.pending, selector);
        }
        Ok(saved)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        if let Some(disk) = &self.disk {
            disk.lock()?.set_len(0)?;
        }
        self.pending.clear();
        self.inner.clear()
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(id)
    }

    fn sync(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.sync_disk()?;
        }
        Ok(())
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

impl Drop for SelectorHistory {
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let _ = self.sync();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, symlink};

    #[test]
    fn history_keeps_only_selectors() {
        let cases = [
            ("run", Some("run")),
            ("status", Some("status")),
            (
                "run greet/bob --id first --workflow greeting",
                Some("run greet/bob --id first --workflow greeting"),
            ),
            (
                "run greet --id=first -- ignored/leaked",
                Some("run greet --id first"),
            ),
            (
                r#"run --input-json '{"password":"private-value"}' greet/bob"#,
                Some("run greet/bob"),
            ),
            (
                "run greet --id first --input-json { extra/leaked }",
                Some("run greet --id first"),
            ),
            (
                "run greet --unknown private-value --id leaked",
                Some("run greet"),
            ),
            ("run greet --id 'not a simple selector'", None),
            ("run greet --id 'unfinished", None),
            ("run greet --id first\nextra/leaked", None),
            ("status 'suite/run-1/bob'", Some("status suite/run-1/bob")),
            ("status run-1/", None),
            ("list bob", Some("list bob")),
            ("permissions bob", Some("permissions bob")),
            ("secrets set api_key", Some("secrets set api_key")),
            ("secrets set api_key private-value", None),
            ("unknown private-value", None),
        ];
        for (line, expected) in cases {
            assert_eq!(canonical(line).as_deref(), expected, "{line}");
        }
        let mut history = memory();
        let oversized = format!("run greet --input-json '{}'", " ".repeat(MAX_RECALL_BYTES));
        for (line, recalled) in [
            ("secrets set api_key private-value", ""),
            ("secrets set api_key", "secrets set api_key"),
            (oversized.as_str(), "run greet"),
        ] {
            let saved = history.save(HistoryItem::from_command_line(line)).unwrap();
            assert_eq!(saved.command_line, recalled);
        }
    }

    #[test]
    fn test_migrates_only_canonical_owner_history() {
        for (old, new) in [
            (
                "run greet --id old --owner bob",
                Some("run greet/bob --id old"),
            ),
            ("status old --owner bob", Some("status old/bob")),
            ("list --owner bob", Some("list bob")),
            ("run greet --input-json private --owner bob", None),
            ("status old/alice --owner bob", None),
            ("status old --owner 'bob'", None),
        ] {
            assert_eq!(migrate_owner(old).as_deref(), new, "{old}");
        }
    }

    #[test]
    fn history_merges_sessions_caps_entries_and_rejects_unsafe_files() {
        struct Home(std::path::PathBuf);
        impl Drop for Home {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let home = Home(std::env::temp_dir().join(format!(
            "boreal-history-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        fs::DirBuilder::new().mode(0o700).create(&home.0).unwrap();
        let path = home.0.join(HISTORY_FILE);
        let uid = boreal_common::os::current_euid();
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(FILE_MODE)
            .open(&path)
            .unwrap()
            .write_all(b"status legacy --owner bob\n")
            .unwrap();
        let mut first = open(&home.0, uid).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "status legacy/bob\n");
        let mut second = open(&home.0, uid).unwrap();
        for i in 0..CAPACITY {
            first
                .save(HistoryItem::from_command_line(format!(
                    r#"run greet --id old-{i} --input-json '{{"name":"private-value"}}'"#
                )))
                .unwrap();
        }
        first.sync().unwrap();
        let last = format!(
            r#"run greet --id old-{} --input-json '{{"name":"private-value"}}'"#,
            CAPACITY - 1
        );
        assert_eq!(
            first
                .search(SearchQuery::all_that_contain_rev("private-value".into()))
                .unwrap()[0]
                .command_line,
            last
        );
        second
            .save(HistoryItem::from_command_line("status new/bob"))
            .unwrap();
        drop(first);
        drop(second);

        let reopened = open(&home.0, uid).unwrap();
        assert_eq!(reopened.count_all().unwrap(), CAPACITY as i64);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("run greet --id old-1\n"));
        assert!(contents.ends_with("status new/bob\n"));
        assert!(!contents.contains("private-value"));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        drop(reopened);

        let other = home.0.join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(open(&home.0, uid).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&other, &path).unwrap();
        assert!(open(&home.0, uid).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(&other, &path).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
        assert!(open(&home.0, uid).is_err());
    }
}
