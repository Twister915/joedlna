use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use socket2::{Domain, Protocol, Socket, Type};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::description::{CONNECTION_MANAGER_TYPE, CONTENT_DIRECTORY_TYPE, MEDIA_SERVER_TYPE};
use crate::platform::SERVER;

const SSDP_ADDRESS: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1_900;

#[derive(Debug, Error)]
pub enum SsdpError {
    #[error("failed to configure SSDP socket: {0}")]
    Socket(#[source] io::Error),
}

#[derive(Debug, Clone)]
struct Advertisement {
    target: String,
    usn: String,
}

pub async fn run_ssdp(
    interface: Ipv4Addr,
    http_port: u16,
    udn: String,
    max_age_seconds: u32,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), SsdpError> {
    let socket = Arc::new(create_socket(interface)?);
    let location = format!("http://{interface}:{http_port}/root.xml");
    let advertisements = advertisements(&udn);
    send_alive(&socket, &advertisements, &location, max_age_seconds).await;
    info!(%interface, "SSDP advertisements active");
    let mut refresh = tokio::time::interval(Duration::from_secs(u64::from(max_age_seconds / 2)));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await;
    let mut buffer = [0_u8; 2_048];

    loop {
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                if let Ok((length, peer)) = received
                    && let Some(search) = parse_search(&buffer[..length])
                {
                    debug!(%peer, target = %search.target, mx = search.mx_seconds, "received SSDP search");
                    let matches = matching_advertisements(&advertisements, &search.target);
                    if !matches.is_empty() {
                        let socket = socket.clone();
                        let location = location.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(search_delay(search.mx_seconds, peer)).await;
                            for advertisement in matches {
                                let response = search_response(&advertisement, &location, max_age_seconds);
                                let _ = socket.send_to(response.as_bytes(), peer).await;
                            }
                        });
                    }
                }
            }
            _ = refresh.tick() => {
                send_alive(&socket, &advertisements, &location, max_age_seconds).await;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    send_byebye(&socket, &advertisements).await;
                    return Ok(());
                }
            }
        }
    }
}

fn create_socket(interface: Ipv4Addr) -> Result<UdpSocket, SsdpError> {
    let socket =
        Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(SsdpError::Socket)?;
    socket.set_reuse_address(true).map_err(SsdpError::Socket)?;
    #[cfg(unix)]
    socket.set_reuse_port(true).map_err(SsdpError::Socket)?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT).into())
        .map_err(SsdpError::Socket)?;
    socket
        .join_multicast_v4(&SSDP_ADDRESS, &interface)
        .map_err(SsdpError::Socket)?;
    socket
        .set_multicast_if_v4(&interface)
        .map_err(SsdpError::Socket)?;
    socket.set_multicast_ttl_v4(2).map_err(SsdpError::Socket)?;
    socket.set_nonblocking(true).map_err(SsdpError::Socket)?;
    UdpSocket::from_std(socket.into()).map_err(SsdpError::Socket)
}

fn advertisements(udn: &str) -> Vec<Advertisement> {
    [
        ("upnp:rootdevice", format!("{udn}::upnp:rootdevice")),
        (udn, udn.to_owned()),
        (MEDIA_SERVER_TYPE, format!("{udn}::{MEDIA_SERVER_TYPE}")),
        (
            CONTENT_DIRECTORY_TYPE,
            format!("{udn}::{CONTENT_DIRECTORY_TYPE}"),
        ),
        (
            CONNECTION_MANAGER_TYPE,
            format!("{udn}::{CONNECTION_MANAGER_TYPE}"),
        ),
    ]
    .into_iter()
    .map(|(target, usn)| Advertisement {
        target: target.into(),
        usn,
    })
    .collect()
}

async fn send_alive(
    socket: &UdpSocket,
    advertisements: &[Advertisement],
    location: &str,
    max_age_seconds: u32,
) {
    let destination = SocketAddrV4::new(SSDP_ADDRESS, SSDP_PORT);
    for advertisement in advertisements {
        let message = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDRESS}:{SSDP_PORT}\r\nCACHE-CONTROL: max-age={max_age_seconds}\r\nLOCATION: {location}\r\nNT: {}\r\nNTS: ssdp:alive\r\nSERVER: {SERVER}\r\nUSN: {}\r\n\r\n",
            advertisement.target, advertisement.usn
        );
        for _ in 0..2 {
            if let Err(error) = socket.send_to(message.as_bytes(), destination).await {
                debug!(%error, "failed to send SSDP alive advertisement");
            }
        }
    }
}

async fn send_byebye(socket: &UdpSocket, advertisements: &[Advertisement]) {
    let destination = SocketAddrV4::new(SSDP_ADDRESS, SSDP_PORT);
    for advertisement in advertisements {
        let message = format!(
            "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_ADDRESS}:{SSDP_PORT}\r\nNT: {}\r\nNTS: ssdp:byebye\r\nUSN: {}\r\n\r\n",
            advertisement.target, advertisement.usn
        );
        let _ = socket.send_to(message.as_bytes(), destination).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Search {
    target: String,
    mx_seconds: u64,
}

fn parse_search(message: &[u8]) -> Option<Search> {
    let message = std::str::from_utf8(message).ok()?;
    let mut lines = message.split("\r\n");
    if !lines.next()?.eq_ignore_ascii_case("M-SEARCH * HTTP/1.1") {
        return None;
    }
    let mut target = None;
    let mut mx_seconds = 1;
    let mut discover = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "man" => {
                discover = value
                    .trim()
                    .trim_matches('"')
                    .eq_ignore_ascii_case("ssdp:discover")
            }
            "st" => target = Some(value.trim().to_owned()),
            "mx" => mx_seconds = value.trim().parse::<u64>().unwrap_or(1).clamp(1, 5),
            _ => {}
        }
    }
    if !discover {
        return None;
    }
    Some(Search {
        target: target?,
        mx_seconds,
    })
}

fn matching_advertisements(advertisements: &[Advertisement], target: &str) -> Vec<Advertisement> {
    advertisements
        .iter()
        .filter(|advertisement| {
            target.eq_ignore_ascii_case("ssdp:all")
                || advertisement.target.eq_ignore_ascii_case(target)
        })
        .cloned()
        .collect()
}

fn search_response(advertisement: &Advertisement, location: &str, max_age_seconds: u32) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={max_age_seconds}\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: {SERVER}\r\nST: {}\r\nUSN: {}\r\n\r\n",
        advertisement.target, advertisement.usn
    )
}

fn search_delay(mx_seconds: u64, peer: SocketAddr) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.subsec_nanos());
    let mix = u64::from(nanos) ^ u64::from(peer.port());
    Duration::from_millis(mix % (mx_seconds.saturating_mul(1_000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_case_insensitive_search_headers() {
        let request = b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 3\r\nST: ssdp:all\r\n\r\n";

        let search = parse_search(request).unwrap();

        assert_eq!(search.target, "ssdp:all");
        assert_eq!(search.mx_seconds, 3);
    }

    #[test]
    fn rejects_non_discovery_datagram() {
        assert!(parse_search(b"NOTIFY * HTTP/1.1\r\n\r\n").is_none());
    }
}
