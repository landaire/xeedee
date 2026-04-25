//! Typed connection layer.
//!
//! [`Client`] is parameterised over a marker type describing the current
//! protocol phase. Fresh clients wrapping a raw transport have not read
//! the banner yet; only after [`Client::read_banner`] succeeds does the
//! client transition to [`Connected`], where commands may be issued.
//!
//! Further states (dedicated handlers, debugger attach, notification
//! channels) can be layered on by moving the `Client<Connected>` through
//! additional transitions that consume it and return a new parameterisation.
//!
//! The async [`Client`] in this module owns its transport and drives the
//! protocol via `futures_io::AsyncRead`/`AsyncWrite`. Callers that need
//! to drive the protocol from a blocking socket, an embedded transport,
//! or a WASM host can use [`engine::ClientEngine`] instead -- a separate,
//! socket-free state machine that exposes the same protocol events.

use futures_util::io::AsyncRead;
use futures_util::io::AsyncWrite;
use futures_util::io::AsyncWriteExt;
use rootcause::prelude::*;

use crate::error::Error;
use crate::error::FramingError;
use crate::error::TransportError;
use crate::protocol::Command;
use crate::protocol::ExpectedBody;
use crate::protocol::SuccessCode;
use crate::protocol::framing::LineBuffer;
use crate::protocol::response::Response;
use crate::protocol::response::read_response;

pub mod engine;
mod state;

pub use engine::ClientEngine;
pub use engine::ClientEvent;
pub use engine::SubmitError;
pub use state::Connected;
pub use state::Fresh;

/// A typed XBDM client.
///
/// `T` is the underlying transport (anything implementing both
/// `AsyncRead` and `AsyncWrite`); `S` is the protocol state marker.
#[derive(Debug)]
pub struct Client<T, S = Fresh> {
    transport: T,
    scratch: LineBuffer,
    _state: core::marker::PhantomData<S>,
}

impl<T> Client<T, Fresh>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Wrap a freshly opened transport. The caller still owes the banner
    /// read before commands may be issued.
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            scratch: LineBuffer::new(),
            _state: core::marker::PhantomData,
        }
    }

    /// Read the initial `201 connected` banner and transition to
    /// [`Connected`]. Any non-201 response is reported as a framing
    /// error.
    pub async fn read_banner(mut self) -> Result<Client<T, Connected>, rootcause::Report<Error>> {
        let response = read_response(&mut self.transport, &mut self.scratch, None).await?;
        match response {
            Response::Line {
                code: SuccessCode::Connected,
                ..
            } => Ok(Client {
                transport: self.transport,
                scratch: self.scratch,
                _state: core::marker::PhantomData,
            }),
            other => Err(
                rootcause::Report::new(Error::from(FramingError::MissingBanner))
                    .attach(format!("received {other:?} instead")),
            ),
        }
    }
}

impl<T> Client<T, Connected>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Execute a typed [`Command`], running it through the request/response
    /// pipeline and returning the command's parsed output.
    pub async fn run<C: Command>(&mut self, cmd: C) -> Result<C::Output, rootcause::Report<Error>> {
        let mut line = cmd.wire_line()?;
        tracing::debug!(wire = %line, "xbdm send");
        line.push_str("\r\n");
        self.transport
            .write_all(line.as_bytes())
            .await
            .map_err(Error::from)
            .into_report()
            .attach_with(|| format!("while sending command {:?}", line.trim_end()))?;
        self.transport
            .flush()
            .await
            .map_err(Error::from)
            .into_report()
            .attach("flushing command line")?;

        let binary_len = match cmd.expected() {
            ExpectedBody::Binary => Some(cmd.binary_len().ok_or_else(|| {
                rootcause::Report::new(Error::from(TransportError::MissingBinaryLen))
            })?),
            _ => None,
        };

        let response_result =
            read_response(&mut self.transport, &mut self.scratch, binary_len).await;
        let response = match response_result {
            Ok(response) => response,
            Err(report) => {
                if let Error::Remote { code, message } = report.current_context()
                    && let Some(mapped) = cmd.handle_remote(*code, message)
                {
                    return mapped;
                }
                return Err(report);
            }
        };
        tracing::debug!(?response, "xbdm recv");
        cmd.parse(response)
    }

    /// Send a raw command line. Useful for REPL-style exploration and
    /// commands we haven't modelled yet. Returns the raw parsed response
    /// so callers can decide what to do with it.
    pub async fn send_raw(&mut self, line: &str) -> Result<Response, rootcause::Report<Error>> {
        let mut framed = String::with_capacity(line.len() + 2);
        framed.push_str(line);
        framed.push_str("\r\n");
        self.transport
            .write_all(framed.as_bytes())
            .await
            .map_err(Error::from)
            .into_report()
            .attach_with(|| format!("sending raw line {line:?}"))?;
        self.transport
            .flush()
            .await
            .map_err(Error::from)
            .into_report()
            .attach("flushing raw command line")?;
        read_response(&mut self.transport, &mut self.scratch, None).await
    }

    /// Politely close the session with the XBDM `bye` command. The server
    /// replies with a status line and drops the socket.
    pub async fn bye(mut self) -> Result<(), rootcause::Report<Error>> {
        self.transport
            .write_all(b"bye\r\n")
            .await
            .map_err(Error::from)
            .into_report()
            .attach("sending bye")?;
        self.transport
            .flush()
            .await
            .map_err(Error::from)
            .into_report()
            .attach("flushing bye")?;
        match read_response(&mut self.transport, &mut self.scratch, None).await {
            Ok(_) => Ok(()),
            Err(report) if matches!(report.current_context(), Error::ConnectionClosed) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn into_inner(self) -> T {
        self.transport
    }

    /// Split the client into borrowed transport + scratch, used by
    /// streaming transfer adapters that need both simultaneously.
    pub(crate) fn transport_and_scratch(&mut self) -> (&mut T, &mut LineBuffer) {
        (&mut self.transport, &mut self.scratch)
    }
}
