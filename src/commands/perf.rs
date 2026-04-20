//! Performance-counter and socket-info probes.

use crate::commands::kv::KvLine;
use crate::commands::kv::parse_kv_line;
use crate::commands::kv::value_u32;
use crate::error::ArgumentError;
use crate::error::Error;
use crate::error::ParseError;
use crate::protocol::ArgBuilder;
use crate::protocol::Command;
use crate::protocol::ExpectedBody;
use crate::protocol::Response;

fn report_parse(err: ParseError) -> rootcause::Report<Error> {
    rootcause::Report::new(Error::from(err))
}

fn require_u32(kv: &KvLine<'_>, key: &'static str) -> Result<u32, ParseError> {
    value_u32(kv.require(key)?, key)
}

fn hi_lo_u64(kv: &KvLine<'_>, hi: &'static str, lo: &'static str) -> Result<u64, ParseError> {
    let hi_val = require_u32(kv, hi)?;
    let lo_val = require_u32(kv, lo)?;
    Ok(((hi_val as u64) << 32) | (lo_val as u64))
}

/// `pclist`: enumerate the performance counters and memory pools the
/// kernel exposes.
#[derive(Debug, Clone, Copy)]
pub struct PerfCounterList;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerfCounterEntry {
    pub name: String,
    pub kind: u32,
}

impl Command for PerfCounterList {
    type Output = Vec<PerfCounterEntry>;

    fn wire_line(&self) -> Result<String, rootcause::Report<Error>> {
        Ok("pclist".to_owned())
    }

    fn expected(&self) -> ExpectedBody {
        ExpectedBody::Multiline
    }

    fn parse(&self, response: Response) -> Result<Self::Output, rootcause::Report<Error>> {
        let lines = response
            .expect_multiline()
            .map_err(rootcause::Report::new)?;
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            let kv = parse_kv_line(&line);
            out.push(PerfCounterEntry {
                name: kv
                    .require("name")
                    .map_err(report_parse)?
                    .as_str()
                    .to_owned(),
                kind: require_u32(&kv, "type").map_err(report_parse)?,
            });
        }
        Ok(out)
    }
}

/// `querypc NAME="<counter>" TYPE=<kind>`: snapshot a single performance
/// counter's value and rate.
#[derive(Debug, Clone)]
pub struct QueryPerfCounter {
    pub name: String,
    pub kind: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerfCounterSample {
    pub kind: u32,
    pub value: u64,
    pub rate: u64,
}

impl Command for QueryPerfCounter {
    type Output = PerfCounterSample;

    fn wire_line(&self) -> Result<String, rootcause::Report<Error>> {
        if self.name.is_empty() {
            return Err(
                rootcause::Report::new(Error::from(ArgumentError::EmptyFilename))
                    .attach("querypc NAME required"),
            );
        }
        let builder = ArgBuilder::new("querypc")
            .quoted("NAME", &self.name)
            .map_err(|e| rootcause::Report::new(Error::from(e)))?
            .hex32("TYPE", self.kind);
        Ok(builder.finish())
    }

    fn expected(&self) -> ExpectedBody {
        ExpectedBody::Multiline
    }

    fn parse(&self, response: Response) -> Result<Self::Output, rootcause::Report<Error>> {
        let mut lines = response
            .expect_multiline()
            .map_err(rootcause::Report::new)?;
        let head = lines
            .pop()
            .ok_or_else(|| report_parse(ParseError::UnrecognizedShape))?;
        let kv = parse_kv_line(&head);
        Ok(PerfCounterSample {
            kind: require_u32(&kv, "type").map_err(report_parse)?,
            value: hi_lo_u64(&kv, "valhi", "vallo").map_err(report_parse)?,
            rate: hi_lo_u64(&kv, "ratehi", "ratelo").map_err(report_parse)?,
        })
    }
}

/// `getsockinfo`: list of open sockets owned by XBDM.
#[derive(Debug, Clone, Copy)]
pub struct GetSocketInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketEntry {
    pub handle: u32,
    pub owner_type: u32,
    pub flags: u32,
    pub addr_family: u32,
    pub socket_type: u32,
    pub protocol: u32,
    pub local_addr: u32,
    pub remote_addr: u32,
    pub local_port: u16,
    pub remote_port: u16,
    pub tcp_state: u32,
}

impl Command for GetSocketInfo {
    type Output = Vec<SocketEntry>;

    fn wire_line(&self) -> Result<String, rootcause::Report<Error>> {
        Ok("getsockinfo".to_owned())
    }

    fn expected(&self) -> ExpectedBody {
        ExpectedBody::Multiline
    }

    fn parse(&self, response: Response) -> Result<Self::Output, rootcause::Report<Error>> {
        let lines = response
            .expect_multiline()
            .map_err(rootcause::Report::new)?;
        let mut out = Vec::with_capacity(lines.len());
        for line in lines {
            let kv = parse_kv_line(&line);
            out.push(SocketEntry {
                handle: require_u32(&kv, "handle").map_err(report_parse)?,
                owner_type: require_u32(&kv, "ownertype").map_err(report_parse)?,
                flags: require_u32(&kv, "flags").map_err(report_parse)?,
                addr_family: require_u32(&kv, "addrfamily").map_err(report_parse)?,
                socket_type: require_u32(&kv, "socktype").map_err(report_parse)?,
                protocol: require_u32(&kv, "protocol").map_err(report_parse)?,
                local_addr: require_u32(&kv, "localaddr").map_err(report_parse)?,
                remote_addr: require_u32(&kv, "remoteaddr").map_err(report_parse)?,
                local_port: require_u32(&kv, "localport").map_err(report_parse)? as u16,
                remote_port: require_u32(&kv, "remoteport").map_err(report_parse)? as u16,
                tcp_state: require_u32(&kv, "tcpstate").map_err(report_parse)?,
            });
        }
        Ok(out)
    }
}
