use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::description::source_protocol_info;
use crate::xml::escape_text;

const DEFAULT_TIMEOUT_SECONDS: u64 = 1_800;
const MAX_TIMEOUT_SECONDS: u64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventService {
    ContentDirectory,
    ConnectionManager,
}

#[derive(Debug, Clone)]
pub struct SubscriptionResponse {
    pub sid: String,
    pub timeout_seconds: u64,
    pub is_new: bool,
}

#[derive(Debug, Error)]
pub enum SubscriptionError {
    #[error("incompatible event subscription headers")]
    IncompatibleHeaders,
    #[error("missing or invalid callback URL")]
    InvalidCallback,
    #[error("callback is not on a permitted private network")]
    UnsafeCallback,
    #[error("NT must be upnp:event")]
    InvalidNt,
    #[error("subscription ID is missing or invalid")]
    InvalidSid,
}

#[derive(Debug, Clone)]
pub struct EventManager {
    subscriptions: Arc<Mutex<HashMap<String, Subscription>>>,
    interface: Ipv4Addr,
}

#[derive(Debug, Clone)]
struct Subscription {
    service: EventService,
    callback: Callback,
    expires: Instant,
    sequence: u32,
}

#[derive(Debug, Clone)]
struct Callback {
    address: SocketAddrV4,
    path: String,
}

impl EventManager {
    pub fn new(interface: Ipv4Addr) -> Self {
        Self {
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            interface,
        }
    }

    pub async fn subscribe(
        &self,
        service: EventService,
        headers: &HeaderMap,
        requester: Ipv4Addr,
    ) -> Result<SubscriptionResponse, SubscriptionError> {
        let sid = header(headers, "sid");
        let callback = header(headers, "callback");
        let nt = header(headers, "nt");
        let timeout_seconds = parse_timeout(header(headers, "timeout"));
        if let Some(sid) = sid {
            if callback.is_some() || nt.is_some() {
                return Err(SubscriptionError::IncompatibleHeaders);
            }
            let mut subscriptions = self.subscriptions.lock().await;
            prune_expired(&mut subscriptions);
            let subscription = subscriptions
                .get_mut(sid)
                .ok_or(SubscriptionError::InvalidSid)?;
            if subscription.service != service {
                return Err(SubscriptionError::InvalidSid);
            }
            subscription.expires = Instant::now() + Duration::from_secs(timeout_seconds);
            return Ok(SubscriptionResponse {
                sid: sid.into(),
                timeout_seconds,
                is_new: false,
            });
        }

        if nt != Some("upnp:event") {
            return Err(SubscriptionError::InvalidNt);
        }
        let callback = Callback::parse(callback.ok_or(SubscriptionError::InvalidCallback)?)?;
        if *callback.address.ip() != requester
            || !same_private_network(self.interface, *callback.address.ip())
        {
            return Err(SubscriptionError::UnsafeCallback);
        }
        let sid = format!("uuid:{}", Uuid::new_v4());
        self.subscriptions.lock().await.insert(
            sid.clone(),
            Subscription {
                service,
                callback,
                expires: Instant::now() + Duration::from_secs(timeout_seconds),
                sequence: 0,
            },
        );
        Ok(SubscriptionResponse {
            sid,
            timeout_seconds,
            is_new: true,
        })
    }

    pub async fn unsubscribe(
        &self,
        service: EventService,
        headers: &HeaderMap,
    ) -> Result<(), SubscriptionError> {
        if header(headers, "callback").is_some() || header(headers, "nt").is_some() {
            return Err(SubscriptionError::IncompatibleHeaders);
        }
        let sid = header(headers, "sid").ok_or(SubscriptionError::InvalidSid)?;
        let mut subscriptions = self.subscriptions.lock().await;
        if subscriptions
            .get(sid)
            .map(|subscription| subscription.service)
            != Some(service)
        {
            return Err(SubscriptionError::InvalidSid);
        }
        subscriptions.remove(sid);
        Ok(())
    }

    pub async fn notify_initial(&self, sid: &str, update_id: u32) {
        let notification = {
            let mut subscriptions = self.subscriptions.lock().await;
            let Some(subscription) = subscriptions.get_mut(sid) else {
                return;
            };
            notification(sid, subscription, update_id)
        };
        send_notification(notification).await;
    }

    pub async fn notify_content_changed(&self, update_id: u32) {
        let notifications = {
            let mut subscriptions = self.subscriptions.lock().await;
            prune_expired(&mut subscriptions);
            subscriptions
                .iter_mut()
                .filter(|(_, subscription)| subscription.service == EventService::ContentDirectory)
                .map(|(sid, subscription)| notification(sid, subscription, update_id))
                .collect::<Vec<_>>()
        };
        for notification in notifications {
            tokio::spawn(send_notification(notification));
        }
    }
}

impl Callback {
    fn parse(value: &str) -> Result<Self, SubscriptionError> {
        let value = value.trim();
        let value = value
            .strip_prefix('<')
            .and_then(|value| value.split_once('>').map(|(url, _)| url))
            .ok_or(SubscriptionError::InvalidCallback)?;
        let authority_and_path = value
            .strip_prefix("http://")
            .ok_or(SubscriptionError::InvalidCallback)?;
        let (authority, path) = authority_and_path
            .split_once('/')
            .map_or((authority_and_path, "/"), |(authority, path)| {
                (authority, path)
            });
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or(SubscriptionError::InvalidCallback)?;
        let address = SocketAddrV4::new(
            host.parse()
                .map_err(|_| SubscriptionError::InvalidCallback)?,
            port.parse()
                .map_err(|_| SubscriptionError::InvalidCallback)?,
        );
        Ok(Self {
            address,
            path: if path == "/" {
                path.into()
            } else {
                format!("/{path}")
            },
        })
    }
}

#[derive(Debug)]
struct Notification {
    callback: Callback,
    sid: String,
    sequence: u32,
    body: String,
}

fn notification(sid: &str, subscription: &mut Subscription, update_id: u32) -> Notification {
    let sequence = subscription.sequence;
    subscription.sequence = if sequence == u32::MAX {
        1
    } else {
        sequence + 1
    };
    let body = event_body(subscription.service, update_id);
    Notification {
        callback: subscription.callback.clone(),
        sid: sid.into(),
        sequence,
        body,
    }
}

async fn send_notification(notification: Notification) {
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(notification.callback.address).await?;
        let request = format!(
            "NOTIFY {} HTTP/1.1\r\nHOST: {}\r\nCONTENT-TYPE: text/xml; charset=\"utf-8\"\r\nCONTENT-LENGTH: {}\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nSID: {}\r\nSEQ: {}\r\nCONNECTION: close\r\n\r\n{}",
            notification.callback.path,
            notification.callback.address,
            notification.body.len(),
            notification.sid,
            notification.sequence,
            notification.body,
        );
        stream.write_all(request.as_bytes()).await?;
        stream.shutdown().await?;
        let mut response = [0_u8; 128];
        let _ = stream.read(&mut response).await;
        Ok::<_, std::io::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => debug!(sid = %notification.sid, "event notification delivered"),
        Ok(Err(error)) => warn!(sid = %notification.sid, %error, "event notification failed"),
        Err(_) => warn!(sid = %notification.sid, "event notification timed out"),
    }
}

fn event_body(service: EventService, update_id: u32) -> String {
    let mut body = String::from(
        r#"<?xml version="1.0" encoding="utf-8"?><e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">"#,
    );
    match service {
        EventService::ContentDirectory => {
            let _ = write!(
                body,
                "<e:property><SystemUpdateID>{update_id}</SystemUpdateID></e:property>"
            );
        }
        EventService::ConnectionManager => {
            let source = escape_text(&source_protocol_info());
            let _ = write!(
                body,
                "<e:property><SourceProtocolInfo>{source}</SourceProtocolInfo></e:property><e:property><SinkProtocolInfo></SinkProtocolInfo></e:property><e:property><CurrentConnectionIDs>0</CurrentConnectionIDs></e:property>"
            );
        }
    }
    body.push_str("</e:propertyset>");
    body
}

fn prune_expired(subscriptions: &mut HashMap<String, Subscription>) {
    let now = Instant::now();
    subscriptions.retain(|_, subscription| subscription.expires > now);
}

fn parse_timeout(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.strip_prefix("Second-"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
        .clamp(1, MAX_TIMEOUT_SECONDS)
}

fn same_private_network(interface: Ipv4Addr, callback: Ipv4Addr) -> bool {
    let interface = interface.octets();
    let callback = callback.octets();
    match (interface, callback) {
        ([10, ..], [10, ..]) => true,
        ([172, left, ..], [172, right, ..])
            if (16..=31).contains(&left) && (16..=31).contains(&right) =>
        {
            true
        }
        ([192, 168, ..], [192, 168, ..]) => true,
        ([169, 254, ..], [169, 254, ..]) => true,
        ([127, ..], [127, ..]) => true,
        _ => false,
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use test_case::test_case;

    use super::*;

    #[test]
    fn parses_callback() {
        let callback = Callback::parse("<http://192.168.1.40:49152/events>").unwrap();
        assert_eq!(*callback.address.ip(), Ipv4Addr::new(192, 168, 1, 40));
        assert_eq!(callback.address.port(), 49_152);
        assert_eq!(callback.path, "/events");
    }

    #[test_case("192.168.1.2", "192.168.40.3", true ; "private 192")]
    #[test_case("10.1.2.3", "10.99.1.2", true ; "private 10")]
    #[test_case("192.168.1.2", "10.0.0.3", false ; "different private ranges")]
    #[test_case("192.168.1.2", "8.8.8.8", false ; "public callback")]
    fn checks_private_network(interface: &str, callback: &str, expected: bool) {
        assert_eq!(
            same_private_network(interface.parse().unwrap(), callback.parse().unwrap()),
            expected
        );
    }
}
