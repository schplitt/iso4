//! The engine's two faces toward a frontend: [`HostLink`] carries a run's
//! answer and its bridge traffic, [`Observer`] is the side channel for what
//! is not part of the answer. The engine never sees a socket or a frame.

use std::fmt;

use crate::types::{BridgeErrorPayload, BridgeTarget, RunCompletion, TerminateMode};
use crate::v8::GraceReport;

/// Who a message belongs to. Step 1 addresses runs only; long-lived
/// objects join later without changing the trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Addr {
    /// A run, by the wire id the frontend gave it (0 = no wire identity).
    Run(u32),
}

/// Engine → host. Each variant maps to one frame today; the frontend
/// encodes it and knows who answers.
#[derive(Debug)]
pub enum Outbound {
    /// A bridge stub was called: the frontend writes its own header plus
    /// `args` in one vectored write, so the serializer's buffer is never
    /// copied.
    BridgeCall {
        call_id: u32,
        target: BridgeTarget,
        args: Vec<u8>,
    },
    /// Credit for an inbound streamed body the sandbox consumed from.
    StreamPull { stream_id: u32, credit: u32 },
    /// The sandbox dropped an inbound streamed body.
    StreamCancel { stream_id: u32, reason: String },
    /// One chunk of an outbound (sandbox → host) streamed body.
    StreamChunk { stream_id: u32, data: Vec<u8> },
    /// End of an outbound streamed body: clean, or the guest source failed.
    StreamEnd {
        stream_id: u32,
        error: Option<String>,
    },
    /// The run's result (final, or early when background work follows).
    Result { completion: RunCompletion },
    /// The grace phase's report after an early `Result`.
    RunComplete { report: GraceReport },
}

/// Host → engine, already decoded; the frontend routed it to the run.
#[derive(Debug)]
pub enum Inbound {
    /// The host's answer to a `BridgeCall`: the value blob (`None` = the
    /// handler returned nothing) or the handler's error.
    BridgeResponse {
        call_id: u32,
        result: Result<Option<Vec<u8>>, BridgeErrorPayload>,
    },
    StreamChunk {
        stream_id: u32,
        data: Vec<u8>,
    },
    StreamEnd {
        stream_id: u32,
        error: Option<String>,
    },
    StreamPull {
        stream_id: u32,
        credit: u32,
    },
    StreamCancel {
        stream_id: u32,
        reason: String,
    },
    Terminate {
        mode: TerminateMode,
    },
    /// The run's connection is gone. The detail lands in the run's failure.
    ConnectionLost {
        detail: Option<String>,
    },
}

impl fmt::Display for Inbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Inbound::BridgeResponse { .. } => "BridgeResponse",
            Inbound::StreamChunk { .. } => "StreamChunk",
            Inbound::StreamEnd { .. } => "StreamEnd",
            Inbound::StreamPull { .. } => "StreamPull",
            Inbound::StreamCancel { .. } => "StreamCancel",
            Inbound::Terminate { .. } => "Terminate",
            Inbound::ConnectionLost { .. } => "ConnectionLost",
        })
    }
}

/// Why a send did not reach the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// The message exceeds what the transport can carry; the sender may
    /// substitute a smaller one.
    TooLarge(String),
    /// The host is gone or stopped draining.
    Closed(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::TooLarge(detail) | LinkError::Closed(detail) => f.write_str(detail),
        }
    }
}

/// Implemented by each frontend. Called from the isolate thread and from
/// the frontend's own threads; must not block beyond the frontend's own
/// outbox discipline.
pub trait HostLink: Send + Sync {
    fn send(&self, to: Addr, msg: Outbound) -> Result<(), LinkError>;
}

/// Which console stream a line went to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleStream {
    Stdout,
    Stderr,
}

/// Side channel, not part of the answer: console lines as they happen.
/// Called on the isolate thread, so an impl must not block. Every method
/// is a no-op by default.
pub trait Observer: Send + Sync {
    /// A console line the run just wrote, before the run's own caps apply;
    /// the result still carries the capped buffer.
    fn console(&self, _to: Addr, _stream: ConsoleStream, _line: &str) {}
}

/// The observer that watches nothing.
pub struct NoopObserver;

impl Observer for NoopObserver {}

/// A link that queues every message for the test to read.
#[cfg(test)]
pub(crate) struct ChannelLink(pub(crate) crossbeam_channel::Sender<(Addr, Outbound)>);

#[cfg(test)]
impl HostLink for ChannelLink {
    fn send(&self, to: Addr, msg: Outbound) -> Result<(), LinkError> {
        self.0
            .send((to, msg))
            .map_err(|_| LinkError::Closed("test host gone".to_string()))
    }
}

/// A [`ChannelLink`] and the receiving end.
#[cfg(test)]
pub(crate) fn channel_link() -> (
    std::sync::Arc<dyn HostLink>,
    crossbeam_channel::Receiver<(Addr, Outbound)>,
) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (std::sync::Arc::new(ChannelLink(tx)), rx)
}
