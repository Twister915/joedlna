use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

use bytes::Bytes;
use quick_xml::Reader;
use quick_xml::events::Event;
use thiserror::Error;

use crate::catalog::{Catalog, Entry, EntryKind, ROOT_ID};
use crate::description::{CONNECTION_MANAGER_TYPE, CONTENT_DIRECTORY_TYPE, source_protocol_info};
use crate::xml::{element, escape_text};

#[derive(Debug, Error)]
pub enum SoapParseError {
    #[error("SOAPAction header is missing or invalid")]
    MissingAction,
    #[error("invalid XML in SOAP body: {0}")]
    InvalidXml(#[from] quick_xml::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpnpError {
    pub code: u16,
    pub description: &'static str,
}

pub fn parse_request(
    soap_action: Option<&str>,
    body: &Bytes,
) -> Result<(String, HashMap<String, String>), SoapParseError> {
    let action = soap_action
        .and_then(|value| {
            value
                .trim_matches(|character| character == '"' || character == ' ')
                .rsplit_once('#')
        })
        .map(|(_, action)| action.to_owned())
        .ok_or(SoapParseError::MissingAction)?;

    let mut reader = Reader::from_reader(body.as_ref());
    reader.config_mut().trim_text(true);
    let mut current = None;
    let mut arguments = HashMap::new();
    loop {
        match reader.read_event()? {
            Event::Start(start) => {
                let name = String::from_utf8_lossy(start.local_name().as_ref()).into_owned();
                current = Some(name);
            }
            Event::Text(text) => {
                if let Some(name) = current.take() {
                    let raw = String::from_utf8_lossy(text.as_ref());
                    let value = quick_xml::escape::unescape(&raw)
                        .map_err(quick_xml::Error::Escape)?
                        .into_owned();
                    arguments.insert(name, value);
                }
            }
            Event::Empty(empty) => {
                let name = String::from_utf8_lossy(empty.local_name().as_ref()).into_owned();
                arguments.entry(name).or_default();
                current = None;
            }
            Event::End(_) => {
                if let Some(name) = current.take() {
                    arguments.entry(name).or_default();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((action, arguments))
}

pub fn content_directory_response(
    action: &str,
    arguments: &HashMap<String, String>,
    catalog: &Catalog,
    bookmarks: &BTreeMap<String, u64>,
    base_url: &str,
) -> Result<String, UpnpError> {
    let output = match action {
        "GetSearchCapabilities" => vec![("SearchCaps", String::new())],
        "GetSortCapabilities" => vec![("SortCaps", "dc:title".into())],
        "GetSystemUpdateID" => vec![("Id", catalog.update_id().to_string())],
        "Browse" => browse(arguments, catalog, bookmarks, base_url)?,
        "X_SetBookmark" => Vec::new(),
        _ => return Err(upnp_error(401)),
    };
    Ok(soap_response(CONTENT_DIRECTORY_TYPE, action, &output))
}

pub fn connection_manager_response(
    action: &str,
    arguments: &HashMap<String, String>,
) -> Result<String, UpnpError> {
    let output = match action {
        "GetProtocolInfo" => vec![("Source", source_protocol_info()), ("Sink", String::new())],
        "GetCurrentConnectionIDs" => vec![("ConnectionIDs", "0".into())],
        "GetCurrentConnectionInfo" => {
            if arguments.get("ConnectionID").map(String::as_str) != Some("0") {
                return Err(upnp_error(706));
            }
            vec![
                ("RcsID", "-1".into()),
                ("AVTransportID", "-1".into()),
                ("ProtocolInfo", String::new()),
                ("PeerConnectionManager", String::new()),
                ("PeerConnectionID", "-1".into()),
                ("Direction", "Output".into()),
                ("Status", "OK".into()),
            ]
        }
        _ => return Err(upnp_error(401)),
    };
    Ok(soap_response(CONNECTION_MANAGER_TYPE, action, &output))
}

fn browse(
    arguments: &HashMap<String, String>,
    catalog: &Catalog,
    bookmarks: &BTreeMap<String, u64>,
    base_url: &str,
) -> Result<Vec<(&'static str, String)>, UpnpError> {
    let object_id = required(arguments, "ObjectID")?;
    let browse_flag = required(arguments, "BrowseFlag")?;
    let starting_index = parse_u32(arguments, "StartingIndex")? as usize;
    let requested_count = parse_u32(arguments, "RequestedCount")? as usize;
    let sort_criteria = required(arguments, "SortCriteria")?;
    let filter = required(arguments, "Filter")?;
    let title_sort = match sort_criteria {
        "" => None,
        "+dc:title" => Some(false),
        "-dc:title" => Some(true),
        _ => return Err(upnp_error(709)),
    };

    let mut didl = didl_header();
    let (number_returned, total_matches, update_id) = match browse_flag {
        "BrowseMetadata" => {
            if object_id == ROOT_ID {
                append_root(&mut didl, catalog, filter);
            } else {
                let entry = catalog.entry(object_id).ok_or_else(|| upnp_error(701))?;
                append_entry(&mut didl, entry, catalog, bookmarks, base_url, filter);
            }
            (1, 1, catalog.object_update_id(object_id))
        }
        "BrowseDirectChildren" => {
            if object_id != ROOT_ID
                && !matches!(
                    catalog.entry(object_id).map(|entry| &entry.kind),
                    Some(EntryKind::Container)
                )
            {
                return Err(upnp_error(701));
            }
            let mut children: Vec<_> = catalog
                .children(object_id)
                .ok_or_else(|| upnp_error(701))?
                .collect();
            let total = children.len();
            if let Some(reverse) = title_sort {
                children.sort_unstable_by(|left, right| {
                    left.title
                        .cmp(&right.title)
                        .then_with(|| left.id.cmp(&right.id))
                });
                if reverse {
                    children.reverse();
                }
            }
            let selected: Vec<_> = if requested_count == 0 {
                children.into_iter().skip(starting_index).collect()
            } else {
                children
                    .into_iter()
                    .skip(starting_index)
                    .take(requested_count)
                    .collect()
            };
            for entry in &selected {
                append_entry(&mut didl, entry, catalog, bookmarks, base_url, filter);
            }
            (selected.len(), total, catalog.object_update_id(object_id))
        }
        _ => return Err(upnp_error(402)),
    };
    didl.push_str("</DIDL-Lite>");
    Ok(vec![
        ("Result", didl),
        ("NumberReturned", number_returned.to_string()),
        ("TotalMatches", total_matches.to_string()),
        ("UpdateID", update_id.to_string()),
    ])
}

fn required<'a>(arguments: &'a HashMap<String, String>, name: &str) -> Result<&'a str, UpnpError> {
    arguments
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| upnp_error(402))
}

fn parse_u32(arguments: &HashMap<String, String>, name: &str) -> Result<u32, UpnpError> {
    required(arguments, name)?
        .parse()
        .map_err(|_| upnp_error(402))
}

fn didl_header() -> String {
    r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/" xmlns:sec="http://www.sec.co.kr/dlna">"#.into()
}

fn append_root(output: &mut String, catalog: &Catalog, filter: &str) {
    let child_count = if filter_has(filter, "@childCount") {
        format!(r#" childCount="{}""#, catalog.child_count(ROOT_ID))
    } else {
        String::new()
    };
    let _ = write!(
        output,
        r#"<container id="0" parentID="-1" restricted="1"{child_count}><dc:title>Root</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>"#,
    );
}

fn append_entry(
    output: &mut String,
    entry: &Entry,
    catalog: &Catalog,
    bookmarks: &BTreeMap<String, u64>,
    base_url: &str,
    filter: &str,
) {
    match entry.kind {
        EntryKind::Container => {
            let child_count = if filter_has(filter, "@childCount") {
                format!(r#" childCount="{}""#, catalog.child_count(&entry.id))
            } else {
                String::new()
            };
            let _ = write!(
                output,
                r#"<container id="{}" parentID="{}" restricted="1"{}><dc:title>{}</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>"#,
                escape_text(&entry.id),
                escape_text(&entry.parent_id),
                child_count,
                escape_text(&entry.title),
            );
        }
        EntryKind::Media(media) => {
            let include_resource = filter_has(filter, "res");
            let include_size = filter_has(filter, "res") || filter_has(filter, "res@size");
            let protocol_info = protocol_info(media.mime_type, media.dlna_profile);
            let extension = entry
                .path
                .extension()
                .and_then(|extension| extension.to_str())
                .filter(|extension| {
                    extension
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric())
                })
                .unwrap_or("bin");
            let resource_url = format!("{base_url}/media/{}/file.{extension}", entry.id);
            let _ = write!(
                output,
                r#"<item id="{}" parentID="{}" restricted="1"><dc:title>{}</dc:title><upnp:class>{}</upnp:class>"#,
                escape_text(&entry.id),
                escape_text(&entry.parent_id),
                escape_text(&entry.title),
                media.upnp_class,
            );
            if filter_has(filter, "sec:dcmInfo")
                && let Some(position) = bookmarks.get(&entry.id)
            {
                let _ = write!(
                    output,
                    "<sec:dcmInfo>CREATIONDATE=0,FOLDER={},BM={position}</sec:dcmInfo>",
                    escape_text(&entry.title),
                );
            }
            if include_resource {
                let size = if include_size {
                    format!(r#" size="{}""#, entry.size)
                } else {
                    String::new()
                };
                let _ = write!(
                    output,
                    r#"<res protocolInfo="{}"{}>{}</res>"#,
                    escape_text(&protocol_info),
                    size,
                    escape_text(&resource_url),
                );
            }
            output.push_str("</item>");
        }
    }
}

fn filter_has(filter: &str, requested: &str) -> bool {
    filter == "*"
        || filter.split(',').any(|property| {
            property == requested || (requested == "res" && property.starts_with("res@"))
        })
}

pub fn protocol_info(mime_type: &str, dlna_profile: Option<&str>) -> String {
    let profile = dlna_profile.map_or(String::new(), |profile| format!("DLNA.ORG_PN={profile};"));
    format!(
        "http-get:*:{mime_type}:{profile}DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
    )
}

fn soap_response(service_type: &str, action: &str, values: &[(&str, String)]) -> String {
    let mut output = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action}Response xmlns:u="{service_type}">"#
    );
    for (name, value) in values {
        element(&mut output, name, value);
    }
    let _ = write!(output, "</u:{action}Response></s:Body></s:Envelope>");
    output
}

pub fn soap_fault(error: UpnpError) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>{}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#,
        error.code, error.description
    )
}

fn upnp_error(code: u16) -> UpnpError {
    let description = match code {
        401 => "Invalid Action",
        402 => "Invalid Args",
        501 => "Action Failed",
        701 => "No Such Object",
        706 => "Invalid Connection Reference",
        709 => "Unsupported or Invalid Sort Criteria",
        710 => "No Such Container",
        720 => "Cannot Process Request",
        _ => "Action Failed",
    };
    UpnpError { code, description }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use bytes::Bytes;
    use tempfile::tempdir;

    use super::*;
    use crate::config::{Config, MediaCategory, NetworkConfig, ScannerConfig, ShareConfig};

    #[test]
    fn parses_namespaced_soap_arguments() {
        let body = Bytes::from_static(
            br#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:test"><ObjectID>0</ObjectID><Filter>*</Filter></u:Browse></s:Body></s:Envelope>"#,
        );

        let (action, arguments) = parse_request(Some("\"urn:test#Browse\""), &body).unwrap();

        assert_eq!(action, "Browse");
        assert_eq!(arguments["ObjectID"], "0");
        assert_eq!(arguments["Filter"], "*");
    }

    #[test]
    fn parses_empty_soap_arguments() {
        let body = Bytes::from_static(
            br#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Browse xmlns:u="urn:test"><ObjectID>0</ObjectID><SortCriteria></SortCriteria><Filter /></u:Browse></s:Body></s:Envelope>"#,
        );

        let (_, arguments) = parse_request(Some("urn:test#Browse"), &body).unwrap();

        assert_eq!(arguments["SortCriteria"], "");
        assert_eq!(arguments["Filter"], "");
    }

    #[test]
    fn protocol_info_advertises_byte_ranges() {
        let value = protocol_info("audio/mpeg", Some("MP3"));
        assert!(value.contains("DLNA.ORG_PN=MP3"));
        assert!(value.contains("DLNA.ORG_OP=01"));
    }

    #[test]
    fn browse_filters_resource_fields() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("song & dance.mp3"), b"audio").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Music".into(),
                path: temp.path().into(),
                media: MediaCategory::ALL.into(),
            }],
        };
        let catalog = Catalog::scan(&config).unwrap();
        let share_id = catalog
            .children(ROOT_ID)
            .unwrap()
            .next()
            .unwrap()
            .id
            .clone();
        let arguments = HashMap::from([
            ("ObjectID".into(), share_id),
            ("BrowseFlag".into(), "BrowseDirectChildren".into()),
            ("Filter".into(), "dc:title,upnp:class".into()),
            ("StartingIndex".into(), "0".into()),
            ("RequestedCount".into(), "0".into()),
            ("SortCriteria".into(), String::new()),
        ]);

        let response = content_directory_response(
            "Browse",
            &arguments,
            &catalog,
            &BTreeMap::new(),
            "http://127.0.0.1:1",
        )
        .unwrap();

        assert!(response.contains("song &amp;amp; dance"));
        assert!(!response.contains("&lt;res"));
    }

    #[test]
    fn browse_returns_samsung_resume_position() {
        let temp = tempdir().unwrap();
        fs::write(temp.path().join("episode.mp4"), b"video").unwrap();
        let config = Config {
            friendly_name: "Test".into(),
            network: NetworkConfig::default(),
            scanner: ScannerConfig::default(),
            shares: vec![ShareConfig {
                name: "Shows".into(),
                path: temp.path().into(),
                media: MediaCategory::ALL.into(),
            }],
        };
        let catalog = Catalog::scan(&config).unwrap();
        let share_id = catalog
            .children(ROOT_ID)
            .unwrap()
            .next()
            .unwrap()
            .id
            .clone();
        let item_id = catalog
            .children(&share_id)
            .unwrap()
            .next()
            .unwrap()
            .id
            .clone();
        let arguments = HashMap::from([
            ("ObjectID".into(), item_id.clone()),
            ("BrowseFlag".into(), "BrowseMetadata".into()),
            ("Filter".into(), "*".into()),
            ("StartingIndex".into(), "0".into()),
            ("RequestedCount".into(), "0".into()),
            ("SortCriteria".into(), String::new()),
        ]);
        let bookmarks = BTreeMap::from([(item_id, 196_147)]);

        let response = content_directory_response(
            "Browse",
            &arguments,
            &catalog,
            &bookmarks,
            "http://127.0.0.1:1",
        )
        .unwrap();

        assert!(response.contains("sec:dcmInfo"));
        assert!(response.contains("BM=196147"));
    }
}
