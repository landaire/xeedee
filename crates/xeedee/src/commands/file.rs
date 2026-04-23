//! Streaming file transfer.
//!
//! XBDM exposes two shapes of `getfile`:
//!
//! - Whole-file: `getfile NAME="..."`. Replies `203- binary response follows`
//!   then a 4-byte little-endian length prefix, then that many payload bytes.
//! - Ranged: `getfile NAME="..." OFFSET=n SIZE=m`. Replies `203- binary
//!   response follows` then exactly `m` payload bytes (no length prefix).
//!
//! After the 203 line we are mid-transfer on the same TCP stream; no other
//! commands can be issued until the payload is drained. That invariant is
//! enforced by [`FileDownload`] borrowing the transport mutably from the
//! client until it is fully consumed.

use futures_io::AsyncRead;
use futures_io::AsyncWrite;
use futures_util::io::AsyncReadExt;
use futures_util::io::AsyncWriteExt;
use rootcause::prelude::*;
use std::io;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use crate::client::Client;
use crate::client::Connected;
use crate::error::Error;
use crate::error::FramingError;
use crate::protocol::ArgBuilder;
use crate::protocol::Response;
use crate::protocol::SuccessCode;
use crate::protocol::framing::LineBuffer;
use crate::protocol::response::read_response;

/// Maximum file size we'll consume in a single prefix-length getfile.
/// XBDM on-device caps payloads around ~5 MiB per range; this bound just
/// catches bogus length prefixes from corrupted streams.
pub const MAX_PREFIX_LENGTH: u64 = 1 << 32;

/// Active getfile transfer. Implements [`AsyncRead`] so callers can pipe it
/// into any [`AsyncWrite`] via `futures_util::io::copy`, or consume it
/// chunk-by-chunk by hand. Drops leave the underlying client in an
/// unreliable state if `remaining() > 0`; prefer one of the convenience
/// consumers unless you know you'll drain the stream yourself.
#[derive(Debug)]
pub struct FileDownload<'a, T> {
    transport: &'a mut T,
    scratch: &'a mut LineBuffer,
    total: u64,
    read_so_far: u64,
}

impl<'a, T> FileDownload<'a, T>
where
    T: AsyncRead + Unpin,
{
    fn new(transport: &'a mut T, scratch: &'a mut LineBuffer, total: u64) -> Self {
        Self {
            transport,
            scratch,
            total,
            read_so_far: 0,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn remaining(&self) -> u64 {
        self.total - self.read_so_far
    }

    pub fn is_exhausted(&self) -> bool {
        self.remaining() == 0
    }

    /// Drain the rest of the payload into a `Vec`.
    pub async fn into_vec(mut self) -> Result<Vec<u8>, rootcause::Report<Error>> {
        let mut buf = Vec::with_capacity(self.total as usize);
        self.read_to_end(&mut buf)
            .await
            .map_err(Error::from)
            .into_report()
            .attach("draining getfile payload into memory")?;
        Ok(buf)
    }

    /// Drain the rest of the payload into any `AsyncWrite`. Returns the
    /// number of bytes written.
    pub async fn copy_into<W>(mut self, writer: &mut W) -> Result<u64, rootcause::Report<Error>>
    where
        W: AsyncWrite + Unpin,
    {
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0u64;
        while !self.is_exhausted() {
            let n = self
                .read(&mut buf)
                .await
                .map_err(Error::from)
                .into_report()
                .attach("reading getfile payload chunk")?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .await
                .map_err(Error::from)
                .into_report()
                .attach("writing getfile payload chunk to sink")?;
            total += n as u64;
        }
        writer
            .flush()
            .await
            .map_err(Error::from)
            .into_report()
            .attach("flushing getfile sink")?;
        Ok(total)
    }
}

impl<'a, T: AsyncRead + Unpin> AsyncRead for FileDownload<'a, T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Poll::Ready(Ok(0));
        }
        let cap = core::cmp::min(buf.len() as u64, remaining) as usize;
        let slice = &mut buf[..cap];

        let this = &mut *self;
        // Before issuing a fresh read, if the line buffer contains leftover
        // bytes from the previous response read, drain those first. This
        // would be rare in practice: the 203 line has already been parsed,
        // but any byte captured after the CRLF is part of our payload.
        if !this.scratch.as_bytes().is_empty() {
            let leftover = this.scratch.as_bytes();
            let copy_len = core::cmp::min(leftover.len(), slice.len());
            slice[..copy_len].copy_from_slice(&leftover[..copy_len]);
            this.scratch.buf.drain(..copy_len);
            this.read_so_far += copy_len as u64;
            return Poll::Ready(Ok(copy_len));
        }
        let pinned = Pin::new(&mut *this.transport);
        match pinned.poll_read(cx, slice) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed mid-getfile payload",
            ))),
            Poll::Ready(Ok(n)) => {
                this.read_so_far += n as u64;
                Poll::Ready(Ok(n))
            }
        }
    }
}

/// Active sendfile/writefile transfer. Implements [`AsyncWrite`] so
/// callers can pipe any [`AsyncRead`] into it via `futures_util::io::copy`.
///
/// The upload is strict about byte counts: the caller must write exactly
/// `declared()` bytes and then call [`FileUpload::finish`] to drain the
/// server's success response. Dropping with bytes pending leaves the
/// connection in a broken state, so prefer the convenience consumers.
#[derive(Debug)]
pub struct FileUpload<'a, T> {
    transport: &'a mut T,
    scratch: &'a mut LineBuffer,
    declared: u64,
    sent_so_far: u64,
}

impl<'a, T> FileUpload<'a, T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn new(transport: &'a mut T, scratch: &'a mut LineBuffer, declared: u64) -> Self {
        Self {
            transport,
            scratch,
            declared,
            sent_so_far: 0,
        }
    }

    pub fn declared(&self) -> u64 {
        self.declared
    }

    pub fn sent(&self) -> u64 {
        self.sent_so_far
    }

    pub fn remaining(&self) -> u64 {
        self.declared - self.sent_so_far
    }

    /// Stream `reader`'s full contents into the upload (up to
    /// `declared()` bytes) and finalize.
    pub async fn copy_from<R>(mut self, reader: &mut R) -> Result<(), rootcause::Report<Error>>
    where
        R: AsyncRead + Unpin,
    {
        let mut buf = [0u8; 64 * 1024];
        while self.remaining() > 0 {
            let cap = core::cmp::min(buf.len() as u64, self.remaining()) as usize;
            let n = reader
                .read(&mut buf[..cap])
                .await
                .map_err(Error::from)
                .into_report()
                .attach("reading source for sendfile")?;
            if n == 0 {
                let msg = format!(
                    "source ran out after {} bytes but {} were declared",
                    self.sent_so_far, self.declared
                );
                return Err(rootcause::Report::new(Error::from(
                    FramingError::TrailingGarbageInHead,
                ))
                .attach(msg));
            }
            self.write_all(&buf[..n])
                .await
                .map_err(Error::from)
                .into_report()
                .attach("sending binary chunk to console")?;
        }
        self.finish().await
    }

    /// Upload an in-memory buffer and finalize. The buffer length must
    /// match `declared()`.
    pub async fn send_all(mut self, payload: &[u8]) -> Result<(), rootcause::Report<Error>> {
        if payload.len() as u64 != self.declared {
            let msg = format!(
                "payload length {} != declared {}",
                payload.len(),
                self.declared
            );
            return Err(
                rootcause::Report::new(Error::from(FramingError::TrailingGarbageInHead))
                    .attach(msg),
            );
        }
        self.write_all(payload)
            .await
            .map_err(Error::from)
            .into_report()
            .attach("writing send_all payload")?;
        self.finish().await
    }

    /// Drain any pending bytes on the wire and read the success response.
    /// Call after having pushed exactly `declared()` bytes via
    /// [`AsyncWrite`].
    pub async fn finish(self) -> Result<(), rootcause::Report<Error>> {
        if self.remaining() != 0 {
            let msg = format!(
                "upload finish called with {} bytes still to send",
                self.remaining()
            );
            return Err(
                rootcause::Report::new(Error::from(FramingError::TrailingGarbageInHead))
                    .attach(msg),
            );
        }
        self.transport
            .flush()
            .await
            .map_err(Error::from)
            .into_report()
            .attach("flushing final upload chunk")?;
        let response = read_response(self.transport, self.scratch, None).await?;
        match response {
            Response::Line {
                code: SuccessCode::Ok,
                ..
            } => Ok(()),
            other => Err(
                rootcause::Report::new(Error::from(FramingError::HeadTooShort))
                    .attach(format!("expected 200 OK after upload, got {other:?}")),
            ),
        }
    }
}

impl<'a, T: AsyncWrite + Unpin> AsyncWrite for FileUpload<'a, T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let remaining = this.declared - this.sent_so_far;
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "FileUpload declared length fully satisfied",
            )));
        }
        let cap = core::cmp::min(buf.len() as u64, remaining) as usize;
        let slice = &buf[..cap];
        let pinned = Pin::new(&mut *this.transport);
        match pinned.poll_write(cx, slice) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(n)) => {
                this.sent_so_far += n as u64;
                Poll::Ready(Ok(n))
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.transport).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.transport).poll_close(cx)
    }
}

/// Argument to [`Client::get_file`] describing whether to read the whole
/// file or a specific byte range.
#[derive(Debug, Clone, Copy)]
pub enum GetFileRange {
    WholeFile,
    Range { offset: u64, size: u64 },
}

impl<T> Client<T, Connected>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Start a getfile transfer. Returns an [`FileDownload`] that borrows
    /// the client's transport; callers must fully drain the download
    /// before issuing further commands on this connection.
    pub async fn get_file<'a>(
        &'a mut self,
        path: &str,
        range: GetFileRange,
    ) -> Result<FileDownload<'a, T>, rootcause::Report<Error>> {
        let mut line = ArgBuilder::new("getfile")
            .quoted("NAME", path)
            .map_err(|e| rootcause::Report::new(Error::from(e)))?;
        let expected_size = if let GetFileRange::Range { offset, size } = range {
            line = line.dec("OFFSET", offset).dec("SIZE", size);
            Some(size)
        } else {
            None
        };
        let wire = line.finish();

        let mut framed = wire;
        framed.push_str("\r\n");
        {
            let (transport, _scratch) = self.transport_and_scratch();
            transport
                .write_all(framed.as_bytes())
                .await
                .map_err(Error::from)
                .into_report()
                .attach("sending getfile command")?;
            transport
                .flush()
                .await
                .map_err(Error::from)
                .into_report()
                .attach("flushing getfile command")?;
        }

        {
            let (transport, scratch) = self.transport_and_scratch();
            let response = read_response(transport, scratch, None).await?;
            match response {
                Response::Binary { .. } => {}
                Response::Line { code, .. } => {
                    return Err(rootcause::Report::new(Error::UnexpectedSuccessCode {
                        expected: SuccessCode::BinaryFollows,
                        got: code,
                    }));
                }
                other => {
                    return Err(
                        rootcause::Report::new(Error::from(FramingError::HeadTooShort))
                            .attach(format!("expected 203 binary follows, got {other:?}")),
                    );
                }
            }
        }

        // Both forms of getfile emit a 4-byte LE length prefix after the
        // 203 line. For the ranged form we cross-check against the SIZE
        // we asked for and surface a mismatch as a framing error.
        let advertised = {
            let (transport, scratch) = self.transport_and_scratch();
            read_length_prefix(transport, scratch).await?
        };
        if let Some(requested) = expected_size
            && advertised != requested
        {
            return Err(
                    rootcause::Report::new(Error::from(FramingError::TrailingGarbageInHead))
                        .attach(format!(
                            "ranged getfile requested {requested} bytes but server advertised {advertised}"
                        )),
                );
        }
        let (transport, scratch) = self.transport_and_scratch();
        Ok(FileDownload::new(transport, scratch, advertised))
    }
}

/// Mode for an outbound file upload. `Create` (sendfile) wipes any
/// existing file and writes the full `size` from offset 0. `WriteAt`
/// (writefile) writes exactly `size` bytes starting at `offset` within an
/// existing file.
#[derive(Debug, Clone, Copy)]
pub enum FileUploadKind {
    Create { size: u64 },
    WriteAt { offset: u64, size: u64 },
}

impl FileUploadKind {
    pub fn size(&self) -> u64 {
        match self {
            FileUploadKind::Create { size } | FileUploadKind::WriteAt { size, .. } => *size,
        }
    }
}

impl<T> Client<T, Connected>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Begin a streaming upload to the console. The returned
    /// [`FileUpload`] is an `AsyncWrite` that accepts exactly `size` (or
    /// `size_at_offset`) bytes before being finalized via `.finish()`.
    pub async fn send_file<'a>(
        &'a mut self,
        path: &str,
        kind: FileUploadKind,
    ) -> Result<FileUpload<'a, T>, rootcause::Report<Error>> {
        let line = match kind {
            FileUploadKind::Create { size } => ArgBuilder::new("sendfile")
                .quoted("NAME", path)
                .map_err(|e| rootcause::Report::new(Error::from(e)))?
                .hex("LENGTH", size)
                .finish(),
            FileUploadKind::WriteAt { offset, size } => ArgBuilder::new("writefile")
                .quoted("NAME", path)
                .map_err(|e| rootcause::Report::new(Error::from(e)))?
                .dec("OFFSET", offset)
                .dec("LENGTH", size)
                .finish(),
        };

        let mut framed = line;
        framed.push_str("\r\n");
        {
            let (transport, _scratch) = self.transport_and_scratch();
            transport
                .write_all(framed.as_bytes())
                .await
                .map_err(Error::from)
                .into_report()
                .attach("sending upload command")?;
            transport
                .flush()
                .await
                .map_err(Error::from)
                .into_report()
                .attach("flushing upload command")?;
        }

        {
            let (transport, scratch) = self.transport_and_scratch();
            let response = read_response(transport, scratch, None).await?;
            match response {
                Response::SendBinary { .. } => {}
                Response::Line {
                    code: SuccessCode::Ok,
                    head,
                } => {
                    return Err(
                        rootcause::Report::new(Error::from(FramingError::HeadTooShort)).attach(
                            format!("expected 204 send-binary but got 200 OK ({head:?})"),
                        ),
                    );
                }
                other => {
                    return Err(
                        rootcause::Report::new(Error::from(FramingError::HeadTooShort))
                            .attach(format!("expected 204 send-binary, got {other:?}")),
                    );
                }
            }
        }

        let (transport, scratch) = self.transport_and_scratch();
        Ok(FileUpload::new(transport, scratch, kind.size()))
    }
}

async fn read_length_prefix<R>(
    reader: &mut R,
    scratch: &mut LineBuffer,
) -> Result<u64, rootcause::Report<Error>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    let mut filled = 0usize;

    if !scratch.as_bytes().is_empty() {
        let leftover = scratch.as_bytes();
        let take = core::cmp::min(leftover.len(), prefix.len());
        prefix[..take].copy_from_slice(&leftover[..take]);
        scratch.buf.drain(..take);
        filled = take;
    }
    while filled < prefix.len() {
        let n = reader
            .read(&mut prefix[filled..])
            .await
            .map_err(Error::from)
            .into_report()
            .attach("reading getfile length prefix")?;
        if n == 0 {
            return Err(rootcause::Report::new(Error::ConnectionClosed));
        }
        filled += n;
    }
    let length = u32::from_le_bytes(prefix) as u64;
    if length > MAX_PREFIX_LENGTH {
        return Err(
            rootcause::Report::new(Error::from(FramingError::TrailingGarbageInHead))
                .attach(format!("refusing {length}-byte getfile payload")),
        );
    }
    Ok(length)
}
