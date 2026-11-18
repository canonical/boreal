use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::input::{self, Schema, json};
use crate::os;
use crate::wire::{PermissionReport, WorkerEntry, WorkerPermissionLine};
use crate::yaml::{self, Value};

pub const DEFAULT_ETC_DIR: &str = "/etc/boreal";
const MAX_YAML_BYTES: usize = 512 * 1024;
const MAX_VERSION_BYTES: usize = 256;
const DEFAULT_TEMPORAL_MANIFEST: &str = include_str!("../templates/default/temporal/Cargo.toml");

pub fn parse_cargo_version(manifest: &str) -> Option<&str> {
    let mut in_version_section = false;
    for raw_line in manifest.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let section = header.trim();
            in_version_section = matches!(section, "workspace.package" | "package");
            continue;
        }
        if !in_version_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("version") {
            let trimmed = rest.trim_start();
            let Some(after_eq) = trimmed.strip_prefix('=') else {
                continue;
            };
            let Some(quoted) = after_eq.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((ver, trailing)) = quoted.split_once('"') else {
                continue;
            };
            if trailing.trim().is_empty()
                && !ver.is_empty()
                && ver.chars().all(|c| !c.is_control() && !c.is_whitespace())
            {
                return Some(ver);
            }
        }
    }
    None
}

pub fn default_lab_version() -> &'static str {
    parse_cargo_version(DEFAULT_TEMPORAL_MANIFEST).unwrap_or("0.1.0")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Scalar {
    fn value(&self) -> input::Value {
        match self {
            Self::Str(s) => json!(s),
            Self::Int(n) => json!(n),
            Self::Float(n) => json!(n),
            Self::Bool(b) => json!(b),
        }
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Self::Str(s) => write_json_str(out, s),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Float(n) => {
                if n.fract() == 0.0 {
                    let _ = write!(out, "{n:.1}");
                } else {
                    let _ = write!(out, "{n}");
                }
            }
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        }
    }

    pub fn display_text(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Int(n) => n.to_string(),
            Self::Float(n) => n.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

pub fn write_json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[derive(Clone, Debug, PartialEq)]
pub enum ArgKind {
    String,
    Int,
    Float,
    Bool,
    Enum { choices: Vec<String> },
}

impl ArgKind {
    pub fn type_label(&self) -> String {
        match self {
            Self::String => "string".into(),
            Self::Int => "int".into(),
            Self::Float => "float".into(),
            Self::Bool => "bool".into(),
            Self::Enum { choices } => format!("enum({})", choices.join("|")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arg {
    name: String,
    kind: ArgKind,
    description: String,
    default: Option<Scalar>,
}

impl Arg {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> &ArgKind {
        &self.kind
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn default(&self) -> Option<&Scalar> {
        self.default.as_ref()
    }

    pub fn is_required(&self) -> bool {
        self.default.is_none()
    }

    pub fn parse_input(&self, raw: &str) -> Result<Scalar, Error> {
        match &self.kind {
            ArgKind::String => Ok(Scalar::Str(raw.to_owned())),
            ArgKind::Bool => match raw {
                "true" => Ok(Scalar::Bool(true)),
                "false" => Ok(Scalar::Bool(false)),
                _ => Err(Error::InvalidArgument(format!(
                    "argument '{}' must be 'true' or 'false'",
                    self.name
                ))),
            },
            ArgKind::Enum { choices } => {
                if choices.iter().any(|c| c == raw) {
                    Ok(Scalar::Str(raw.to_owned()))
                } else {
                    Err(Error::InvalidArgument(format!(
                        "argument '{}' must be one of: {}",
                        self.name,
                        choices.join(", ")
                    )))
                }
            }
            ArgKind::Int => {
                let value = raw.parse::<i64>().map_err(|_| {
                    Error::InvalidArgument(format!("argument '{}' must be an integer", self.name))
                })?;
                Ok(Scalar::Int(value))
            }
            ArgKind::Float => {
                let value = raw.parse::<f64>().map_err(|_| {
                    Error::InvalidArgument(format!(
                        "argument '{}' must be a floating-point number",
                        self.name
                    ))
                })?;
                if !value.is_finite() {
                    return Err(Error::InvalidArgument(format!(
                        "argument '{}' must be finite",
                        self.name
                    )));
                }
                Ok(Scalar::Float(value))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    name: String,
    workflows: BTreeMap<String, Workflow>,
}

impl Package {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn workflows(&self) -> impl Iterator<Item = (&str, &Workflow)> {
        self.workflows
            .iter()
            .map(|(alias, workflow)| (alias.as_str(), workflow))
    }

    pub fn only_workflow(&self) -> Option<&Workflow> {
        if self.workflows.len() == 1 {
            self.workflows.values().next()
        } else {
            None
        }
    }

    pub fn select(&self, alias: Option<&str>) -> Result<&Workflow, Error> {
        match alias {
            Some(alias) => self.workflows.get(alias).ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "unknown workflow '{alias}' for package '{}'",
                    self.name
                ))
            }),
            None => self.only_workflow().ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "package '{}' requires --workflow ({})",
                    self.name,
                    self.workflows
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }),
        }
    }

    pub fn workflow_by_type(&self, workflow_type: &str) -> Option<&Workflow> {
        self.workflows
            .values()
            .find(|workflow| workflow.workflow_type == workflow_type)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    workflow_type: String,
    description: String,
    args: Vec<Arg>,
    positional: bool,
    legacy: bool,
    input: Schema,
}

impl Workflow {
    pub fn workflow_type(&self) -> &str {
        &self.workflow_type
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn args(&self) -> &[Arg] {
        &self.args
    }

    pub fn accepts_positionals(&self) -> bool {
        self.positional
    }

    pub fn input_schema(&self) -> &Schema {
        &self.input
    }

    pub fn required_count(&self) -> usize {
        self.args.iter().filter(|a| a.is_required()).count()
    }

    pub fn signature(&self) -> String {
        if !self.positional {
            return "--input-json <JSON>".into();
        }
        if self.args.is_empty() {
            return "(no args)".into();
        }
        self.args
            .iter()
            .map(|arg| match &arg.default {
                None => format!("<{}:{}>", arg.name, arg.kind.type_label()),
                Some(def) => format!(
                    "[{}:{}={}]",
                    arg.name,
                    arg.kind.type_label(),
                    def.display_text()
                ),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn validate_and_encode(&self, raw_args: &[String]) -> Result<String, Error> {
        if !self.positional {
            return Err(Error::InvalidArgument(
                "structured workflow input needs --input-json".into(),
            ));
        }
        let required = self.required_count();
        let total = self.args.len();
        if raw_args.len() < required || raw_args.len() > total {
            let expected = if required == total {
                format!("{required}")
            } else {
                format!("{required}..={total}")
            };
            return Err(Error::InvalidArgument(format!(
                "workflow '{}' expects {expected} argument(s), got {}",
                self.workflow_type,
                raw_args.len()
            )));
        }
        let mut out = String::from("{");
        for (idx, spec) in self.args.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            write_json_str(&mut out, &spec.name);
            out.push(':');
            let val = match raw_args.get(idx) {
                Some(raw) => spec.parse_input(raw)?,
                None => spec.default.clone().ok_or_else(|| {
                    Error::InvalidArgument(format!("missing required argument '{}'", spec.name))
                })?,
            };
            val.write_json(&mut out);
        }
        out.push('}');
        if self.legacy {
            Ok(out)
        } else {
            self.validate_json(&out)
        }
    }

    pub fn validate_json(&self, raw: &str) -> Result<String, Error> {
        let mut value = input::parse(raw).map_err(Error::InvalidArgument)?;
        if self.legacy
            && let Some(obj) = value.as_object_mut()
        {
            for arg in &self.args {
                if let Some(default) = &arg.default {
                    obj.entry(&arg.name).or_insert_with(|| default.value());
                }
            }
        }
        self.input
            .validate(&value)
            .map_err(Error::InvalidArgument)?;
        Ok(value.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    All,
    None,
    Users(Vec<String>),
}

impl Grant {
    pub fn allows(&self, caller: &str, owner: &str) -> bool {
        if caller == owner {
            return true;
        }
        match self {
            Self::All => true,
            Self::None => false,
            Self::Users(list) => list.iter().any(|u| u == caller),
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::All => "all".into(),
            Self::None => "owner-only".into(),
            Self::Users(users) => {
                if users.is_empty() {
                    "owner-only".into()
                } else {
                    users.join(",")
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    read: Grant,
    write: Grant,
}

impl Permissions {
    pub fn read(&self) -> &Grant {
        &self.read
    }

    pub fn write(&self) -> &Grant {
        &self.write
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worker {
    name: String,
    package: String,
    permissions: Permissions,
}

impl Worker {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn package(&self) -> &str {
        &self.package
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserPolicy {
    user: String,
    permissions: Permissions,
    workers: BTreeMap<String, Worker>,
}

impl UserPolicy {
    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    pub fn workers(&self) -> &BTreeMap<String, Worker> {
        &self.workers
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VerifiedWorker<'a> {
    pub owner: &'a str,
    pub worker: &'a Worker,
    pub workflow: &'a Workflow,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    version: String,
    packages: BTreeMap<String, Package>,
    users: BTreeMap<String, UserPolicy>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidCatalog(String),
    InvalidArgument(String),
    PermissionDenied(String),
    UnknownUser(String),
    UnknownWorker { owner: String, worker: String },
    ExecutionMismatch(String),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::InvalidCatalog(msg)
            | Self::InvalidArgument(msg)
            | Self::PermissionDenied(msg)
            | Self::ExecutionMismatch(msg) => msg.clone(),
            Self::UnknownUser(u) => format!("unknown Boreal user '{u}'"),
            Self::UnknownWorker { owner, worker } => {
                format!("unknown worker '{worker}' for owner '{owner}'")
            }
        }
    }
}

pub fn validate_workflow_id(id: &str) -> Result<(), Error> {
    if id.is_empty()
        || id.trim().is_empty()
        || id.len() > 255
        || id.bytes().any(|b| b.is_ascii_control())
    {
        return Err(Error::InvalidArgument(
            "workflow ID must be 1..=255 printable non-control characters".into(),
        ));
    }
    if id.starts_with("temporal-sys-") {
        return Err(Error::InvalidArgument(
            "workflow ID prefix 'temporal-sys-' is reserved for Temporal system workflows".into(),
        ));
    }
    Ok(())
}

fn valid_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
}

fn valid_arg_ident(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl Catalog {
    pub fn load_system() -> Result<Self, Error> {
        Self::load_from_dir(Path::new(DEFAULT_ETC_DIR), Some(0))
    }

    pub fn load_from_dir(dir: &Path, expected_uid: Option<u32>) -> Result<Self, Error> {
        os::verify_dir_owner_and_mode(dir, expected_uid, 0o027)
            .map_err(|e| Error::InvalidCatalog(e.detail()))?;
        let schemas_bytes = os::read_owned_file(
            &dir.join("schemas.yaml"),
            expected_uid,
            0o227,
            MAX_YAML_BYTES,
        )
        .map_err(|e| Error::InvalidCatalog(e.detail()))?;
        let schemas_text = String::from_utf8(schemas_bytes)
            .map_err(|_| Error::InvalidCatalog("schemas.yaml is not valid UTF-8".into()))?;
        let schema_dirs = parse_schema_directories(&schemas_text)?;
        let mut schema_texts = Vec::with_capacity(schema_dirs.len());
        for schema_dir in &schema_dirs {
            let text = read_schema_file(schema_dir, expected_uid)?;
            schema_texts.push(text);
        }

        let users_dir = dir.join("users");
        os::verify_dir_owner_and_mode(&users_dir, expected_uid, 0o027)
            .map_err(|e| Error::InvalidCatalog(e.detail()))?;

        let mut user_texts = BTreeMap::new();
        for entry in fs::read_dir(&users_dir).map_err(|e| Error::InvalidCatalog(e.to_string()))? {
            let entry = entry.map_err(|e| Error::InvalidCatalog(e.to_string()))?;
            let os_name = entry.file_name();
            let user_name = os_name.to_str().ok_or_else(|| {
                Error::InvalidCatalog("non-UTF-8 entry in /etc/boreal/users".into())
            })?;
            let user_dir = entry.path();
            os::verify_dir_owner_and_mode(&user_dir, expected_uid, 0o027)
                .map_err(|e| Error::InvalidCatalog(e.detail()))?;
            let perms_bytes = os::read_owned_file(
                &user_dir.join("perms.yaml"),
                expected_uid,
                0o227,
                MAX_YAML_BYTES,
            )
            .map_err(|e| Error::InvalidCatalog(e.detail()))?;
            let perms_text = String::from_utf8(perms_bytes).map_err(|_| {
                Error::InvalidCatalog(format!("perms.yaml for {user_name} is not UTF-8"))
            })?;
            user_texts.insert(user_name.to_owned(), perms_text);
        }

        let mut catalog = Self::from_schema_texts(&schema_texts, &user_texts)?;
        match os::read_owned_file(&dir.join("version"), expected_uid, 0o227, MAX_VERSION_BYTES) {
            Ok(raw_ver) => {
                let text = String::from_utf8(raw_ver)
                    .map_err(|_| Error::InvalidCatalog("version file is not valid UTF-8".into()))?;
                let trimmed = text.trim();
                if trimmed.is_empty()
                    || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
                {
                    return Err(Error::InvalidCatalog(
                        "version file must be a single non-empty token".into(),
                    ));
                }
                catalog.version = trimmed.to_owned();
            }
            Err(os::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::InvalidCatalog(err.detail())),
        }
        Ok(catalog)
    }

    pub fn from_yaml(
        schema_yaml: &str,
        users_yaml: &BTreeMap<String, String>,
    ) -> Result<Self, Error> {
        Self::from_schema_texts(&[schema_yaml.to_owned()], users_yaml)
    }

    fn from_schema_texts(
        schema_texts: &[String],
        users_yaml: &BTreeMap<String, String>,
    ) -> Result<Self, Error> {
        let mut packages = BTreeMap::new();
        for text in schema_texts {
            let doc = yaml::parse(text)
                .map_err(|e| Error::InvalidCatalog(format!("schema.yaml parse error: {e:?}")))?;
            for (name, package) in parse_packages(&doc)? {
                if packages.insert(name.clone(), package).is_some() {
                    return Err(Error::InvalidCatalog(format!(
                        "duplicate schema key '{name}'"
                    )));
                }
            }
        }

        let mut users = BTreeMap::new();
        for (dir_user, text) in users_yaml {
            if dir_user == "root" || !valid_ident(dir_user) {
                return Err(Error::InvalidCatalog(format!(
                    "invalid Boreal user directory '{dir_user}'"
                )));
            }
            let doc = yaml::parse(text).map_err(|e| {
                Error::InvalidCatalog(format!("perms.yaml parse error for {dir_user}: {e:?}"))
            })?;
            let policy = parse_user_policy(dir_user, &doc)?;
            users.insert(dir_user.clone(), policy);
        }

        let catalog = Self {
            version: default_lab_version().to_owned(),
            packages,
            users,
        };
        catalog.validate_cross_references()?;
        Ok(catalog)
    }

    pub fn mock() -> Self {
        let workers_yaml = "\
greet:
  workflow: DemoWorkflow
  description: Schedule a durable timer and greet the named recipient
  args:
  - name: name
    type: string
    description: Recipient name
  - name: wait_seconds
    type: int
    default: 20
    description: Durable timer duration in seconds
count:
  workflow: CountWorkflow
  description: Schedule a durable timer and count Unicode scalar values
  args:
  - name: name
    type: string
    description: Input text to count
  - name: wait_seconds
    type: int
    default: 20
    description: Durable timer duration in seconds
";
        let mut users_yaml = BTreeMap::new();
        users_yaml.insert(
            "alice".into(),
            "\
user: alice
permissions:
  read: true
  write: false
workers:
  greet:
    package: greet
    permissions:
      read: true
      write: [bob]
  count:
    package: count
    permissions:
      read: [bob]
      write: false
"
            .into(),
        );
        users_yaml.insert(
            "bob".into(),
            "\
user: bob
permissions:
  read: [alice]
  write: false
workers:
  greet:
    package: greet
    permissions:
      read: [alice]
      write: [alice]
  count:
    package: count
    permissions:
      read: [alice]
      write: false
"
            .into(),
        );
        Self::from_yaml(workers_yaml, &users_yaml).expect("built-in mock catalog must be valid")
    }

    fn validate_cross_references(&self) -> Result<(), Error> {
        let mut unit_keys = BTreeSet::new();
        for (user, policy) in &self.users {
            for grant in [&policy.permissions.read, &policy.permissions.write] {
                if let Grant::Users(targets) = grant {
                    for target in targets {
                        if !self.users.contains_key(target) {
                            return Err(Error::InvalidCatalog(format!(
                                "permission grant references undeclared user '{target}'"
                            )));
                        }
                    }
                }
            }
            for (worker_name, worker) in &policy.workers {
                if !self.packages.contains_key(&worker.package) {
                    return Err(Error::InvalidCatalog(format!(
                        "worker '{user}/{worker_name}' references unknown package '{}'",
                        worker.package
                    )));
                }
                let unit_key = format!("{user}-{worker_name}");
                if !unit_keys.insert(unit_key.clone()) {
                    return Err(Error::InvalidCatalog(format!(
                        "duplicate task queue '{unit_key}'"
                    )));
                }
                for grant in [&worker.permissions.read, &worker.permissions.write] {
                    if let Grant::Users(targets) = grant {
                        for target in targets {
                            if !self.users.contains_key(target) {
                                return Err(Error::InvalidCatalog(format!(
                                    "worker grant references undeclared user '{target}'"
                                )));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn packages(&self) -> &BTreeMap<String, Package> {
        &self.packages
    }

    pub fn users(&self) -> &BTreeMap<String, UserPolicy> {
        &self.users
    }

    pub fn is_user(&self, name: &str) -> bool {
        self.users.contains_key(name)
    }

    pub fn require_user(&self, name: &str) -> Result<&UserPolicy, Error> {
        self.users
            .get(name)
            .ok_or_else(|| Error::UnknownUser(name.to_owned()))
    }

    pub fn worker_and_package(
        &self,
        owner: &str,
        worker: &str,
    ) -> Result<(&Worker, &Package), Error> {
        let policy = self.require_user(owner)?;
        let w = policy
            .workers
            .get(worker)
            .ok_or_else(|| Error::UnknownWorker {
                owner: owner.to_owned(),
                worker: worker.to_owned(),
            })?;
        let pkg = self
            .packages
            .get(&w.package)
            .ok_or_else(|| Error::InvalidCatalog(format!("missing package '{}'", w.package)))?;
        Ok((w, pkg))
    }

    pub fn can_read(&self, caller: &str, owner: &str, worker: &str) -> bool {
        if !self.is_user(caller) {
            return false;
        }
        let Some(policy) = self.users.get(owner) else {
            return false;
        };
        let Some(w) = policy.workers.get(worker) else {
            return false;
        };
        w.permissions.read.allows(caller, owner)
    }

    pub fn can_write(&self, caller: &str, owner: &str, worker: &str) -> bool {
        if !self.is_user(caller) {
            return false;
        }
        let Some(policy) = self.users.get(owner) else {
            return false;
        };
        let Some(w) = policy.workers.get(worker) else {
            return false;
        };
        w.permissions.write.allows(caller, owner)
    }

    pub fn has_any_read(&self, caller: &str, owner: &str) -> bool {
        if !self.is_user(caller) || !self.is_user(owner) {
            return false;
        }
        if caller == owner {
            return true;
        }
        self.users[owner]
            .workers
            .keys()
            .any(|w| self.can_read(caller, owner, w))
    }

    pub fn has_any_write(&self, caller: &str, owner: &str) -> bool {
        if !self.is_user(caller) || !self.is_user(owner) {
            return false;
        }
        if caller == owner {
            return true;
        }
        self.users[owner]
            .workers
            .keys()
            .any(|w| self.can_write(caller, owner, w))
    }

    pub fn has_any_access(&self, caller: &str, owner: &str) -> bool {
        self.has_any_read(caller, owner) || self.has_any_write(caller, owner)
    }

    pub fn readable_owners(&self, caller: &str) -> Vec<&str> {
        self.users
            .keys()
            .map(String::as_str)
            .filter(|owner| self.has_any_read(caller, owner))
            .collect()
    }

    pub fn writable_owners(&self, caller: &str) -> Vec<&str> {
        self.users
            .keys()
            .map(String::as_str)
            .filter(|owner| self.has_any_write(caller, owner))
            .collect()
    }

    pub fn visible_permission_users(&self, caller: &str) -> Vec<&str> {
        self.users
            .keys()
            .map(String::as_str)
            .filter(|owner| self.has_any_access(caller, owner))
            .collect()
    }

    pub fn list_accessible_workers(&self, caller: &str) -> Result<Vec<WorkerEntry>, Error> {
        if !self.is_user(caller) {
            return Err(Error::PermissionDenied(format!(
                "caller '{caller}' is not a registered Boreal user"
            )));
        }
        let mut out = Vec::new();
        for (owner, policy) in &self.users {
            for (wname, worker) in &policy.workers {
                let read = self.can_read(caller, owner, wname);
                let write = self.can_write(caller, owner, wname);
                if !read && !write {
                    continue;
                }
                let pkg = self.packages.get(&worker.package).ok_or_else(|| {
                    Error::InvalidCatalog(format!("missing package '{}'", worker.package))
                })?;
                out.extend(pkg.workflows.iter().map(|(alias, workflow)| WorkerEntry {
                    owner: owner.clone(),
                    worker: wname.clone(),
                    workflow_type: workflow.workflow_type.clone(),
                    read,
                    write,
                    signature: if pkg.workflows.len() == 1 {
                        workflow.signature()
                    } else {
                        format!("--workflow {alias} {}", workflow.signature())
                    },
                    description: workflow.description.clone(),
                }));
            }
        }
        Ok(out)
    }

    pub fn inspect_permissions(
        &self,
        caller: &str,
        target: Option<&str>,
    ) -> Result<PermissionReport, Error> {
        if !self.is_user(caller) {
            return Err(Error::PermissionDenied(format!(
                "caller '{caller}' is not a registered Boreal user"
            )));
        }
        let target_user = target.unwrap_or(caller);
        let policy = self.require_user(target_user)?;
        if caller == target_user {
            let workers = policy
                .workers
                .iter()
                .map(|(wname, w)| WorkerPermissionLine {
                    worker: wname.clone(),
                    package: w.package.clone(),
                    read: w.permissions.read.summary(),
                    write: w.permissions.write.summary(),
                })
                .collect();
            return Ok(PermissionReport {
                user: target_user.to_owned(),
                is_self: true,
                default_read: Some(policy.permissions.read.summary()),
                default_write: Some(policy.permissions.write.summary()),
                workers,
            });
        }
        if !self.has_any_access(caller, target_user) {
            return Err(Error::PermissionDenied(format!(
                "permission denied: '{caller}' has no access to '{target_user}' workers"
            )));
        }
        let workers = policy
            .workers
            .iter()
            .filter_map(|(wname, w)| {
                let r = self.can_read(caller, target_user, wname);
                let wr = self.can_write(caller, target_user, wname);
                if !r && !wr {
                    return None;
                }
                Some(WorkerPermissionLine {
                    worker: wname.clone(),
                    package: w.package.clone(),
                    read: if r { "allowed" } else { "denied" }.into(),
                    write: if wr { "allowed" } else { "denied" }.into(),
                })
            })
            .collect();
        Ok(PermissionReport {
            user: target_user.to_owned(),
            is_self: false,
            default_read: None,
            default_write: None,
            workers,
        })
    }

    pub fn verify_execution<'a>(
        &'a self,
        owner: &'a str,
        task_queue: &str,
        workflow_type: &str,
        run_id: &str,
    ) -> Result<VerifiedWorker<'a>, Error> {
        if run_id.trim().is_empty() {
            return Err(Error::ExecutionMismatch(
                "workflow execution is missing a valid run ID".into(),
            ));
        }
        let policy = self.require_user(owner)?;
        let prefix = format!("{owner}-");
        let worker_name = task_queue.strip_prefix(&prefix).ok_or_else(|| {
            Error::ExecutionMismatch(format!(
                "execution task queue '{task_queue}' does not belong to owner '{owner}'"
            ))
        })?;
        let worker = policy.workers.get(worker_name).ok_or_else(|| {
            Error::ExecutionMismatch(format!(
                "execution task queue '{task_queue}' is not a registered worker of '{owner}'"
            ))
        })?;
        let package = self.packages.get(&worker.package).ok_or_else(|| {
            Error::InvalidCatalog(format!("missing package '{}'", worker.package))
        })?;
        let workflow = package.workflow_by_type(workflow_type).ok_or_else(|| {
            Error::ExecutionMismatch(format!(
                "execution workflow '{workflow_type}' is not exported by '{}'",
                package.name
            ))
        })?;
        Ok(VerifiedWorker {
            owner,
            worker,
            workflow,
        })
    }
}

fn parse_schema_directories(text: &str) -> Result<Vec<PathBuf>, Error> {
    let doc = yaml::parse(text)
        .map_err(|e| Error::InvalidCatalog(format!("schemas.yaml parse error: {e:?}")))?;
    let map = match &doc {
        Value::Map(map) => map,
        Value::Null => return Ok(Vec::new()),
        _ => {
            return Err(Error::InvalidCatalog(
                "schemas.yaml root must be a mapping".into(),
            ));
        }
    };
    let seq = match map.get("directories") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Seq(items)) => items,
        Some(_) => {
            return Err(Error::InvalidCatalog(
                "schemas.yaml directories must be a list".into(),
            ));
        }
    };
    let mut dirs = Vec::with_capacity(seq.len());
    for item in seq {
        let raw = item.as_str().ok_or_else(|| {
            Error::InvalidCatalog("schemas.yaml directory entries must be strings".into())
        })?;
        let path = PathBuf::from(raw);
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::InvalidCatalog(format!(
                "schema directory '{raw}' must be an absolute path without '..'"
            )));
        }
        dirs.push(path);
    }
    Ok(dirs)
}

fn read_schema_file(dir: &Path, expected_uid: Option<u32>) -> Result<String, Error> {
    let meta = fs::symlink_metadata(dir).map_err(|e| Error::InvalidCatalog(e.to_string()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(Error::InvalidCatalog(format!(
            "{} must be a non-symlink directory",
            dir.display()
        )));
    }
    if let Some(uid) = expected_uid
        && meta.uid() != uid
    {
        return Err(Error::InvalidCatalog(format!(
            "{} is owned by uid {}, expected {uid}",
            dir.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(Error::InvalidCatalog(format!(
            "{} is writable by group or other",
            dir.display()
        )));
    }
    let path = dir.join("schema.yaml");
    let bytes = os::read_owned_file(&path, expected_uid, 0o022, MAX_YAML_BYTES)
        .map_err(|e| Error::InvalidCatalog(e.detail()))?;
    String::from_utf8(bytes)
        .map_err(|_| Error::InvalidCatalog(format!("{} is not valid UTF-8", path.display())))
}

fn parse_packages(doc: &Value) -> Result<BTreeMap<String, Package>, Error> {
    let map = match doc {
        Value::Null => return Ok(BTreeMap::new()),
        Value::Map(m) => m,
        _ => {
            return Err(Error::InvalidCatalog(
                "schema.yaml root must be a mapping".into(),
            ));
        }
    };
    let mut packages = BTreeMap::new();
    for (name, val) in map {
        if !valid_ident(name) {
            return Err(Error::InvalidCatalog(format!(
                "invalid worker package name '{name}'"
            )));
        }
        let pkg_map = val
            .as_map()
            .ok_or_else(|| Error::InvalidCatalog(format!("package '{name}' must be a mapping")))?;
        let workflows = if let Some(exports) = pkg_map.get("workflows") {
            if ["workflow", "description", "args", "input_schema"]
                .iter()
                .any(|key| pkg_map.contains_key(*key))
            {
                return Err(Error::InvalidCatalog(format!(
                    "package '{name}' mixes single and named workflow fields"
                )));
            }
            let exports = exports
                .as_map()
                .filter(|exports| !exports.is_empty())
                .ok_or_else(|| {
                    Error::InvalidCatalog(format!(
                        "package '{name}' workflows must be a non-empty mapping"
                    ))
                })?;
            let mut types = BTreeSet::new();
            let mut workflows = BTreeMap::new();
            for (alias, entry) in exports {
                if !valid_arg_ident(alias) {
                    return Err(Error::InvalidCatalog(format!(
                        "invalid workflow alias '{alias}' in package '{name}'"
                    )));
                }
                let context = format!("{name}/{alias}");
                let map = entry.as_map().ok_or_else(|| {
                    Error::InvalidCatalog(format!("workflow '{context}' must be a mapping"))
                })?;
                let workflow = parse_workflow(&context, map)?;
                if !types.insert(workflow.workflow_type.clone()) {
                    return Err(Error::InvalidCatalog(format!(
                        "duplicate Temporal workflow '{}' in package '{name}'",
                        workflow.workflow_type
                    )));
                }
                workflows.insert(alias.clone(), workflow);
            }
            workflows
        } else {
            BTreeMap::from([(name.clone(), parse_workflow(name, pkg_map)?)])
        };
        packages.insert(
            name.clone(),
            Package {
                name: name.clone(),
                workflows,
            },
        );
    }
    Ok(packages)
}

fn parse_workflow(name: &str, map: &BTreeMap<String, Value>) -> Result<Workflow, Error> {
    let workflow_type = map
        .get("workflow")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            Error::InvalidCatalog(format!("package '{name}' requires non-empty workflow"))
        })?
        .to_owned();
    let description = match map.get("description") {
        Some(Value::Str(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        _ => {
            return Err(Error::InvalidCatalog(format!(
                "package '{name}' description must be a string"
            )));
        }
    };
    let raw_args = match map.get("args") {
        Some(Value::Seq(items)) => items.as_slice(),
        None | Some(Value::Null) => &[],
        _ => {
            return Err(Error::InvalidCatalog(format!(
                "package '{name}' args must be a sequence"
            )));
        }
    };
    let mut args = Vec::with_capacity(raw_args.len());
    let mut seen = BTreeSet::new();
    let mut seen_optional = false;
    for raw_arg in raw_args {
        let arg = parse_arg(name, raw_arg)?;
        if !seen.insert(arg.name.clone()) {
            return Err(Error::InvalidCatalog(format!(
                "duplicate argument '{}' in package '{name}'",
                arg.name
            )));
        }
        if arg.is_required() && seen_optional {
            return Err(Error::InvalidCatalog(format!(
                "required argument '{}' follows optional argument in '{name}'",
                arg.name
            )));
        }
        if !arg.is_required() {
            seen_optional = true;
        }
        args.push(arg);
    }
    let full = match map.get("input_schema") {
        None => None,
        Some(Value::Str(raw)) => Some(
            input::parse(raw)
                .map_err(|e| Error::InvalidCatalog(format!("workflow '{name}': {e}")))?,
        ),
        _ => {
            return Err(Error::InvalidCatalog(format!(
                "workflow '{name}' input_schema must be a JSON string"
            )));
        }
    };
    let legacy = full.is_none();
    let schema = Schema::new(full.unwrap_or_else(|| legacy_schema(&args)))
        .map_err(|e| Error::InvalidCatalog(format!("workflow '{name}' input schema: {e}")))?;
    Ok(Workflow {
        workflow_type,
        description,
        args,
        positional: legacy || map.contains_key("args"),
        legacy,
        input: schema,
    })
}

fn legacy_schema(args: &[Arg]) -> input::Value {
    let properties: input::Map<String, input::Value> = args.iter().map(|arg| {
        let mut schema = match &arg.kind {
            ArgKind::String => json!({"type":"string"}),
            ArgKind::Bool => json!({"type":"boolean"}),
            ArgKind::Int => json!({"type":"integer", "format":"int64", "minimum": i64::MIN, "maximum": i64::MAX}),
            ArgKind::Float => json!({"type":"number", "format":"double"}),
            ArgKind::Enum { choices } => json!({"type":"string", "enum":choices}),
        };
        if !arg.description.is_empty() {
            schema["description"] = json!(arg.description);
        }
        if let Some(default) = &arg.default {
            schema["default"] = default.value();
        }
        (arg.name.clone(), schema)
    }).collect();
    let required: Vec<_> = args
        .iter()
        .filter(|arg| arg.is_required())
        .map(Arg::name)
        .collect();
    json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
}

fn parse_arg(pkg: &str, val: &Value) -> Result<Arg, Error> {
    let map = val
        .as_map()
        .ok_or_else(|| Error::InvalidCatalog(format!("argument in '{pkg}' must be a mapping")))?;
    let name = map
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| valid_arg_ident(n))
        .ok_or_else(|| Error::InvalidCatalog(format!("invalid argument name in '{pkg}'")))?
        .to_owned();
    let description = match map.get("description") {
        Some(Value::Str(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        _ => {
            return Err(Error::InvalidCatalog(format!(
                "argument '{name}' description must be a string"
            )));
        }
    };
    let type_name = map
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidCatalog(format!("argument '{name}' missing type")))?;

    let has_choices = map
        .get("choices")
        .is_some_and(|v| !matches!(v, Value::Null));

    let kind = match type_name {
        "string" => {
            if has_choices {
                return Err(Error::InvalidCatalog(format!(
                    "string argument '{name}' cannot have choices"
                )));
            }
            ArgKind::String
        }
        "bool" => {
            if has_choices {
                return Err(Error::InvalidCatalog(format!(
                    "bool argument '{name}' cannot have choices"
                )));
            }
            ArgKind::Bool
        }
        "enum" => {
            let seq = map
                .get("choices")
                .and_then(Value::as_seq)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    Error::InvalidCatalog(format!("enum argument '{name}' requires choices"))
                })?;
            let mut choices = Vec::with_capacity(seq.len());
            let mut unique = BTreeSet::new();
            for item in seq {
                let s = item
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        Error::InvalidCatalog(format!("invalid enum choice in '{name}'"))
                    })?;
                if !unique.insert(s) {
                    return Err(Error::InvalidCatalog(format!(
                        "duplicate enum choice '{s}' in '{name}'"
                    )));
                }
                choices.push(s.to_owned());
            }
            ArgKind::Enum { choices }
        }
        "int" => {
            if has_choices {
                return Err(Error::InvalidCatalog(format!(
                    "int argument '{name}' cannot have choices"
                )));
            }
            ArgKind::Int
        }
        "float" => {
            if has_choices {
                return Err(Error::InvalidCatalog(format!(
                    "float argument '{name}' cannot have choices"
                )));
            }
            ArgKind::Float
        }
        other => {
            return Err(Error::InvalidCatalog(format!(
                "unknown argument type '{other}' on '{name}'"
            )));
        }
    };

    let default = match map.get("default") {
        None | Some(Value::Null) => None,
        Some(raw_default) => Some(parse_default(&name, &kind, raw_default)?),
    };

    Ok(Arg {
        name,
        kind,
        description,
        default,
    })
}

fn parse_default(name: &str, kind: &ArgKind, val: &Value) -> Result<Scalar, Error> {
    match kind {
        ArgKind::String => val
            .as_str()
            .map(|s| Scalar::Str(s.to_owned()))
            .ok_or_else(|| Error::InvalidCatalog(format!("default for '{name}' must be string"))),
        ArgKind::Bool => val
            .as_bool()
            .map(Scalar::Bool)
            .ok_or_else(|| Error::InvalidCatalog(format!("default for '{name}' must be bool"))),
        ArgKind::Enum { choices } => {
            let s = val.as_str().ok_or_else(|| {
                Error::InvalidCatalog(format!("default for '{name}' must be string"))
            })?;
            if !choices.iter().any(|c| c == s) {
                return Err(Error::InvalidCatalog(format!(
                    "default '{s}' for '{name}' is not in choices"
                )));
            }
            Ok(Scalar::Str(s.to_owned()))
        }
        ArgKind::Int => val
            .as_i64()
            .map(Scalar::Int)
            .ok_or_else(|| Error::InvalidCatalog(format!("default for '{name}' must be int"))),
        ArgKind::Float => val
            .as_f64()
            .filter(|f| f.is_finite())
            .map(Scalar::Float)
            .ok_or_else(|| Error::InvalidCatalog(format!("default for '{name}' must be float"))),
    }
}

fn parse_grant(val: &Value) -> Result<Grant, Error> {
    match val {
        Value::Bool(true) => Ok(Grant::All),
        Value::Bool(false) => Ok(Grant::None),
        Value::Seq(items) => {
            let mut users = Vec::with_capacity(items.len());
            for item in items {
                let name = item
                    .as_str()
                    .filter(|s| valid_ident(s) && *s != "root")
                    .ok_or_else(|| {
                        Error::InvalidCatalog("invalid username in permission list".into())
                    })?;
                if !users.iter().any(|u| u == name) {
                    users.push(name.to_owned());
                }
            }
            Ok(Grant::Users(users))
        }
        _ => Err(Error::InvalidCatalog(
            "permission must be a boolean or list of usernames".into(),
        )),
    }
}

fn parse_permissions(val: &Value) -> Result<Permissions, Error> {
    let map = val
        .as_map()
        .ok_or_else(|| Error::InvalidCatalog("permissions must be a mapping".into()))?;
    let read = parse_grant(
        map.get("read")
            .ok_or_else(|| Error::InvalidCatalog("permissions missing 'read'".into()))?,
    )?;
    let write = parse_grant(
        map.get("write")
            .ok_or_else(|| Error::InvalidCatalog("permissions missing 'write'".into()))?,
    )?;
    Ok(Permissions { read, write })
}

fn parse_user_policy(expected_user: &str, doc: &Value) -> Result<UserPolicy, Error> {
    let map = doc
        .as_map()
        .ok_or_else(|| Error::InvalidCatalog("perms.yaml root must be a mapping".into()))?;
    let user = map
        .get("user")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidCatalog("perms.yaml missing 'user'".into()))?;
    if user != expected_user {
        return Err(Error::InvalidCatalog(format!(
            "perms.yaml user '{user}' does not match directory '{expected_user}'"
        )));
    }
    let permissions = parse_permissions(
        map.get("permissions")
            .ok_or_else(|| Error::InvalidCatalog("perms.yaml missing 'permissions'".into()))?,
    )?;
    let mut workers = BTreeMap::new();
    match map.get("workers") {
        None | Some(Value::Null) => {}
        Some(Value::Map(wmap)) => {
            for (wname, wval) in wmap {
                if !valid_ident(wname) {
                    return Err(Error::InvalidCatalog(format!(
                        "invalid worker name '{wname}'"
                    )));
                }
                let entry = wval.as_map().ok_or_else(|| {
                    Error::InvalidCatalog(format!("worker '{wname}' must be a mapping"))
                })?;
                let package = entry
                    .get("package")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::InvalidCatalog(format!("worker '{wname}' missing package"))
                    })?
                    .to_owned();
                let worker_perms =
                    parse_permissions(entry.get("permissions").ok_or_else(|| {
                        Error::InvalidCatalog(format!("worker '{wname}' missing permissions"))
                    })?)?;
                workers.insert(
                    wname.clone(),
                    Worker {
                        name: wname.clone(),
                        package,
                        permissions: worker_perms,
                    },
                );
            }
        }
        _ => {
            return Err(Error::InvalidCatalog(
                "perms.yaml 'workers' must be a mapping".into(),
            ));
        }
    }
    Ok(UserPolicy {
        user: user.to_owned(),
        permissions,
        workers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_lab_policy_argument_encoding_and_execution_verification() {
        let mut users = BTreeMap::new();
        users.insert(
            "alice".into(),
            "\
user: alice
permissions:
  read: true
  write: false
workers:
  greet:
    package: greet
    permissions:
      read: true
      write: [bob]
  count:
    package: count
    permissions:
      read: [bob]
      write: false
"
            .into(),
        );
        users.insert(
            "bob".into(),
            "\
user: bob
permissions:
  read: false
  write: false
workers:
  greet:
    package: greet
    permissions:
      read: false
      write: false
"
            .into(),
        );
        users.insert(
            "carol".into(),
            "\
user: carol
permissions:
  read: false
  write: false
workers: {}
"
            .into(),
        );
        let workers = "\
greet:
  workflow: DemoWorkflow
  description: Greet
  args:
  - name: name
    type: string
  - name: wait_seconds
    type: int
    default: 20
count:
  workflow: CountWorkflow
  description: Count
  args:
  - name: name
    type: string
  - name: wait_seconds
    type: int
    default: 20
";
        let cat = Catalog::from_yaml(workers, &users).unwrap();

        assert!(cat.can_write("bob", "alice", "greet"));
        assert!(!cat.can_write("bob", "alice", "count"));
        assert!(cat.can_read("bob", "alice", "count"));
        assert!(cat.can_read("carol", "alice", "greet"));
        assert!(!cat.can_read("carol", "alice", "count"));
        assert!(!cat.has_any_read("carol", "bob"));
        assert!(!cat.can_read("mallory", "alice", "greet"));

        let (_, pkg) = cat.worker_and_package("alice", "greet").unwrap();
        let workflow = pkg.select(None).unwrap();
        assert_eq!(
            workflow
                .validate_and_encode(&["Alice \"A\"".into()])
                .unwrap(),
            r#"{"name":"Alice \"A\"","wait_seconds":20}"#
        );
        assert_eq!(
            workflow
                .validate_and_encode(&["Alice".into(), "0".into()])
                .unwrap(),
            r#"{"name":"Alice","wait_seconds":0}"#
        );
        assert!(workflow.validate_and_encode(&[]).is_err());

        let verified = cat
            .verify_execution("alice", "alice-greet", "DemoWorkflow", "run-123")
            .unwrap();
        assert_eq!(verified.worker.name(), "greet");
        assert!(
            cat.verify_execution("alice", "alice-greet", "CountWorkflow", "run-123")
                .is_err()
        );
        assert!(
            cat.verify_execution("alice", "alice-greet", "DemoWorkflow", " ")
                .is_err()
        );
        assert!(
            cat.verify_execution("alice", "bob-greet", "DemoWorkflow", "run-123")
                .is_err()
        );
        assert_eq!(cat.version(), default_lab_version());
        assert_eq!(default_lab_version(), "0.1.0");
    }

    #[test]
    fn test_named_workflows_share_queue_but_validate_separately() {
        let schemas = "\
example:
  workflows:
    greet:
      workflow: Greet
      args:
        - name: name
          type: string
    count:
      workflow: Count
      args:
        - name: limit
          type: int
";
        let users = BTreeMap::from([(
            "alice".into(),
            "\
user: alice
permissions: {read: false, write: false}
workers:
  tools:
    package: example
    permissions: {read: false, write: false}
"
            .into(),
        )]);
        let catalog = Catalog::from_yaml(schemas, &users).unwrap();
        let (_, package) = catalog.worker_and_package("alice", "tools").unwrap();
        assert!(package.select(None).is_err());
        assert!(package.select(Some("missing")).is_err());
        for (alias, ty, input) in [
            ("greet", "Greet", r#"{"name":"7"}"#),
            ("count", "Count", r#"{"limit":7}"#),
        ] {
            let workflow = package.select(Some(alias)).unwrap();
            assert_eq!(workflow.validate_and_encode(&["7".into()]).unwrap(), input);
            assert_eq!(
                catalog
                    .verify_execution("alice", "alice-tools", ty, "run-id")
                    .unwrap()
                    .workflow,
                workflow
            );
        }
        assert!(
            catalog
                .verify_execution("alice", "alice-tools", "Internal", "run-id")
                .is_err()
        );
    }

    #[test]
    fn test_catalog_validates_structured_payloads_separately_from_positionals() {
        let doc = yaml::parse(r##"
demo:
  workflow: Demo
  input_schema: |-
    {"type":"object","properties":{"nested":{"$ref":"#/$defs/Nested"}},"required":["nested"],"$defs":{"Nested":{"type":"object","required":["left"],"properties":{"left":{"oneOf":[{"type":"object","required":["Integer"],"properties":{"Integer":{"type":"integer"}}},{"type":"object","required":["Float"],"properties":{"Float":{"type":"number"}}}]}}}}}
legacy:
  workflow: Legacy
  args:
    - name: seconds
      type: int
      default: 20
"##).unwrap();
        let packages = parse_packages(&doc).unwrap();
        let workflow = packages["demo"].select(None).unwrap();
        let good = r#"{"nested":{"left":{"Integer":18446744073709551615}}}"#;
        assert_eq!(workflow.validate_json(good).unwrap(), good);
        assert!(
            workflow
                .validate_json(r#"{"nested":{"left":{"Integer":"bad"}}}"#)
                .is_err()
        );
        assert!(workflow.validate_and_encode(&[]).is_err());
        assert_eq!(
            packages["legacy"]
                .select(None)
                .unwrap()
                .validate_json("{}")
                .unwrap(),
            r#"{"seconds":20}"#
        );
    }

    #[test]
    fn test_cargo_manifest_and_etc_version_loading() {
        let cases = [
            (
                "[workspace]\nresolver = \"3\"\n[workspace.package]\nversion = \"0.4.2\" # comment\n",
                Some("0.4.2"),
            ),
            (
                "[package]\nname = \"worker\"\nversion = \"1.2.0-rc.1\"\n",
                Some("1.2.0-rc.1"),
            ),
            ("[dependencies]\nserde = { version = \"1.0\" }\n", None),
        ];
        for (manifest, expected) in cases {
            assert_eq!(parse_cargo_version(manifest), expected, "{manifest}");
        }

        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let dir = std::env::temp_dir().join(format!("boreal-cat-ver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.join("users"))
            .unwrap();
        let schema_dir = dir.join("schema");
        fs::DirBuilder::new()
            .mode(0o755)
            .create(&schema_dir)
            .unwrap();
        let schema = schema_dir.join("schema.yaml");
        fs::write(&schema, "w:\n  workflow: Demo\n").unwrap();
        fs::set_permissions(&schema, fs::Permissions::from_mode(0o444)).unwrap();
        let schemas = dir.join("schemas.yaml");
        fs::write(
            &schemas,
            format!("directories:\n  - {}\n", schema_dir.display()),
        )
        .unwrap();
        fs::set_permissions(&schemas, fs::Permissions::from_mode(0o400)).unwrap();

        let fallback = Catalog::load_from_dir(&dir, None).unwrap();
        assert_eq!(fallback.version(), "0.1.0");

        let ver_file = dir.join("version");
        fs::write(&ver_file, "0.3.1\n").unwrap();
        fs::set_permissions(&ver_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Catalog::load_from_dir(&dir, None).is_err());

        fs::set_permissions(&ver_file, fs::Permissions::from_mode(0o400)).unwrap();
        let loaded = Catalog::load_from_dir(&dir, None).unwrap();
        assert_eq!(loaded.version(), "0.3.1");

        let _ = fs::remove_dir_all(&dir);
    }
}
