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

const HISTORY_FILE: &str = "boreal.history";
const CAPACITY: usize = 2000;
const MAX_SELECTOR_BYTES: usize = 255;
const MAX_ENTRY_BYTES: usize = 1100;
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
    history.sync()?;
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
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
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
    // With --id, even the first positional word after the worker is payload.
    // Detect that form before looking at any positional words, even if an
    // earlier ambiguous option will stop us before we reach it.
    let explicit_id = run
        && args
            .iter()
            .take_while(|word| *word != "--")
            .any(|word| word == "--id" || word.starts_with("--id="));
    let mut worker = None;
    let mut owner = None;
    let mut workflow = None;
    let mut id = None;
    let mut allow_legacy_id = !explicit_id;
    let mut args = args.iter().map(String::as_str);
    while let Some(word) = args.next() {
        if word == "--" {
            break;
        }
        let (flag, attached) = word
            .split_once('=')
            .map_or((word, None), |(flag, value)| (flag, Some(value)));
        match flag {
            "--owner" if owner.is_none() => owner = Some(name(attached.or_else(|| args.next())?)?),
            "--workflow" if run && workflow.is_none() => {
                workflow = Some(name(attached.or_else(|| args.next())?)?);
            }
            "--id" if run && id.is_none() => {
                id = Some(workflow_id(attached.or_else(|| args.next())?)?);
            }
            "--input-json" if run && worker.is_some() => {
                allow_legacy_id = false;
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
            _ if run && worker.is_none() => worker = Some(name(word)?),
            _ if (run && allow_legacy_id && id.is_none()) || (required_id && id.is_none()) => {
                id = Some(workflow_id(word)?);
            }
            _ if run => {
                // Ignore all positional payloads. Treat a possibly unquoted,
                // split JSON argument as ambiguous rather than scanning it
                // for more options.
                if word.starts_with(['{', '[']) && input::parse(word).is_err() {
                    break;
                }
            }
            _ => return None,
        }
    }

    let mut entry = cmd.to_owned();
    if run {
        entry.push(' ');
        entry.push_str(worker?);
        if let Some(id) = id {
            entry.push_str(" --id ");
            entry.push_str(id);
        }
        if let Some(workflow) = workflow {
            entry.push_str(" --workflow ");
            entry.push_str(workflow);
        }
    } else if required_id {
        entry.push(' ');
        entry.push_str(id?);
    }
    if let Some(owner) = owner {
        entry.push_str(" --owner ");
        entry.push_str(owner);
    }
    Some(entry)
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
        // Only our canonical format is eligible for recall. Do not copy raw
        // noncanonical text or a partial final write into history.
        if canonical(line).as_deref() != Some(line) {
            dirty = true;
            continue;
        }
        dirty |= entries.len() == CAPACITY;
        dirty |= !push_entry(&mut entries, line.to_owned());
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
}

impl History for SelectorHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        let Some(command) = canonical(&item.command_line) else {
            return Ok(HistoryItem::from_command_line(""));
        };
        let saved = self.inner.save(HistoryItem::from_command_line(command))?;
        if self.disk.is_some() && saved.id.is_some() {
            push_entry(&mut self.pending, saved.command_line.clone());
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
        let Some(disk) = &self.disk else {
            return Ok(());
        };
        let mut file = disk.lock()?;
        let (mut entries, mut dirty) = read_entries(&mut file)?;
        // Merge only new local entries, not the snapshot taken at startup.
        for entry in &self.pending {
            dirty |= push_entry(&mut entries, entry.clone());
        }
        if dirty {
            write_entries(&mut file, &entries)?;
        }
        drop(file);

        self.pending.clear();
        let mut inner = FileBackedHistory::new(CAPACITY).expect("fixed history capacity");
        for entry in entries {
            inner
                .save(HistoryItem::from_command_line(entry))
                .expect("in-memory history accepts canonical selectors");
        }
        self.inner = inner;
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
            (
                "run greet first private-value --workflow greeting --owner bob",
                Some("run greet --id first --workflow greeting --owner bob"),
            ),
            (
                "run greet private-value --id=first -- --owner leaked",
                Some("run greet --id first"),
            ),
            (
                r#"run greet --input-json '{"password":"private-value"}' --owner=bob"#,
                Some("run greet --owner bob"),
            ),
            (
                "run greet --id first --input-json { --owner leaked }",
                Some("run greet --id first"),
            ),
            (
                "run greet --unknown private-value --id leaked",
                Some("run greet"),
            ),
            ("run greet --id 'not a simple selector'", None),
            ("run greet --id 'unfinished", None),
            ("run greet --id first\n--owner leaked", None),
            (
                "status --owner=bob 'run-1'",
                Some("status run-1 --owner bob"),
            ),
            ("permissions bob", Some("permissions bob")),
            ("secrets set api_key", Some("secrets set api_key")),
            ("secrets set api_key private-value", None),
            ("unknown private-value", None),
        ];
        let mut history = memory();
        for (line, expected) in cases {
            assert_eq!(canonical(line).as_deref(), expected, "{line}");
            let saved = history.save(HistoryItem::from_command_line(line)).unwrap();
            assert_eq!(saved.command_line, expected.unwrap_or_default());
        }
        assert!(
            history
                .search(SearchQuery::all_that_contain_rev("private-value".into()))
                .unwrap()
                .is_empty()
        );
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
        let mut first = open(&home.0, uid).unwrap();
        let mut second = open(&home.0, uid).unwrap();
        for i in 0..CAPACITY {
            first
                .save(HistoryItem::from_command_line(format!(
                    "run greet --id old-{i} private-value"
                )))
                .unwrap();
        }
        second
            .save(HistoryItem::from_command_line("status new --owner bob"))
            .unwrap();
        drop(first);
        drop(second);

        let reopened = open(&home.0, uid).unwrap();
        assert_eq!(reopened.count_all().unwrap(), CAPACITY as i64);
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("run greet --id old-1\n"));
        assert!(contents.ends_with("status new --owner bob\n"));
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
