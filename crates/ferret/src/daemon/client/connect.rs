//! Reaching the daemon: connecting to the index's endpoint, starting an
//! owner when none answers, and the hello handshake that checks its build
//! and capabilities.

use std::fs;
use std::io::{self, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::daemon::endpoint::{self, Endpoint};
use crate::daemon::{BUILD, FORMAT, MAJOR, duration};
use crate::protocol::Value;

use super::*;

pub(super) enum ConnectError {
    NoOwner(io::Error),
    Owner(io::Error),
    OwnerTimeout,
}
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOwner(error) | Self::Owner(error) => error.fmt(f),
            Self::OwnerTimeout => f.write_str("daemon owner did not become ready before the startup deadline; it may still be loading or draining; check ferret status --json and retry"),
        }
    }
}
pub(super) struct Connected {
    pub(super) reader: BufReader<UnixStream>,
    pub(super) raw_output: bool,
}
pub(super) fn connect(
    endpoint: &Endpoint,
    index: &Path,
    can_spawn: bool,
    writer: bool,
) -> Result<Connected, ConnectError> {
    let mut observed_owner = false;
    connect_owner(endpoint, index, can_spawn, writer, &mut observed_owner).map_err(|error| {
        if !observed_owner {
            ConnectError::NoOwner(error)
        } else if matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ) {
            ConnectError::OwnerTimeout
        } else {
            ConnectError::Owner(error)
        }
    })
}
fn connect_owner(
    endpoint: &Endpoint,
    index: &Path,
    can_spawn: bool,
    writer: bool,
    observed_owner: &mut bool,
) -> io::Result<Connected> {
    // The incarnation lives in the fixed-length manifest. Opening the catalog
    // also walks every change-log transaction, which a long-lived index
    // accumulates without bound.
    let incarnation = ferret_catalog::Catalog::published_generation(index)
        .ok()
        .flatten()
        .map(|generation| crate::find_json::hex(&generation.incarnation));
    let until = Instant::now() + duration("FERRET_DAEMON_STARTUP_MS", 10_000);
    let handshake = Handshake {
        endpoint,
        context: endpoint::context()?,
        incarnation,
        until,
        can_spawn,
        writer,
    };
    let mut draining_socket = None;
    while Instant::now() < handshake.until {
        if let Some(identity) = draining_socket {
            if socket_identity(endpoint) == Some(identity) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            // The leaving owner is gone: carry on as if it had never been
            // seen, so a writer with nobody listening runs in-process.
            *observed_owner = false;
        }
        match open_socket(endpoint) {
            Ok((stream, identity)) => {
                *observed_owner = true;
                match handshake.greet(stream, identity)? {
                    Greeting::Draining(identity) => draining_socket = identity,
                    Greeting::Ready(connected) => return Ok(connected),
                }
            }
            Err(error) if !can_spawn || *observed_owner && writer => return Err(error),
            Err(_) => {
                spawn(endpoint, index)?;
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "daemon started; using the in-process engine for this query",
                ));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon startup deadline",
    ))
}

/// The device and inode of the daemon's socket, if it has one.
fn socket_identity(endpoint: &Endpoint) -> Option<(u64, u64)> {
    fs::symlink_metadata(endpoint.socket())
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Connects to the daemon's socket, if it is one only this user can use,
/// with the socket's identity.
fn open_socket(endpoint: &Endpoint) -> io::Result<(UnixStream, (u64, u64))> {
    let metadata = fs::symlink_metadata(endpoint.socket())?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != endpoint::uid()?
        || metadata.mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unusable daemon socket",
        ));
    }
    let identity = (metadata.dev(), metadata.ino());
    UnixStream::connect(endpoint.socket()).map(|stream| (stream, identity))
}

/// What a client checks an owner's greeting against, until its deadline.
struct Handshake<'a> {
    endpoint: &'a Endpoint,
    context: String,
    /// The published catalog's incarnation, in hex.
    incarnation: Option<String>,
    until: Instant,
    can_spawn: bool,
    writer: bool,
}

/// What an owner's greeting leaves the client to do.
enum Greeting {
    /// Wait for the leaving owner's socket, with this identity, to go.
    Draining(Option<(u64, u64)>),
    Ready(Connected),
}

impl Handshake<'_> {
    /// Reads the owner's greeting on `stream`, the socket `identity`, and
    /// asks an owner that cannot answer to drain.
    fn greet(&self, stream: UnixStream, identity: (u64, u64)) -> io::Result<Greeting> {
        let writer = self.writer;
        stream.set_read_timeout(Some(
            self.until
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1)),
        ))?;
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        // A close before any hello is an owner that is exiting, draining or
        // full. A writer waits for its socket to go, as for a build-mismatch
        // drain: the daemon releases its catalog lock before removing the
        // socket (D110). Queries answer in-process at once instead.
        let mut hello = match event(&mut reader, &mut line) {
            Err(error)
                if writer
                    && matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
            {
                return Ok(Greeting::Draining(Some(identity)));
            }
            hello => hello?,
        };
        // Writer commands wait out loading on this connection, within the
        // startup deadline: answering in-process would race the daemon's
        // writer ownership.
        while writer && field_text(&hello, "state") == Some("loading") {
            hello = event(&mut reader, &mut line)?;
        }
        if !self.same_context(&hello) {
            return Err(io::Error::other("incompatible daemon context or protocol"));
        }
        if !self.can_serve(&hello) {
            if !self.can_spawn {
                return Err(io::Error::other(
                    "incompatible writer owner; use ferret status --json",
                ));
            }
            reader.get_mut().write_all(b"{\"op\":\"drain\"}\n")?;
            // Wait for the owning lock and socket to be released. Active
            // queries finish; the same deadline bounds fallback.
            return Ok(Greeting::Draining(socket_identity(self.endpoint)));
        }
        match field_text(&hello, "state") {
            Some("loading") => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "daemon is still loading; using the in-process engine",
            )),
            Some("ready") => {
                if let Some(incarnation) = &self.incarnation
                    && hello
                        .field("generation")
                        .and_then(|generation| field_text(generation, "incarnation"))
                        != Some(incarnation.as_str())
                {
                    return Err(io::Error::other("incompatible catalog incarnation"));
                }
                reader.get_ref().set_read_timeout(None)?;
                Ok(Greeting::Ready(Connected {
                    reader,
                    raw_output: has_capability(&hello, "raw-output-v1"),
                }))
            }
            _ => Err(io::Error::other("daemon could not open the catalog")),
        }
    }
    /// Whether `hello` is a hello from a daemon for this index and context,
    /// speaking this protocol's major version.
    fn same_context(&self, hello: &Value) -> bool {
        field_text(hello, "event") == Some("hello")
            && field_number(hello, "major") == Some(MAJOR)
            && field_text(hello, "index") == Some(&self.endpoint.identity)
            && field_text(hello, "context") == Some(&self.context)
    }
    /// Whether the daemon behind `hello` is this build, on this format, and
    /// can answer this client's queries and, for a writer, its commands.
    fn can_serve(&self, hello: &Value) -> bool {
        field_text(hello, "build") == Some(BUILD)
            && field_number(hello, "format") == Some(FORMAT)
            && supports_queries(hello)
            && (!self.writer || has_capability(hello, "writer"))
    }
}
fn has_capability(hello: &Value, capability: &str) -> bool {
    matches!(hello.field("capabilities"), Some(Value::Arr(values)) if values.iter().any(|v| v.text() == Some(capability)))
}
fn supports_queries(hello: &Value) -> bool {
    let Some(Value::Arr(capabilities)) = hello.field("capabilities") else {
        return false;
    };
    ["query-only", "cancel", "drain"].iter().all(|required| {
        capabilities
            .iter()
            .any(|value| value.text() == Some(*required))
    })
}
