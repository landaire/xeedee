//! Tokio-backed runner for the XBDM NAP discovery protocol.
//!
//! Opens a UDP socket bound to a wildcard address with `SO_BROADCAST`
//! enabled, transmits a request packet to the broadcast destination, and
//! collects any `0x02` replies received within a per-call timeout.

use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::time::Duration;

use rootcause::prelude::*;
use tokio::net::UdpSocket;

use crate::discovery::wire::DiscoveredConsole;
use crate::discovery::wire::NAP_PORT;
use crate::discovery::wire::NapError;
use crate::discovery::wire::NapRequest;
use crate::discovery::wire::encode_request;
use crate::discovery::wire::parse_response;
use crate::error::Error;
use crate::error::TransportError;

/// Tunables for a single discovery call.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Destination address the request is sent to. Usually the IPv4
    /// broadcast address `255.255.255.255`, but can be a subnet-directed
    /// broadcast or a specific console IP for a unicast probe.
    pub destination: SocketAddr,
    /// How long we continue listening for replies after sending.
    pub listen_for: Duration,
    /// How many times the request is (re)transmitted over the listen
    /// window. XBDM occasionally drops a packet on busy kits.
    pub retransmits: u32,
    /// Delay between retransmits.
    pub retransmit_interval: Duration,
    /// Whether to set `SO_BROADCAST` on the socket. Required for
    /// 255.255.255.255 destinations; irrelevant otherwise.
    pub broadcast: bool,
}

impl DiscoveryConfig {
    /// Subnet-wide discovery defaults: broadcast to 255.255.255.255:730,
    /// listen for 1.5 s, retransmit twice.
    pub fn broadcast() -> Self {
        Self {
            destination: SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), NAP_PORT),
            listen_for: Duration::from_millis(1500),
            retransmits: 3,
            retransmit_interval: Duration::from_millis(400),
            broadcast: true,
        }
    }

    /// Unicast resolve defaults: single packet to a specific address, 500
    /// ms listen window.
    pub fn unicast(target: SocketAddr) -> Self {
        Self {
            destination: target,
            listen_for: Duration::from_millis(500),
            retransmits: 1,
            retransmit_interval: Duration::from_millis(0),
            broadcast: false,
        }
    }
}

/// Ask a specific name. Returns `Some` on the first matching reply, `None`
/// if no console answers within `config.listen_for`.
pub async fn find_by_name(
    name: &str,
    config: DiscoveryConfig,
) -> Result<Option<DiscoveredConsole>, rootcause::Report<Error>> {
    let request = NapRequest::lookup(name);
    let target_name = name.to_owned();
    let results = run(request, config, Some(1), move |reply| {
        reply.name == target_name
    })
    .await?;
    Ok(results.into_iter().next())
}

/// Ask every console on the segment to identify itself. Collects replies
/// until `config.listen_for` elapses. Duplicate replies (same address) are
/// de-duplicated.
pub async fn discover_all(
    config: DiscoveryConfig,
) -> Result<Vec<DiscoveredConsole>, rootcause::Report<Error>> {
    run(NapRequest::what_is_your_name(), config, None, |_| true).await
}

async fn run(
    request: NapRequest,
    config: DiscoveryConfig,
    stop_after: Option<usize>,
    mut keep: impl FnMut(&DiscoveredConsole) -> bool,
) -> Result<Vec<DiscoveredConsole>, rootcause::Report<Error>> {
    let packet = encode_request(&request).map_err(|e| {
        rootcause::Report::new(Error::from(TransportError::ConnectTimeout))
            .attach(format!("nap encode error: {e}"))
    })?;
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
        .await
        .map_err(Error::from)
        .into_report()
        .attach("binding udp socket for nap discovery")?;
    if config.broadcast {
        socket
            .set_broadcast(true)
            .map_err(Error::from)
            .into_report()
            .attach("enabling SO_BROADCAST")?;
    }

    tracing::debug!(
        target = %config.destination,
        bytes = packet.len(),
        opcode = ?request.opcode,
        "nap send"
    );

    let deadline = tokio::time::Instant::now() + config.listen_for;
    let retransmit_plan = (1..=config.retransmits)
        .map(|_| config.retransmit_interval)
        .collect::<Vec<_>>();
    let mut next_send = tokio::time::Instant::now();

    socket
        .send_to(&packet, config.destination)
        .await
        .map_err(Error::from)
        .into_report()
        .attach("sending initial nap packet")?;
    let mut sends_left = retransmit_plan.len();

    let mut buf = [0u8; 1024];
    let mut results: Vec<DiscoveredConsole> = Vec::new();
    let mut seen_addrs: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        if sends_left > 0 && now >= next_send {
            if let Err(e) = socket.send_to(&packet, config.destination).await {
                tracing::debug!(error = %e, "nap retransmit failed (continuing)");
            }
            next_send = now + retransmit_plan[retransmit_plan.len() - sends_left];
            sends_left -= 1;
        }
        let budget = deadline.saturating_duration_since(now);
        let next_event = if sends_left > 0 {
            core::cmp::min(budget, next_send.saturating_duration_since(now))
        } else {
            budget
        };
        let recv = tokio::time::timeout(next_event, socket.recv_from(&mut buf)).await;
        match recv {
            Err(_) => continue,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "nap recv_from error (continuing)");
                continue;
            }
            Ok(Ok((len, addr))) => match parse_response(&buf[..len]) {
                Ok(reply) => {
                    let console = DiscoveredConsole {
                        name: reply.name,
                        addr,
                    };
                    if keep(&console) && seen_addrs.insert(addr) {
                        tracing::debug!(name = %console.name, addr = %addr, "nap reply");
                        results.push(console);
                        if let Some(limit) = stop_after
                            && results.len() >= limit
                        {
                            break;
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        from = %addr,
                        error = ?err,
                        bytes = len,
                        "ignoring malformed nap reply"
                    );
                }
            },
        }
    }

    let _ = (NapError::EmptyPacket,);
    Ok(results)
}
