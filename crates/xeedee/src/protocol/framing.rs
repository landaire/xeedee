use futures_util::io::AsyncRead;
use futures_util::io::AsyncReadExt;
use rootcause::prelude::*;

use crate::error::Error;
use crate::error::FramingError;

/// Maximum length of a single protocol line we accept. XBDM text lines are
/// short in practice; anything longer is almost certainly a desynced stream.
pub const MAX_LINE_LEN: usize = 16 * 1024;

/// Accumulating buffer for line-oriented reads.
///
/// Holds bytes read from the wire that haven't yet been consumed as a
/// complete line. Exposed as an opaque struct so we can change the internal
/// representation (e.g. `bytes::BytesMut`) later without source breakage.
#[derive(Debug, Default)]
pub struct LineBuffer {
    pub(crate) buf: Vec<u8>,
}

impl LineBuffer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Read a single CRLF-terminated line from `reader`, stripping the `\r\n`.
///
/// Bytes already in `scratch` are consumed first; any bytes past the
/// terminator are retained in `scratch` for subsequent reads.
pub async fn read_line<R>(
    reader: &mut R,
    scratch: &mut LineBuffer,
) -> Result<String, rootcause::Report<Error>>
where
    R: AsyncRead + Unpin,
{
    let mut tmp = [0u8; 512];
    loop {
        if let Some(pos) = memchr::memchr(b'\n', &scratch.buf) {
            let line_end = if pos > 0 && scratch.buf[pos - 1] == b'\r' {
                pos - 1
            } else {
                pos
            };
            let line = String::from_utf8(scratch.buf[..line_end].to_vec())
                .map_err(|_| rootcause::Report::new(Error::from(FramingError::NonUtf8Line)))?;
            scratch.buf.drain(..=pos);
            return Ok(line);
        }

        if scratch.buf.len() > MAX_LINE_LEN {
            return Err(rootcause::Report::new(Error::from(
                FramingError::LineTooLong,
            )));
        }

        let n = reader
            .read(&mut tmp)
            .await
            .map_err(Error::from)
            .into_report()?;
        if n == 0 {
            return Err(rootcause::Report::new(Error::ConnectionClosed));
        }
        scratch.buf.extend_from_slice(&tmp[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::io::Cursor;

    #[test]
    fn reads_single_line() {
        let mut reader = Cursor::new(b"200- OK\r\nleftover".to_vec());
        let mut buf = LineBuffer::new();
        let line = futures::executor::block_on(read_line(&mut reader, &mut buf)).unwrap();
        assert_eq!(line, "200- OK");
        assert_eq!(buf.as_bytes(), b"leftover");
    }

    #[test]
    fn reports_connection_closed_on_partial_line() {
        let mut reader = Cursor::new(b"200- partial".to_vec());
        let mut buf = LineBuffer::new();
        let err = futures::executor::block_on(read_line(&mut reader, &mut buf)).unwrap_err();
        assert!(matches!(err.current_context(), Error::ConnectionClosed));
    }

    #[test]
    fn handles_bare_lf() {
        let mut reader = Cursor::new(b"ok\n".to_vec());
        let mut buf = LineBuffer::new();
        let line = futures::executor::block_on(read_line(&mut reader, &mut buf)).unwrap();
        assert_eq!(line, "ok");
    }
}
