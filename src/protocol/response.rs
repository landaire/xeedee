use futures_util::io::AsyncRead;
use futures_util::io::AsyncReadExt;
use rootcause::prelude::*;

use crate::error::Error;
use crate::error::ExpectedShape;
use crate::error::FramingError;
use crate::protocol::framing::LineBuffer;
use crate::protocol::framing::read_line;
use crate::protocol::parse::response_head;
use crate::protocol::parse::run_framing;
use crate::protocol::status::Classified;
use crate::protocol::status::StatusCode;
use crate::protocol::status::SuccessCode;
use crate::protocol::status::parse_status;

/// The parsed head of a response line (the `NNN[ -]rest` portion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub code: StatusCode,
    pub rest: String,
}

/// A fully parsed response.
#[derive(Debug, Clone)]
pub enum Response {
    Line { code: SuccessCode, head: String },
    Multiline { head: String, lines: Vec<String> },
    Binary { head: String, data: Vec<u8> },
    SendBinary { head: String },
}

/// Parse a `NNN[- ]rest` header appearing at the start of every response.
pub fn parse_response_head(line: &str) -> Result<ResponseHead, Error> {
    let _ = parse_status(line)?;
    let (code, rest) = run_framing(response_head, line, FramingError::TrailingGarbageInHead)?;
    let code = StatusCode::new(code).ok_or(FramingError::StatusOutOfRange)?;
    Ok(ResponseHead {
        code,
        rest: rest.to_owned(),
    })
}

/// Read a single response from `reader`, handling multi-line and binary
/// follow-ups. Binary payloads are only collected when `binary_len` is given;
/// otherwise a `203` response returns with an empty data buffer and the
/// caller is expected to drain the bytes itself.
pub async fn read_response<R>(
    reader: &mut R,
    scratch: &mut LineBuffer,
    binary_len: Option<usize>,
) -> Result<Response, rootcause::Report<Error>>
where
    R: AsyncRead + Unpin,
{
    let head_line = read_line(reader, scratch).await?;
    let head = parse_response_head(&head_line).map_err(rootcause::Report::new)?;

    match head.code.try_classify() {
        Classified::Error(code) => Err(rootcause::Report::new(Error::Remote {
            code,
            message: head.rest.clone(),
        })
        .attach(format!("wire line: {head_line:?}"))),
        Classified::Unknown(code) => {
            Err(
                rootcause::Report::new(Error::UnknownStatusCode { raw: code.raw() })
                    .attach(format!("wire line: {head_line:?}")),
            )
        }
        Classified::Success(SuccessCode::MultilineFollows) => {
            let mut lines = Vec::new();
            loop {
                let line = read_line(reader, scratch).await?;
                if line == "." {
                    break;
                }
                lines.push(line);
            }
            Ok(Response::Multiline {
                head: head.rest,
                lines,
            })
        }
        Classified::Success(SuccessCode::BinaryFollows) => {
            let mut data = Vec::new();
            if let Some(len) = binary_len {
                data.resize(len, 0);
                reader
                    .read_exact(&mut data)
                    .await
                    .map_err(Error::from)
                    .into_report()
                    .attach("reading binary response payload")?;
            }
            Ok(Response::Binary {
                head: head.rest,
                data,
            })
        }
        Classified::Success(SuccessCode::SendBinary) => {
            Ok(Response::SendBinary { head: head.rest })
        }
        Classified::Success(
            code @ (SuccessCode::Ok
            | SuccessCode::Connected
            | SuccessCode::Disconnecting
            | SuccessCode::Dedicated),
        ) => Ok(Response::Line {
            code,
            head: head.rest,
        }),
    }
}

impl Response {
    pub fn head(&self) -> &str {
        match self {
            Response::Line { head, .. }
            | Response::Multiline { head, .. }
            | Response::Binary { head, .. }
            | Response::SendBinary { head } => head,
        }
    }

    pub fn expect_ok(self) -> Result<String, Error> {
        match self {
            Response::Line {
                code: SuccessCode::Ok,
                head,
            } => Ok(head),
            Response::Line { code, .. } => Err(Error::UnexpectedSuccessCode {
                expected: SuccessCode::Ok,
                got: code,
            }),
            _ => Err(Error::UnexpectedStatus {
                expected: ExpectedShape::SingleLine200,
                got: StatusCode::new(200).unwrap(),
            }),
        }
    }

    pub fn expect_multiline(self) -> Result<Vec<String>, Error> {
        match self {
            Response::Multiline { lines, .. } => Ok(lines),
            _ => Err(Error::UnexpectedStatus {
                expected: ExpectedShape::Multiline202,
                got: StatusCode::new(202).unwrap(),
            }),
        }
    }

    pub fn expect_binary(self) -> Result<Vec<u8>, Error> {
        match self {
            Response::Binary { data, .. } => Ok(data),
            _ => Err(Error::UnexpectedStatus {
                expected: ExpectedShape::Binary203,
                got: StatusCode::new(203).unwrap(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_head() {
        let head = parse_response_head("200- OK").unwrap();
        assert_eq!(head.code, StatusCode::new(200).unwrap());
        assert_eq!(head.rest, "OK");
    }

    #[test]
    fn parses_connected_banner() {
        let head = parse_response_head("201- connected").unwrap();
        assert_eq!(head.code, StatusCode::new(201).unwrap());
        assert_eq!(head.rest, "connected");
    }

    #[test]
    fn parses_error_head() {
        let head = parse_response_head("407- unknown command").unwrap();
        assert_eq!(head.code, StatusCode::new(407).unwrap());
        assert_eq!(head.rest, "unknown command");
    }
}
