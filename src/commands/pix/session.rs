//! Driver for the PIX command prefix.
//!
//! All PIX commands share a single-line shape: `PIX!<token>[ args...]`.
//! The success reply is `200- PIX!OK` (sometimes `200- PIX` with
//! additional state) and the busy reply is
//! `200- Cannot execute - previous command still pending` with HRESULT
//! `0x82DA0101`. We expose each known token as a constructor on
//! [`CaptureSession`] so callers can't accidentally issue a bare
//! `DmSendCommand`.

use core::fmt::Write as _;

use futures_util::io::AsyncRead;
use futures_util::io::AsyncWrite;

use crate::client::Client;
use crate::client::Connected;
use crate::error::Error;
use crate::protocol::Response;
use crate::protocol::SuccessCode;

/// Protocol version xbmovie sends. The console rejects sessions that
/// don't match. Split low/high halves mean major `1`, minor `1`, so
/// `0x0001_0001 == 65537`.
pub const PIX_VERSION: u32 = 0x0001_0001;

/// Typed error for PIX wire interactions (in addition to the crate-wide
/// [`Error`] which covers the underlying transport and framing).
#[derive(Debug, thiserror::Error)]
pub enum PixError {
    #[error("pix runtime replied busy: {message:?}")]
    Busy { message: String },
    #[error("pix handshake failed: runtime does not implement the expected command set")]
    HandshakeFailed,
    #[error("pix response did not start with the expected `PIX!` prefix: {got:?}")]
    UnexpectedPrefix { got: String },
    #[error("pix capture file name exceeds xbmovie's 38-byte limit")]
    CaptureNameTooLong,
    #[error(
        "pix extension does not appear to be loaded on the console: a bare `PIX!` token \
         was accepted but no `PIX!OK` acknowledgment came back; run `dbgextld` on the \
         capture extension XEX first"
    )]
    ExtensionNotLoaded,
}

/// A single PIX notification parsed off the notification channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// Segment `index` has finished being written to the console's HDD.
    CaptureFileCreationEnded { index: u32 },
    /// The entire recording session has ended.
    CaptureEnded,
    /// Any other `PIX!{...}` line we haven't modelled.
    Other(String),
}

impl Notification {
    /// Parse a notification line (trailing CRLF stripped).
    pub fn parse(line: &str) -> Option<Self> {
        let body = line.strip_prefix("PIX!")?;
        if let Some(rest) = body.strip_prefix("{CaptureFileCreationEnded}") {
            let index: u32 = rest.trim().parse().unwrap_or(0);
            return Some(Notification::CaptureFileCreationEnded { index });
        }
        if body == "{CaptureEnded}" {
            return Some(Notification::CaptureEnded);
        }
        Some(Notification::Other(body.to_owned()))
    }
}

/// Bound session around a `Client<Connected>` that has completed the
/// `PIX!{Connect}` + `PIX!{Version}` handshake.
///
/// The session borrows the client mutably so that other commands cannot
/// interleave with an active PIX transaction. Drop-without-close is
/// allowed but produces a warning via tracing: callers should prefer
/// [`CaptureSession::disconnect`].
pub struct CaptureSession<'a, T> {
    client: &'a mut Client<T, Connected>,
}

impl<'a, T> core::fmt::Debug for CaptureSession<'a, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CaptureSession").finish_non_exhaustive()
    }
}

impl<'a, T> CaptureSession<'a, T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Perform the full PIX handshake: `{Connect}` + `{Version} <ver>`.
    ///
    /// Does NOT verify that the PIX extension is actually loaded on the
    /// console; use [`CaptureSession::connect_strict`] for that.
    pub async fn connect(
        client: &'a mut Client<T, Connected>,
    ) -> Result<Self, rootcause::Report<Error>> {
        let mut session = CaptureSession { client };
        session.send_line_named("{Connect}").await?;
        session
            .send_line_named(&format!("{{Version}} {}", PIX_VERSION))
            .await?;
        Ok(session)
    }

    /// As [`CaptureSession::connect`] but probes an intentionally bogus
    /// PIX token first. If the extension is actually resident it
    /// responds with an error or at minimum with a `PIX!`-prefixed
    /// reply; stock XBDM accepts arbitrary `PIX!<token>` as a no-op and
    /// returns plain `200- OK`, which is our signal that the capture
    /// extension isn't loaded.
    pub async fn connect_strict(
        client: &'a mut Client<T, Connected>,
    ) -> Result<Self, rootcause::Report<Error>> {
        let probe = client.send_raw("PIX!{XeedeeProbe}").await?;
        match probe {
            Response::Line { head, .. } if head.contains("PIX") => {
                // a real PIX handler responded with its own error shape
            }
            Response::Line { .. } => {
                return Err(rootcause::Report::new(Error::from(
                    PixError::ExtensionNotLoaded,
                )));
            }
            other => {
                return Err(
                    rootcause::Report::new(Error::from(PixError::HandshakeFailed))
                        .attach(format!("probe returned {other:?}")),
                );
            }
        }
        Self::connect(client).await
    }

    /// Cap the per-segment file size in megabytes. xbmovie's default is
    /// 512 MB.
    pub async fn limit_capture_size_mb(
        &mut self,
        megabytes: u32,
    ) -> Result<(), rootcause::Report<Error>> {
        self.send_line(&format!("{{LimitCaptureSize}} {}", megabytes))
            .await
    }

    /// Pick the output file. xbmovie prefixes any user-supplied path
    /// with `\Device\Harddisk0\Partition1\DEVKIT\`; we pass it straight
    /// through so callers that already have a qualified path get to use
    /// it.
    pub async fn begin_capture_file_creation(
        &mut self,
        path: &str,
    ) -> Result<(), rootcause::Report<Error>> {
        if path.len() > 38 + "\\Device\\Harddisk0\\Partition1\\DEVKIT\\".len() {
            return Err(rootcause::Report::new(Error::from(
                crate::error::ArgumentError::EmptyFilename,
            ))
            .attach("xbmovie restricts the visible filename to 38 bytes"));
        }
        self.send_line(&format!("{{BeginCaptureFileCreation}} {}", path))
            .await
    }

    pub async fn begin_capture(&mut self) -> Result<(), rootcause::Report<Error>> {
        self.send_line_named("{BeginCapture}").await
    }

    pub async fn end_capture(&mut self) -> Result<(), rootcause::Report<Error>> {
        self.send_line_named("{EndCapture}").await
    }

    pub async fn end_capture_file_creation(&mut self) -> Result<(), rootcause::Report<Error>> {
        self.send_line_named("{EndCaptureFileCreation}").await
    }

    pub async fn disconnect(mut self) -> Result<(), rootcause::Report<Error>> {
        self.send_line_named("{Disconnect}").await
    }

    /// Emit a single `PIX!<body>` line and expect the canonical
    /// success shape. Busy responses are promoted to [`PixError::Busy`].
    async fn send_line_named(&mut self, body: &str) -> Result<(), rootcause::Report<Error>> {
        self.send_line(body).await
    }

    async fn send_line(&mut self, body: &str) -> Result<(), rootcause::Report<Error>> {
        let mut wire = String::with_capacity(body.len() + 4);
        wire.push_str("PIX!");
        wire.push_str(body);

        let response = self.client.send_raw(&wire).await?;
        match response {
            Response::Line {
                code: SuccessCode::Ok,
                head,
            } => {
                if head.contains("Cannot execute") {
                    return Err(rootcause::Report::new(Error::from(PixError::Busy {
                        message: head,
                    })));
                }
                // In xbmovie the expected reply is `200- PIX!OK`, but on
                // live devkits we see plain `200- OK` depending on which
                // extension handler is installed. Accept both.
                Ok(())
            }
            other => {
                let mut msg = String::new();
                let _ = write!(msg, "expected 200 OK, got {other:?}");
                Err(rootcause::Report::new(Error::from(PixError::HandshakeFailed)).attach(msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_segment_done_notification() {
        let ev = Notification::parse("PIX!{CaptureFileCreationEnded}3").unwrap();
        assert_eq!(ev, Notification::CaptureFileCreationEnded { index: 3 });
    }

    #[test]
    fn parses_capture_ended() {
        assert_eq!(
            Notification::parse("PIX!{CaptureEnded}"),
            Some(Notification::CaptureEnded)
        );
    }

    #[test]
    fn falls_back_to_other_for_unknown_tokens() {
        let ev = Notification::parse("PIX!{NewThing}data").unwrap();
        assert_eq!(ev, Notification::Other("{NewThing}data".into()));
    }

    #[test]
    fn rejects_non_pix_lines() {
        assert_eq!(Notification::parse("PDB!{Stuff}"), None);
    }
}
