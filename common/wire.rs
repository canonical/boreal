use std::io::{self, Read, Write};

pub const BROKER_SOCKET: &str = "/run/boreal/broker.sock";
pub const UPDATER_SOCKET: &str = "/run/boreal/updater.sock";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    PermissionDenied,
    InvalidArgument,
    NotFound,
    AdmissionClosed,
    Unavailable,
    Internal,
}

impl ErrorCode {
    fn tag(self) -> u8 {
        match self {
            Self::PermissionDenied => 1,
            Self::InvalidArgument => 2,
            Self::NotFound => 3,
            Self::AdmissionClosed => 4,
            Self::Unavailable => 5,
            Self::Internal => 6,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, Error> {
        match tag {
            1 => Ok(Self::PermissionDenied),
            2 => Ok(Self::InvalidArgument),
            3 => Ok(Self::NotFound),
            4 => Ok(Self::AdmissionClosed),
            5 => Ok(Self::Unavailable),
            6 => Ok(Self::Internal),
            _ => Err(Error::Decode("invalid error code tag")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Workers,
    Start {
        owner: String,
        worker: String,
        id: String,
        args: Vec<String>,
    },
    List {
        owner: String,
    },
    Status {
        owner: String,
        id: String,
    },
    Result {
        owner: String,
        id: String,
    },
    PrepareCancel {
        owner: String,
        id: String,
    },
    Cancel {
        owner: String,
        id: String,
        run_id: String,
    },
    Permissions {
        user: Option<String>,
    },
    Summary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerEntry {
    pub owner: String,
    pub worker: String,
    pub workflow_type: String,
    pub read: bool,
    pub write: bool,
    pub signature: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowSummary {
    pub owner: String,
    pub worker: String,
    pub workflow_type: String,
    pub workflow_id: String,
    pub run_id: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowDetail {
    pub owner: String,
    pub worker: String,
    pub workflow_type: String,
    pub task_queue: String,
    pub workflow_id: String,
    pub run_id: String,
    pub status: String,
    pub start_time: String,
    pub close_time: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultOutcome {
    Completed {
        run_id: String,
        output: String,
    },
    Running {
        run_id: String,
    },
    Unsuccessful {
        run_id: String,
        status: String,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerPermissionLine {
    pub worker: String,
    pub package: String,
    pub read: String,
    pub write: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionReport {
    pub user: String,
    pub is_self: bool,
    pub default_read: Option<String>,
    pub default_write: Option<String>,
    pub workers: Vec<WorkerPermissionLine>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkflowCounts {
    pub paused: u32,
    pub running: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptSummary {
    pub workflows: Option<WorkflowCounts>,
    pub draining: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Workers(Vec<WorkerEntry>),
    Started {
        owner: String,
        worker: String,
        workflow_type: String,
        workflow_id: String,
        run_id: String,
    },
    List(Vec<WorkflowSummary>),
    Status(WorkflowDetail),
    WorkflowResult(ResultOutcome),
    CancelPrepared(WorkflowDetail),
    Cancelled {
        owner: String,
        worker: String,
        workflow_type: String,
        workflow_id: String,
        run_id: String,
    },
    Permissions(PermissionReport),
    Error {
        code: ErrorCode,
        message: String,
    },
    Summary(PromptSummary),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdaterRequest {
    GateStatus,
    CloseGate { update_id: String, rev: String },
    CheckDrain { update_id: String },
    ReopenGate { update_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdaterResponse {
    GateStatus {
        open: bool,
        update_id: Option<String>,
        rev: Option<String>,
    },
    GateClosed,
    DrainQuiescent,
    DrainBusy {
        active_workflows: u32,
        active_leases: u32,
        detail: String,
    },
    GateReopened,
    Error(String),
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    FrameTooLarge,
    Decode(&'static str),
}

impl Error {
    pub fn detail(&self) -> String {
        match self {
            Self::Io(e) => e.to_string(),
            Self::FrameTooLarge => "IPC frame exceeds maximum size".into(),
            Self::Decode(msg) => (*msg).into(),
        }
    }
}

pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), Error> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge);
    }
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len).map_err(Error::Io)?;
    writer.write_all(payload).map_err(Error::Io)?;
    writer.flush().map_err(Error::Io)
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, Error> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).map_err(Error::Io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(Error::Io)?;
    Ok(buf)
}

struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }

    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            Some(v) => {
                self.u8(1);
                self.str(v);
            }
            None => self.u8(0),
        }
    }

    fn opt_counts(&mut self, val: Option<WorkflowCounts>) {
        match val {
            Some(counts) => {
                self.u8(1);
                self.u32(counts.paused);
                self.u32(counts.running);
            }
            None => self.u8(0),
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or(Error::Decode("truncated IPC frame"))?;
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Decode("invalid bool byte")),
        }
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn str(&mut self) -> Result<String, Error> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Error::Decode("non-UTF-8 string in IPC frame"))
    }

    fn opt_str(&mut self) -> Result<Option<String>, Error> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.str()?)),
            _ => Err(Error::Decode("invalid option tag")),
        }
    }

    fn opt_counts(&mut self) -> Result<Option<WorkflowCounts>, Error> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(WorkflowCounts {
                paused: self.u32()?,
                running: self.u32()?,
            })),
            _ => Err(Error::Decode("invalid option tag")),
        }
    }

    fn finish(&self) -> Result<(), Error> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Decode("unexpected trailing bytes in IPC frame"))
        }
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::new();
        match self {
            Self::Workers => enc.u8(1),
            Self::Start {
                owner,
                worker,
                id,
                args,
            } => {
                enc.u8(2);
                enc.str(owner);
                enc.str(worker);
                enc.str(id);
                enc.u32(args.len() as u32);
                for arg in args {
                    enc.str(arg);
                }
            }
            Self::List { owner } => {
                enc.u8(3);
                enc.str(owner);
            }
            Self::Status { owner, id } => {
                enc.u8(4);
                enc.str(owner);
                enc.str(id);
            }
            Self::Result { owner, id } => {
                enc.u8(5);
                enc.str(owner);
                enc.str(id);
            }
            Self::PrepareCancel { owner, id } => {
                enc.u8(6);
                enc.str(owner);
                enc.str(id);
            }
            Self::Cancel { owner, id, run_id } => {
                enc.u8(7);
                enc.str(owner);
                enc.str(id);
                enc.str(run_id);
            }
            Self::Permissions { user } => {
                enc.u8(8);
                enc.opt_str(user.as_deref());
            }
            Self::Summary => enc.u8(9),
        }
        enc.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut dec = Decoder::new(bytes);
        let req = match dec.u8()? {
            1 => Self::Workers,
            2 => {
                let owner = dec.str()?;
                let worker = dec.str()?;
                let id = dec.str()?;
                let count = dec.u32()? as usize;
                if count > 256 {
                    return Err(Error::Decode("too many workflow arguments"));
                }
                let mut args = Vec::with_capacity(count);
                for _ in 0..count {
                    args.push(dec.str()?);
                }
                Self::Start {
                    owner,
                    worker,
                    id,
                    args,
                }
            }
            3 => Self::List { owner: dec.str()? },
            4 => Self::Status {
                owner: dec.str()?,
                id: dec.str()?,
            },
            5 => Self::Result {
                owner: dec.str()?,
                id: dec.str()?,
            },
            6 => Self::PrepareCancel {
                owner: dec.str()?,
                id: dec.str()?,
            },
            7 => Self::Cancel {
                owner: dec.str()?,
                id: dec.str()?,
                run_id: dec.str()?,
            },
            8 => Self::Permissions {
                user: dec.opt_str()?,
            },
            9 => Self::Summary,
            _ => return Err(Error::Decode("unknown Request tag")),
        };
        dec.finish()?;
        Ok(req)
    }
}

fn encode_detail(enc: &mut Encoder, d: &WorkflowDetail) {
    enc.str(&d.owner);
    enc.str(&d.worker);
    enc.str(&d.workflow_type);
    enc.str(&d.task_queue);
    enc.str(&d.workflow_id);
    enc.str(&d.run_id);
    enc.str(&d.status);
    enc.str(&d.start_time);
    enc.str(&d.close_time);
}

fn decode_detail(dec: &mut Decoder<'_>) -> Result<WorkflowDetail, Error> {
    Ok(WorkflowDetail {
        owner: dec.str()?,
        worker: dec.str()?,
        workflow_type: dec.str()?,
        task_queue: dec.str()?,
        workflow_id: dec.str()?,
        run_id: dec.str()?,
        status: dec.str()?,
        start_time: dec.str()?,
        close_time: dec.str()?,
    })
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::new();
        match self {
            Self::Workers(items) => {
                enc.u8(1);
                enc.u32(items.len() as u32);
                for item in items {
                    enc.str(&item.owner);
                    enc.str(&item.worker);
                    enc.str(&item.workflow_type);
                    enc.bool(item.read);
                    enc.bool(item.write);
                    enc.str(&item.signature);
                    enc.str(&item.description);
                }
            }
            Self::Started {
                owner,
                worker,
                workflow_type,
                workflow_id,
                run_id,
            } => {
                enc.u8(2);
                enc.str(owner);
                enc.str(worker);
                enc.str(workflow_type);
                enc.str(workflow_id);
                enc.str(run_id);
            }
            Self::List(items) => {
                enc.u8(3);
                enc.u32(items.len() as u32);
                for item in items {
                    enc.str(&item.owner);
                    enc.str(&item.worker);
                    enc.str(&item.workflow_type);
                    enc.str(&item.workflow_id);
                    enc.str(&item.run_id);
                    enc.str(&item.status);
                }
            }
            Self::Status(d) => {
                enc.u8(4);
                encode_detail(&mut enc, d);
            }
            Self::WorkflowResult(outcome) => {
                enc.u8(5);
                match outcome {
                    ResultOutcome::Completed { run_id, output } => {
                        enc.u8(1);
                        enc.str(run_id);
                        enc.str(output);
                    }
                    ResultOutcome::Running { run_id } => {
                        enc.u8(2);
                        enc.str(run_id);
                    }
                    ResultOutcome::Unsuccessful {
                        run_id,
                        status,
                        detail,
                    } => {
                        enc.u8(3);
                        enc.str(run_id);
                        enc.str(status);
                        enc.str(detail);
                    }
                }
            }
            Self::CancelPrepared(d) => {
                enc.u8(6);
                encode_detail(&mut enc, d);
            }
            Self::Cancelled {
                owner,
                worker,
                workflow_type,
                workflow_id,
                run_id,
            } => {
                enc.u8(7);
                enc.str(owner);
                enc.str(worker);
                enc.str(workflow_type);
                enc.str(workflow_id);
                enc.str(run_id);
            }
            Self::Permissions(report) => {
                enc.u8(8);
                enc.str(&report.user);
                enc.bool(report.is_self);
                enc.opt_str(report.default_read.as_deref());
                enc.opt_str(report.default_write.as_deref());
                enc.u32(report.workers.len() as u32);
                for line in &report.workers {
                    enc.str(&line.worker);
                    enc.str(&line.package);
                    enc.str(&line.read);
                    enc.str(&line.write);
                }
            }
            Self::Error { code, message } => {
                enc.u8(9);
                enc.u8(code.tag());
                enc.str(message);
            }
            Self::Summary(summary) => {
                enc.u8(10);
                enc.opt_counts(summary.workflows);
                enc.bool(summary.draining);
            }
        }
        enc.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut dec = Decoder::new(bytes);
        let resp = match dec.u8()? {
            1 => {
                let n = dec.u32()? as usize;
                if n > 4096 {
                    return Err(Error::Decode("too many worker entries"));
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(WorkerEntry {
                        owner: dec.str()?,
                        worker: dec.str()?,
                        workflow_type: dec.str()?,
                        read: dec.bool()?,
                        write: dec.bool()?,
                        signature: dec.str()?,
                        description: dec.str()?,
                    });
                }
                Self::Workers(items)
            }
            2 => Self::Started {
                owner: dec.str()?,
                worker: dec.str()?,
                workflow_type: dec.str()?,
                workflow_id: dec.str()?,
                run_id: dec.str()?,
            },
            3 => {
                let n = dec.u32()? as usize;
                if n > 10000 {
                    return Err(Error::Decode("too many workflow entries"));
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(WorkflowSummary {
                        owner: dec.str()?,
                        worker: dec.str()?,
                        workflow_type: dec.str()?,
                        workflow_id: dec.str()?,
                        run_id: dec.str()?,
                        status: dec.str()?,
                    });
                }
                Self::List(items)
            }
            4 => Self::Status(decode_detail(&mut dec)?),
            5 => {
                let outcome = match dec.u8()? {
                    1 => ResultOutcome::Completed {
                        run_id: dec.str()?,
                        output: dec.str()?,
                    },
                    2 => ResultOutcome::Running { run_id: dec.str()? },
                    3 => ResultOutcome::Unsuccessful {
                        run_id: dec.str()?,
                        status: dec.str()?,
                        detail: dec.str()?,
                    },
                    _ => return Err(Error::Decode("unknown ResultOutcome tag")),
                };
                Self::WorkflowResult(outcome)
            }
            6 => Self::CancelPrepared(decode_detail(&mut dec)?),
            7 => Self::Cancelled {
                owner: dec.str()?,
                worker: dec.str()?,
                workflow_type: dec.str()?,
                workflow_id: dec.str()?,
                run_id: dec.str()?,
            },
            8 => {
                let user = dec.str()?;
                let is_self = dec.bool()?;
                let default_read = dec.opt_str()?;
                let default_write = dec.opt_str()?;
                let n = dec.u32()? as usize;
                if n > 4096 {
                    return Err(Error::Decode("too many permission entries"));
                }
                let mut workers = Vec::with_capacity(n);
                for _ in 0..n {
                    workers.push(WorkerPermissionLine {
                        worker: dec.str()?,
                        package: dec.str()?,
                        read: dec.str()?,
                        write: dec.str()?,
                    });
                }
                Self::Permissions(PermissionReport {
                    user,
                    is_self,
                    default_read,
                    default_write,
                    workers,
                })
            }
            9 => Self::Error {
                code: ErrorCode::from_tag(dec.u8()?)?,
                message: dec.str()?,
            },
            10 => Self::Summary(PromptSummary {
                workflows: dec.opt_counts()?,
                draining: dec.bool()?,
            }),
            _ => return Err(Error::Decode("unknown Response tag")),
        };
        dec.finish()?;
        Ok(resp)
    }
}

impl UpdaterRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::new();
        match self {
            Self::GateStatus => enc.u8(1),
            Self::CloseGate { update_id, rev } => {
                enc.u8(2);
                enc.str(update_id);
                enc.str(rev);
            }
            Self::CheckDrain { update_id } => {
                enc.u8(3);
                enc.str(update_id);
            }
            Self::ReopenGate { update_id } => {
                enc.u8(4);
                enc.str(update_id);
            }
        }
        enc.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut dec = Decoder::new(bytes);
        let req = match dec.u8()? {
            1 => Self::GateStatus,
            2 => Self::CloseGate {
                update_id: dec.str()?,
                rev: dec.str()?,
            },
            3 => Self::CheckDrain {
                update_id: dec.str()?,
            },
            4 => Self::ReopenGate {
                update_id: dec.str()?,
            },
            _ => return Err(Error::Decode("unknown UpdaterRequest tag")),
        };
        dec.finish()?;
        Ok(req)
    }
}

impl UpdaterResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut enc = Encoder::new();
        match self {
            Self::GateStatus {
                open,
                update_id,
                rev,
            } => {
                enc.u8(1);
                enc.bool(*open);
                enc.opt_str(update_id.as_deref());
                enc.opt_str(rev.as_deref());
            }
            Self::GateClosed => enc.u8(2),
            Self::DrainQuiescent => enc.u8(3),
            Self::DrainBusy {
                active_workflows,
                active_leases,
                detail,
            } => {
                enc.u8(4);
                enc.u32(*active_workflows);
                enc.u32(*active_leases);
                enc.str(detail);
            }
            Self::GateReopened => enc.u8(5),
            Self::Error(msg) => {
                enc.u8(6);
                enc.str(msg);
            }
        }
        enc.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut dec = Decoder::new(bytes);
        let resp = match dec.u8()? {
            1 => Self::GateStatus {
                open: dec.bool()?,
                update_id: dec.opt_str()?,
                rev: dec.opt_str()?,
            },
            2 => Self::GateClosed,
            3 => Self::DrainQuiescent,
            4 => Self::DrainBusy {
                active_workflows: dec.u32()?,
                active_leases: dec.u32()?,
                detail: dec.str()?,
            },
            5 => Self::GateReopened,
            6 => Self::Error(dec.str()?),
            _ => return Err(Error::Decode("unknown UpdaterResponse tag")),
        };
        dec.finish()?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_and_updater_ipc_frames_roundtrip() {
        let requests = [
            Request::Workers,
            Request::Start {
                owner: "alice".into(),
                worker: "greet".into(),
                id: "run-1".into(),
                args: vec!["Élan".into(), "5".into()],
            },
            Request::Cancel {
                owner: "alice".into(),
                id: "run-1".into(),
                run_id: "r1".into(),
            },
            Request::Permissions { user: None },
            Request::Summary,
        ];
        for req in requests {
            let mut pipe = Vec::new();
            write_frame(&mut pipe, &req.encode()).unwrap();
            let raw = read_frame(&mut pipe.as_slice()).unwrap();
            assert_eq!(Request::decode(&raw).unwrap(), req);
        }

        let responses = [
            Response::Started {
                owner: "alice".into(),
                worker: "greet".into(),
                workflow_type: "DemoWorkflow".into(),
                workflow_id: "run-1".into(),
                run_id: "uuid".into(),
            },
            Response::WorkflowResult(ResultOutcome::Completed {
                run_id: "uuid".into(),
                output: "Hello!".into(),
            }),
            Response::Error {
                code: ErrorCode::PermissionDenied,
                message: "denied".into(),
            },
            Response::Summary(PromptSummary {
                workflows: Some(WorkflowCounts {
                    paused: 1,
                    running: 3,
                }),
                draining: true,
            }),
            Response::Summary(PromptSummary {
                workflows: None,
                draining: false,
            }),
        ];
        for resp in responses {
            assert_eq!(Response::decode(&resp.encode()).unwrap(), resp);
        }

        let ureq = UpdaterRequest::CloseGate {
            update_id: "u1".into(),
            rev: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        assert_eq!(UpdaterRequest::decode(&ureq.encode()).unwrap(), ureq);
        let uresp = UpdaterResponse::DrainBusy {
            active_workflows: 2,
            active_leases: 1,
            detail: "waiting".into(),
        };
        assert_eq!(UpdaterResponse::decode(&uresp.encode()).unwrap(), uresp);
    }
}
