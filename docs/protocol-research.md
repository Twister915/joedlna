# DLNA and UPnP AV protocol research

## What “DLNA” means on the wire

DLNA is not one standalone discovery-and-streaming protocol. A basic Digital Media Server combines:

- IP networking;
- UPnP Device Architecture for SSDP discovery, XML description, SOAP control, and GENA eventing;
- UPnP AV's `MediaServer`, `ContentDirectory`, and `ConnectionManager` device-control protocols;
- DIDL-Lite metadata returned inside SOAP;
- HTTP media transfer;
- DLNA interoperability headers, transfer modes, media profiles, and certification rules.

The Open Connectivity Foundation now publishes the UPnP standards after the UPnP Forum transferred its assets. The [UPnP standards index](https://openconnectivity.org/developer/specifications/upnp-resources/upnp/) and [Device Architecture 2.0](https://openconnectivity.org/upnp-specs/UPnP-arch-DeviceArchitecture-v2.0-20200417.pdf) are the architectural sources. JoeDLNA intentionally speaks the UPnP 1.0-compatible subset because old television control points are the target.

## Discovery

SSDP uses IPv4 multicast address `239.255.255.250:1900`. A root device advertises separate notification targets for:

- `upnp:rootdevice`;
- its UUID;
- `urn:schemas-upnp-org:device:MediaServer:1`;
- each service type.

Advertisements contain a lifetime and `LOCATION` for the root description. A control point sends `M-SEARCH` with `MAN: "ssdp:discover"`, an `MX` response window, and an `ST` search target. The server answers the requester's unicast address after a delay inside that window. JoeDLNA sends each initial advertisement twice and refreshes at half of `max-age`.

The normative packet forms and required headers come from [UPnP Device Architecture](https://upnp.org/specs/arch/UPnPDA10_20000613.htm). OCF's current architecture also confirms the fixed multicast endpoint and the requirement to advertise the full device/service surface.

## Description and service selection

The [MediaServer:1 device template](https://openconnectivity.org/wp-content/uploads/2015/11/UPnP-av-MediaServer-v1-Device-20020625.pdf) requires exactly the two services JoeDLNA exposes:

- `ContentDirectory:1`, which lets a control point locate and describe media;
- `ConnectionManager:1`, which lets it match source protocols/formats against a renderer.

`AVTransport` is optional for a MediaServer and is not needed for pull-mode HTTP transfer. The TV fetches the selected resource URL directly.

## ContentDirectory and DIDL-Lite

The authoritative [ContentDirectory:1 service specification](https://upnp.org/specs/av/UPnP-av-ContentDirectory-v1-Service.pdf) defines four required actions:

- `GetSearchCapabilities`;
- `GetSortCapabilities`;
- `GetSystemUpdateID`;
- `Browse`.

`BrowseMetadata` returns one object. `BrowseDirectChildren` applies zero-based pagination and reports both `NumberReturned` and `TotalMatches`. Object ID `0` is the root and its parent ID is reserved value `-1`. Each object has an ID, parent ID, title, UPnP class, and restricted flag. Playable items carry one or more `res` elements whose `protocolInfo` describes transport, network, MIME type, and additional information.

JoeDLNA implements the required actions, the two browse flags, requested-property filtering, `dc:title` sorting, pagination, root semantics, per-container update IDs, and standard errors 401, 402, 701, 706, and 709. Search and mutation actions are optional and omitted.

## ConnectionManager

The [ConnectionManager:1 specification](https://upnp.org/specs/av/UPnP-av-ConnectionManager-v1-Service.pdf) requires `GetProtocolInfo`, `GetCurrentConnectionIDs`, and `GetCurrentConnectionInfo`. If optional `PrepareForConnection` is absent, `CurrentConnectionIDs` should be `0`, and limited out-of-band information is available for connection ID 0. JoeDLNA implements that exact stateless HTTP model and returns error 706 for other IDs.

## Eventing

The event subscription URL accepts `SUBSCRIBE`, renewal `SUBSCRIBE`, and `UNSUBSCRIBE`. New subscriptions carry `CALLBACK` and `NT: upnp:event`; renewals carry only a `SID`. The publisher returns `SID` and `TIMEOUT`, immediately sends sequence-zero state, and increments `SEQ` thereafter.

JoeDLNA events `SystemUpdateID` for ContentDirectory and the three required ConnectionManager variables. It rejects public or cross-private-range callback addresses. This follows the current UDA rule that delivery URLs must be on the publisher's network segment.

## HTTP and DLNA transfer behavior

The `res` URL is ordinary HTTP. Byte seek support matters to televisions for startup, seeking, and resuming. JoeDLNA implements complete, open-ended, and suffix single ranges with 206/416 behavior, `Accept-Ranges`, `Content-Range`, `Content-Length`, `GET`, and `HEAD`, following [RFC 9110 section 14](https://www.rfc-editor.org/rfc/rfc9110.html#name-range-requests).

For DLNA-aware requests it recognizes `getcontentFeatures.dlna.org` and returns `contentFeatures.dlna.org`; it reports streaming transfer mode and protocol-info operation `DLNA.ORG_OP=01` (byte ranges), conversion indicator `DLNA.ORG_CI=0` (original content), and DLNA 1.5 flags.

The official [DLNA 4.0 white paper](https://www.dlna.org/s/DLNA-4-0-White-Paper.pdf) explains that the June 2016 Guidelines add later profiles, IPv6, energy behavior, and transcoding requirements. The current [DLNA Guidelines overview](https://spirespark.com/dlna/guidelines) explains why a profile is more than a filename format: it fixes a suitable combination of codecs, resolution, aspect ratio, bitrate, and related parameters, then exposes a profile-ID token during discovery and transfer. That is why JoeDLNA does not invent `DLNA.ORG_PN` from `.mp4` alone. JoeDLNA does not claim DLNA 4.0 or certification. The [DLNA FAQ](https://www.dlna.org/faq) says the Guidelines are no longer being updated; certification tooling is now operated by SpireSpark.

## Conformance boundary

| Area | Implemented now | Deferred |
|---|---|---|
| SSDP IPv4 | Alive, byebye, M-SEARCH, all required targets | Interface enumeration, unicast search port |
| SSDP IPv6 | No | UDA IPv6 multicast scopes |
| Device description | MediaServer:1, DMS-1.50 marker, two required services | Icons, presentation controls |
| ContentDirectory | Required actions and browse hierarchy | Search, writable objects, playlists |
| ConnectionManager | Three required actions, stateless connection 0 | Prepare/complete connection |
| GENA | Subscribe, renew, unsubscribe, initial/update notify | IPv6 callbacks, retry policy |
| Samsung TV extensions | `X_SetBookmark` and `sec:dcmInfo` resume position | Other vendor extensions |
| HTTP | GET, HEAD, one byte range, DLNA headers | Multipart ranges, time seek ranges |
| Media understanding | Extension-to-MIME classification | Codec probing, validated DLNA profiles, duration/resolution |
| DLNA 4.0 | No claim | Transcoding, modern mandatory profiles, IPv6, certification |

The largest real-world risk is renderer-specific codec and container support. Serving `video/mp4`
correctly cannot make a television decode the streams inside it. A future probing layer must validate
media before attaching `DLNA.ORG_PN` and may expose transcoded resources alongside originals.
