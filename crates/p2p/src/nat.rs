//! `STUN`-based `NAT` traversal helpers.
//!
//! The `STUN` client implements `RFC 5389` Binding Requests and parses
//! `XOR-MAPPED-ADDRESS` from the response. Two interfaces are exposed:
//!
//! * [`NatTraversal::detect_nat_type`] — single-server probe that distinguishes
//!   only `Open` from `Symmetric`. Kept for callers that have a single
//!   `STUN` endpoint configured.
//! * [`detect_nat_type_rfc5780`] — `RFC 5780` two-server detection that
//!   classifies the full `NAT` taxonomy by issuing four Binding Requests
//!   against a primary server (supporting `CHANGE-REQUEST`) and a
//!   secondary server on a different `IP`.
//!
//! See [`crate::traversal::NatType`] for the dialect used by the
//! hole-punching decision tree; the two enums will be reconciled once
//! callers migrate. See `TODO(nat-reconcile)`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::time::{Duration, timeout};

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const CHANGE_REQUEST: u16 = 0x0003;
const RESPONSE_ORIGIN: u16 = 0x802b;
const OTHER_ADDRESS: u16 = 0x802c;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_HEADER_LEN: usize = 20;
const STUN_TRANSACTION_ID_LEN: usize = 12;
const STUN_ATTRIBUTE_HEADER_LEN: usize = 4;
const STUN_IPV6_ADDR_LEN: usize = 16;
const STUN_IPV4_ATTRIBUTE_VALUE_LEN: usize = 8;
const STUN_IPV6_ATTRIBUTE_VALUE_LEN: usize = 20;
const STUN_CHANGE_REQUEST_VALUE_LEN: u16 = 4;
const STUN_FAMILY_IPV4: u8 = 0x01;
const STUN_FAMILY_IPV6: u8 = 0x02;
const STUN_RESPONSE_MAX_LEN: usize = 1500;
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

/// `CHANGE-REQUEST` flag asking the server to respond from a different `IP`.
/// `RFC 5780 §7.2` reserves bit position 2 of the high byte for "change `IP`".
const CHANGE_REQUEST_FLAG_CHANGE_IP: u32 = 0x0000_0004;
/// `CHANGE-REQUEST` flag asking the server to respond from a different port.
/// `RFC 5780 §7.2` reserves bit position 1 of the high byte for "change port".
const CHANGE_REQUEST_FLAG_CHANGE_PORT: u32 = 0x0000_0002;

/// `NAT` type detected by `STUN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatType {
    /// Host is directly reachable on a public address — no `NAT`.
    Open,
    /// Full cone `NAT` — mapped port reachable from any external host.
    FullCone,
    /// Restricted cone `NAT` — only reachable from hosts that have received packets.
    RestrictedCone,
    /// Port-restricted cone `NAT` — like restricted but port-specific.
    PortRestrictedCone,
    /// Symmetric `NAT` — different mapping per destination. Requires relay.
    Symmetric,
    /// Detection failed or returned an inconclusive result. Treated
    /// conservatively as needing relay.
    Unknown,
}

// TODO(nat-reconcile): merge this enum with `crate::traversal::NatType` in a
// follow-up round. Both share the same six variants now (`Open`, `FullCone`,
// `RestrictedCone`, `PortRestrictedCone`, `Symmetric`, `Unknown`); this one
// is kept distinct until every caller of `nat::NatType::Open` /
// `nat::NatType::Symmetric` has been audited and migrated.

/// Error returned by `RFC 5780` `NAT` detection.
#[derive(Debug, Error)]
pub enum NatDetectionError {
    /// Underlying `I/O` error from the socket.
    #[error("STUN socket I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Single-request timeout exhausted retries.
    #[error("STUN request timed out after all retries")]
    Timeout,
    /// Server response could not be parsed.
    #[error("malformed STUN response: {0}")]
    MalformedStunResponse(&'static str),
    /// Primary server returned a `Test II` response from the same address
    /// as `Test I`, so `CHANGE-REQUEST` cannot be honoured and the
    /// classification cannot proceed past `Test II`.
    #[error("STUN server does not honour CHANGE-REQUEST")]
    NoChangeRequestSupport,
}

/// Configuration for `RFC 5780` `NAT` detection.
#[derive(Debug, Clone, Copy)]
pub struct NatDetectionConfig {
    /// Primary `STUN` server — must support `CHANGE-REQUEST` per `RFC 5780`.
    pub primary: SocketAddr,
    /// Secondary `STUN` server. Must be on a different `IP` from `primary`
    /// for the symmetric-`NAT` distinguisher to work.
    pub secondary: SocketAddr,
    /// How long to wait for each individual `STUN` response.
    pub per_request_timeout: Duration,
    /// How many times to retry a single request before giving up.
    pub retries: u32,
}

/// Decoded `STUN` Binding Success response.
#[derive(Debug, Clone, Copy)]
struct BindingResponse {
    /// `XOR-MAPPED-ADDRESS` — the external (mapped) address the server
    /// observed the client at.
    mapped: SocketAddr,
    /// `RESPONSE-ORIGIN` — the address the server sent the response from.
    /// `None` when the server did not include this attribute (legacy
    /// `STUN`).
    response_origin: Option<SocketAddr>,
    /// `OTHER-ADDRESS` — the alternate address+port the server can be
    /// reached at, used to validate `CHANGE-REQUEST` support.
    other_address: Option<SocketAddr>,
}

/// NAT traversal coordinator.
#[derive(Debug, Clone, Copy)]
pub struct NatTraversal;

impl NatTraversal {
    /// Detect the local `NAT` type using `STUN` against a single server.
    ///
    /// Distinguishes only `Open` from `Symmetric` — full classification
    /// requires [`detect_nat_type_rfc5780`].
    pub async fn detect_nat_type(stun_server: &str) -> Result<NatType> {
        let (local_address, external_address) = stun_binding_request(stun_server).await?;
        if local_address.ip() == external_address.ip()
            && local_address.port() == external_address.port()
        {
            Ok(NatType::Open)
        } else {
            Ok(NatType::Symmetric)
        }
    }

    /// Get the external address as seen by a `STUN` server.
    pub async fn external_address(stun_server: &str) -> Result<SocketAddr> {
        let (_, external_address) = stun_binding_request(stun_server).await?;
        Ok(external_address)
    }
}

async fn stun_binding_request(stun_server: &str) -> Result<(SocketAddr, SocketAddr)> {
    let server = resolve_stun_server(stun_server)
        .await
        .with_context(|| format!("resolving STUN server {stun_server}"))?;
    let bind_address = match server {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let socket = UdpSocket::bind(bind_address)
        .await
        .context("binding STUN UDP socket")?;
    socket
        .connect(server)
        .await
        .context("connecting STUN UDP socket")?;

    let transaction_id = transaction_id()?;
    let request = encode_binding_request(&transaction_id, 0);
    socket
        .send(&request)
        .await
        .context("sending STUN binding request")?;

    let mut response = [0u8; STUN_RESPONSE_MAX_LEN];
    let received = timeout(STUN_TIMEOUT, socket.recv(&mut response))
        .await
        .context("waiting for STUN binding response")?
        .context("receiving STUN binding response")?;
    let response_slice = response
        .get(..received)
        .ok_or_else(|| anyhow::anyhow!("STUN response length out of range"))?;
    let parsed = decode_binding_response(response_slice, &transaction_id)
        .map_err(|reason| anyhow::anyhow!("decoding STUN response: {reason}"))?;
    let local_address = socket
        .local_addr()
        .context("reading STUN socket local address")?;

    Ok((local_address, parsed.mapped))
}

/// Send a single `STUN` Binding Request and wait for the response.
///
/// `target` is the server's `IP`+port. `change_request_flags` is the value
/// of the `CHANGE-REQUEST` attribute payload (zero to omit the attribute).
async fn send_binding_request(
    socket: &UdpSocket,
    target: SocketAddr,
    change_request_flags: u32,
    per_request_timeout: Duration,
) -> Result<BindingResponse, NatDetectionError> {
    let transaction_id =
        transaction_id().map_err(|_| NatDetectionError::MalformedStunResponse("transaction ID"))?;
    let request = encode_binding_request(&transaction_id, change_request_flags);
    socket.send_to(&request, target).await?;

    let mut response = [0u8; STUN_RESPONSE_MAX_LEN];
    // Loop reading until we get a response that matches our transaction
    // ID or the deadline elapses. STUN servers occasionally retransmit
    // earlier replies, so a mismatched packet is not a fatal error.
    let received = loop {
        let outcome = timeout(per_request_timeout, socket.recv_from(&mut response)).await;
        let (len, src) = match outcome {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => return Err(NatDetectionError::Io(err)),
            Err(_) => return Err(NatDetectionError::Timeout),
        };
        let slice = response
            .get(..len)
            .ok_or(NatDetectionError::MalformedStunResponse("length"))?;
        if response_matches_transaction(slice, &transaction_id) {
            break (slice, src);
        }
        // Otherwise drop the stray packet and keep waiting; the timeout
        // bounds the loop.
    };

    let (response_slice, src) = received;
    let mut parsed = decode_binding_response(response_slice, &transaction_id)
        .map_err(NatDetectionError::MalformedStunResponse)?;
    // If the server omitted `RESPONSE-ORIGIN`, fall back to the packet's
    // source address — that's where the response demonstrably came from.
    if parsed.response_origin.is_none() {
        parsed.response_origin = Some(src);
    }
    Ok(parsed)
}

/// Send a Binding Request, retrying up to `config.retries` extra attempts.
async fn send_with_retries(
    socket: &UdpSocket,
    target: SocketAddr,
    change_request_flags: u32,
    config: &NatDetectionConfig,
) -> Result<BindingResponse, NatDetectionError> {
    let mut last_timeout = false;
    let attempts = config.retries.saturating_add(1);
    for _ in 0..attempts {
        match send_binding_request(
            socket,
            target,
            change_request_flags,
            config.per_request_timeout,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(NatDetectionError::Timeout) => {
                last_timeout = true;
            }
            Err(other) => return Err(other),
        }
    }
    if last_timeout {
        Err(NatDetectionError::Timeout)
    } else {
        Err(NatDetectionError::MalformedStunResponse("no attempts"))
    }
}

/// `RFC 5780 §4` two-server `NAT` detection.
///
/// Runs the standard probe sequence using `socket` for all four probes so
/// the caller can reuse the same socket for hole-punching afterwards:
///
/// 1. **`Test I`** — Binding Request to `config.primary`. Establishes the
///    baseline `XOR-MAPPED-ADDRESS`.
/// 2. **`Test II`** — Binding Request to `config.primary` with
///    `CHANGE-REQUEST = change IP + change port`. A response from a
///    different `IP`+port proves endpoint-independent filtering and
///    classifies the path as `Open` or `FullCone`.
/// 3. **`Test III`** — Binding Request to `config.secondary`. A different
///    `XOR-MAPPED-ADDRESS` here implies endpoint-dependent mapping and
///    classifies as `Symmetric`.
/// 4. **`Test IV`** — Binding Request to `config.primary` with
///    `CHANGE-REQUEST = change port` only. Distinguishes
///    `RestrictedCone` from `PortRestrictedCone` per `RFC 5780 §4.4`.
///
/// # Errors
///
/// Returns [`NatDetectionError::Timeout`] when `Test I` exhausts retries,
/// [`NatDetectionError::NoChangeRequestSupport`] when the server's
/// `Test II` reply came from the same address as `Test I` (so the
/// `CHANGE-REQUEST` attribute was not honoured), and
/// [`NatDetectionError::MalformedStunResponse`] when any response cannot
/// be parsed.
pub async fn detect_nat_type_rfc5780(
    socket: &UdpSocket,
    config: &NatDetectionConfig,
) -> Result<NatType, NatDetectionError> {
    // Test I — baseline binding against the primary server.
    let test_i = send_with_retries(socket, config.primary, 0, config).await?;

    // If the mapped address matches the socket's local address, the host
    // is on a public address; no NAT is in the path.
    let local_address = socket.local_addr()?;
    if address_matches_local(local_address, test_i.mapped) {
        return Ok(NatType::Open);
    }

    // Record the primary's `OTHER-ADDRESS` (where it claims it can
    // reply from when asked to change its source). When Test II
    // arrives, the responding origin should match it.
    let primary_alternate = test_i.other_address;

    // Test II — ask the primary to respond from a different IP+port.
    let test_ii_flags = CHANGE_REQUEST_FLAG_CHANGE_IP | CHANGE_REQUEST_FLAG_CHANGE_PORT;
    let test_ii_outcome = send_with_retries(socket, config.primary, test_ii_flags, config).await;
    let test_ii_origin_changed = match &test_ii_outcome {
        Ok(response) => {
            // The response came back. Verify it actually came from a
            // different address+port than the primary server.
            let origin =
                response
                    .response_origin
                    .ok_or(NatDetectionError::MalformedStunResponse(
                        "missing RESPONSE-ORIGIN",
                    ))?;
            if origin == config.primary {
                // Server returned a response but did not actually change
                // its source address. Treat this as a STUN server that
                // does not honour CHANGE-REQUEST.
                return Err(NatDetectionError::NoChangeRequestSupport);
            }
            // If the server advertised an OTHER-ADDRESS in Test I,
            // confirm the response came from there. A mismatch means
            // the server's advertisement is inconsistent — reject.
            if let Some(alternate) = primary_alternate
                && origin != alternate
            {
                return Err(NatDetectionError::MalformedStunResponse(
                    "Test II origin does not match OTHER-ADDRESS",
                ));
            }
            true
        }
        Err(NatDetectionError::Timeout) => false,
        Err(other) => return Err(NatDetectionError::MalformedStunResponse(error_label(other))),
    };

    if test_ii_origin_changed {
        // Filtering allows traffic from any external host once a mapping
        // exists — full cone NAT (or open Internet, but we ruled that
        // out above by comparing the mapped address).
        return Ok(NatType::FullCone);
    }

    // Test III — ask the secondary server for its view of the mapped
    // address. A different external port between Test I and Test III
    // implies the NAT is symmetric (mapping changes per destination).
    let test_iii = send_with_retries(socket, config.secondary, 0, config).await?;
    if test_iii.mapped != test_i.mapped {
        return Ok(NatType::Symmetric);
    }

    // Test IV — ask the primary to respond from the same IP but a
    // different port. If we hear back, only the destination IP needed
    // to match — that's address-restricted (RestrictedCone). If we
    // time out, both IP and port must match — port-restricted.
    let test_iv = send_with_retries(
        socket,
        config.primary,
        CHANGE_REQUEST_FLAG_CHANGE_PORT,
        config,
    )
    .await;
    match test_iv {
        Ok(_) => Ok(NatType::RestrictedCone),
        Err(NatDetectionError::Timeout) => Ok(NatType::PortRestrictedCone),
        Err(other) => Err(NatDetectionError::MalformedStunResponse(error_label(
            &other,
        ))),
    }
}

/// Map an arbitrary `NatDetectionError` to a static label for re-wrapping
/// under [`NatDetectionError::MalformedStunResponse`]. Used where the
/// outer probe semantically only allows `MalformedStunResponse`.
const fn error_label(err: &NatDetectionError) -> &'static str {
    match err {
        NatDetectionError::Io(_) => "I/O error during probe",
        NatDetectionError::Timeout => "timeout during probe",
        NatDetectionError::MalformedStunResponse(reason) => reason,
        NatDetectionError::NoChangeRequestSupport => "CHANGE-REQUEST unsupported",
    }
}

/// Does the externally-mapped address match the local socket's address?
///
/// `local_address` may carry an unspecified `IP` (the typical result of
/// binding to `0.0.0.0`); in that case the address is considered to
/// match when only the port agrees. This is the same rule the
/// single-server probe uses.
fn address_matches_local(local_address: SocketAddr, mapped: SocketAddr) -> bool {
    if local_address.port() != mapped.port() {
        return false;
    }
    if local_address.ip().is_unspecified() {
        return true;
    }
    local_address.ip() == mapped.ip()
}

fn response_matches_transaction(
    response: &[u8],
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
) -> bool {
    let Some(slice) = response.get(8..STUN_HEADER_LEN) else {
        return false;
    };
    slice == transaction_id
}

async fn resolve_stun_server(stun_server: &str) -> Result<SocketAddr> {
    let mut addresses = tokio::net::lookup_host(stun_server).await?;
    addresses
        .next()
        .ok_or_else(|| anyhow::anyhow!("STUN server resolved to no socket addresses"))
}

/// Encode a Binding Request with an optional `CHANGE-REQUEST` attribute.
///
/// `change_request_flags` is the four-byte attribute value (bit 2 =
/// change `IP`, bit 1 = change port). When zero, the attribute is
/// omitted and the request is the bare 20-byte header.
fn encode_binding_request(
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
    change_request_flags: u32,
) -> Vec<u8> {
    let attribute_len: u16 = if change_request_flags == 0 {
        0
    } else {
        // STUN_ATTRIBUTE_HEADER_LEN (4) + STUN_CHANGE_REQUEST_VALUE_LEN (4) = 8,
        // which fits in u16 trivially.
        let header_u16 = u16::try_from(STUN_ATTRIBUTE_HEADER_LEN).unwrap_or(u16::MAX);
        header_u16.saturating_add(STUN_CHANGE_REQUEST_VALUE_LEN)
    };
    let capacity = STUN_HEADER_LEN.saturating_add(usize::from(attribute_len));
    let mut request = Vec::with_capacity(capacity);
    request.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    request.extend_from_slice(&attribute_len.to_be_bytes());
    request.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    request.extend_from_slice(transaction_id);

    if change_request_flags != 0 {
        request.extend_from_slice(&CHANGE_REQUEST.to_be_bytes());
        request.extend_from_slice(&STUN_CHANGE_REQUEST_VALUE_LEN.to_be_bytes());
        request.extend_from_slice(&change_request_flags.to_be_bytes());
    }
    request
}

fn decode_binding_response(
    response: &[u8],
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
) -> Result<BindingResponse, &'static str> {
    if response.len() < STUN_HEADER_LEN {
        return Err("response too short for header");
    }

    let message_type_bytes: [u8; 2] = response
        .get(0..2)
        .and_then(|s| s.try_into().ok())
        .ok_or("message type out of bounds")?;
    let message_type = u16::from_be_bytes(message_type_bytes);
    if message_type != BINDING_SUCCESS_RESPONSE {
        return Err("unexpected message type");
    }

    let message_len_bytes: [u8; 2] = response
        .get(2..4)
        .and_then(|s| s.try_into().ok())
        .ok_or("message length out of bounds")?;
    let message_len = usize::from(u16::from_be_bytes(message_len_bytes));

    let magic_cookie_bytes: [u8; 4] = response
        .get(4..8)
        .and_then(|s| s.try_into().ok())
        .ok_or("magic cookie out of bounds")?;
    let magic_cookie = u32::from_be_bytes(magic_cookie_bytes);
    if magic_cookie != STUN_MAGIC_COOKIE {
        return Err("invalid magic cookie");
    }

    if response
        .get(8..STUN_HEADER_LEN)
        .ok_or("missing transaction ID")?
        != transaction_id
    {
        return Err("transaction ID mismatch");
    }

    if response.len() < STUN_HEADER_LEN.saturating_add(message_len) {
        return Err("attributes truncated");
    }

    let mut offset = STUN_HEADER_LEN;
    let attributes_end = STUN_HEADER_LEN.saturating_add(message_len);
    let mut mapped: Option<SocketAddr> = None;
    let mut response_origin: Option<SocketAddr> = None;
    let mut other_address: Option<SocketAddr> = None;
    while offset.saturating_add(STUN_ATTRIBUTE_HEADER_LEN) <= attributes_end {
        let header_end = offset.saturating_add(STUN_ATTRIBUTE_HEADER_LEN);
        let attr_header: &[u8; STUN_ATTRIBUTE_HEADER_LEN] = response
            .get(offset..header_end)
            .and_then(|s| s.try_into().ok())
            .ok_or("attribute header out of bounds")?;
        let attribute_type = u16::from_be_bytes([attr_header[0], attr_header[1]]);
        let attribute_len = usize::from(u16::from_be_bytes([attr_header[2], attr_header[3]]));
        let value_start = header_end;
        let value_end = value_start.saturating_add(attribute_len);
        if value_end > attributes_end {
            return Err("attribute truncated");
        }

        let value = response
            .get(value_start..value_end)
            .ok_or("attribute value out of bounds")?;

        match attribute_type {
            XOR_MAPPED_ADDRESS => {
                mapped = Some(decode_xor_mapped_address(value, transaction_id)?);
            }
            RESPONSE_ORIGIN => {
                response_origin = Some(decode_plain_address(value)?);
            }
            OTHER_ADDRESS => {
                other_address = Some(decode_plain_address(value)?);
            }
            _ => {
                // Unknown attribute; ignore and keep parsing.
            }
        }

        offset = value_end.saturating_add(padding_for(attribute_len));
    }

    let mapped = mapped.ok_or("missing XOR-MAPPED-ADDRESS")?;
    Ok(BindingResponse {
        mapped,
        response_origin,
        other_address,
    })
}

fn decode_xor_mapped_address(
    value: &[u8],
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
) -> Result<SocketAddr, &'static str> {
    if value.len() < STUN_IPV4_ATTRIBUTE_VALUE_LEN {
        return Err("XOR-MAPPED-ADDRESS attribute too short");
    }

    let family = value.get(1).copied().ok_or("missing family byte")?;
    let x_port_bytes: [u8; 2] = value
        .get(2..4)
        .and_then(|s| s.try_into().ok())
        .ok_or("missing port")?;
    let x_port = u16::from_be_bytes(x_port_bytes);
    // `STUN_MAGIC_COOKIE >> 16` is always a 16-bit value, so the
    // truncation here is intentional and safe.
    let cookie_high: u16 = u16::try_from(STUN_MAGIC_COOKIE >> 16).map_err(|_| "cookie overflow")?;
    let port = x_port ^ cookie_high;

    match family {
        STUN_FAMILY_IPV4 => {
            if value.len() != STUN_IPV4_ATTRIBUTE_VALUE_LEN {
                return Err("invalid IPv4 XOR-MAPPED-ADDRESS length");
            }
            let x_addr_bytes: [u8; 4] = value
                .get(4..8)
                .and_then(|s| s.try_into().ok())
                .ok_or("missing IPv4 address")?;
            let x_addr = u32::from_be_bytes(x_addr_bytes);
            let addr = x_addr ^ STUN_MAGIC_COOKIE;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port))
        }
        STUN_FAMILY_IPV6 => {
            if value.len() != STUN_IPV6_ATTRIBUTE_VALUE_LEN {
                return Err("invalid IPv6 XOR-MAPPED-ADDRESS length");
            }
            let mut xor_key = [0u8; STUN_IPV6_ADDR_LEN];
            xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
            xor_key[4..].copy_from_slice(transaction_id);

            let ipv6_bytes: &[u8; STUN_IPV6_ADDR_LEN] = value
                .get(4..4 + STUN_IPV6_ADDR_LEN)
                .and_then(|s| s.try_into().ok())
                .ok_or("missing IPv6 address")?;
            let mut address_bytes = [0u8; STUN_IPV6_ADDR_LEN];
            for (byte, (raw, key)) in address_bytes
                .iter_mut()
                .zip(ipv6_bytes.iter().zip(xor_key.iter()))
            {
                *byte = raw ^ key;
            }
            Ok(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(address_bytes)),
                port,
            ))
        }
        _ => Err("unsupported XOR-MAPPED-ADDRESS family"),
    }
}

/// Decode a plain (non-`XOR`ed) `MAPPED-ADDRESS` style attribute value.
///
/// `RESPONSE-ORIGIN` and `OTHER-ADDRESS` share the same wire format as
/// the legacy `MAPPED-ADDRESS` attribute defined by `RFC 5389 §15.1`:
/// one reserved byte, one address-family byte, two port bytes, then
/// the address bytes (`4` for `IPv4`, `16` for `IPv6`).
fn decode_plain_address(value: &[u8]) -> Result<SocketAddr, &'static str> {
    if value.len() < STUN_IPV4_ATTRIBUTE_VALUE_LEN {
        return Err("address attribute too short");
    }

    let family = value.get(1).copied().ok_or("missing family byte")?;
    let port_bytes: [u8; 2] = value
        .get(2..4)
        .and_then(|s| s.try_into().ok())
        .ok_or("missing port")?;
    let port = u16::from_be_bytes(port_bytes);

    match family {
        STUN_FAMILY_IPV4 => {
            if value.len() != STUN_IPV4_ATTRIBUTE_VALUE_LEN {
                return Err("invalid IPv4 address attribute length");
            }
            let addr_bytes: [u8; 4] = value
                .get(4..8)
                .and_then(|s| s.try_into().ok())
                .ok_or("missing IPv4 address bytes")?;
            Ok(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(addr_bytes)),
                port,
            ))
        }
        STUN_FAMILY_IPV6 => {
            if value.len() != STUN_IPV6_ATTRIBUTE_VALUE_LEN {
                return Err("invalid IPv6 address attribute length");
            }
            let addr_bytes: [u8; STUN_IPV6_ADDR_LEN] = value
                .get(4..4 + STUN_IPV6_ADDR_LEN)
                .and_then(|s| s.try_into().ok())
                .ok_or("missing IPv6 address bytes")?;
            Ok(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(addr_bytes)),
                port,
            ))
        }
        _ => Err("unsupported address family"),
    }
}

const fn padding_for(attribute_len: usize) -> usize {
    (STUN_ATTRIBUTE_HEADER_LEN - (attribute_len % STUN_ATTRIBUTE_HEADER_LEN))
        % STUN_ATTRIBUTE_HEADER_LEN
}

/// Monotonic counter mixed into every transaction ID so two requests
/// issued in the same nanosecond cannot collide. `SystemTime::now()` has
/// nanosecond resolution but is not guaranteed to advance between
/// adjacent calls; under the RFC 5780 four-test sequence we issue
/// requests back-to-back within microseconds, well within the same
/// nanosecond on some platforms. The counter monotonically increases
/// per-process and gives us 64 bits of uniqueness independent of the
/// clock.
static TRANSACTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Generate a 96-bit STUN transaction ID. RFC 5389 §6 requires
/// transactions to be uniquely identified; the response-matching code
/// (see [`response_matches_transaction`]) needs distinct IDs across any
/// two in-flight requests. We satisfy that by mixing a per-process
/// monotonic counter, the system clock, and the process ID — the
/// counter guarantees uniqueness even under same-nanosecond bursts,
/// the timestamp provides cross-process uniqueness without coordination,
/// and the PID adds a final disambiguator between concurrent processes
/// that happen to start in the same nanosecond.
fn transaction_id() -> Result<[u8; STUN_TRANSACTION_ID_LEN]> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let pid = u128::from(std::process::id());
    let counter =
        u128::from(TRANSACTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let mixed = timestamp ^ (pid << 64) ^ (counter << 32);
    let bytes = mixed.to_be_bytes();
    let offset = bytes.len() - STUN_TRANSACTION_ID_LEN;
    let mut transaction_id = [0u8; STUN_TRANSACTION_ID_LEN];
    transaction_id.copy_from_slice(
        bytes
            .get(offset..)
            .ok_or_else(|| anyhow::anyhow!("transaction ID offset out of range"))?,
    );
    Ok(transaction_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use tokio::net::UdpSocket;

    /// How a mock STUN server should react to a single Binding Request.
    #[derive(Debug, Clone, Copy)]
    enum MockAction {
        /// Reply with the given mapped/origin/other attributes.
        Reply {
            mapped: SocketAddr,
            response_origin: Option<SocketAddr>,
            other_address: Option<SocketAddr>,
        },
        /// Drop the request — do not reply.
        Drop,
    }

    /// A mock STUN server bound on `127.0.0.1`. The server runs as a
    /// background task that invokes the caller-supplied handler for
    /// every incoming Binding Request.
    #[derive(Debug)]
    struct MockStun {
        socket: Arc<UdpSocket>,
        address: SocketAddr,
    }

    impl MockStun {
        async fn bind() -> Self {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            Self {
                socket: Arc::new(socket),
                address,
            }
        }

        fn addr(&self) -> SocketAddr {
            self.address
        }

        /// Run a per-request handler in a background task. The server
        /// runs until the handler returns `None` or the test ends.
        fn spawn<H>(&self, mut handler: H)
        where
            H: FnMut(BindingRequestInfo) -> Option<MockAction> + Send + 'static,
        {
            let socket = self.socket.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; STUN_RESPONSE_MAX_LEN];
                loop {
                    let (len, peer) = match socket.recv_from(&mut buf).await {
                        Ok(pair) => pair,
                        Err(_) => return,
                    };
                    let Some(slice) = buf.get(..len) else {
                        continue;
                    };
                    let Some(info) = parse_request(slice) else {
                        continue;
                    };
                    let Some(action) = handler(info) else {
                        return;
                    };
                    match action {
                        MockAction::Reply {
                            mapped,
                            response_origin,
                            other_address,
                        } => {
                            let response = encode_binding_response(
                                &info.transaction_id,
                                mapped,
                                response_origin,
                                other_address,
                            );
                            // On loopback we cannot bind from arbitrary
                            // source IPs, so the response_origin is
                            // signalled via the attribute payload.
                            let _ = socket.send_to(&response, peer).await;
                        }
                        MockAction::Drop => {}
                    }
                }
            });
        }
    }

    /// Parsed metadata about an incoming Binding Request.
    #[derive(Debug, Clone, Copy)]
    struct BindingRequestInfo {
        transaction_id: [u8; STUN_TRANSACTION_ID_LEN],
        change_request_flags: u32,
    }

    /// Parse a Binding Request and extract the transaction ID and any
    /// CHANGE-REQUEST flags.
    fn parse_request(packet: &[u8]) -> Option<BindingRequestInfo> {
        if packet.len() < STUN_HEADER_LEN {
            return None;
        }
        let mtype = u16::from_be_bytes([*packet.first()?, *packet.get(1)?]);
        if mtype != BINDING_REQUEST {
            return None;
        }
        let cookie = u32::from_be_bytes(packet.get(4..8)?.try_into().ok()?);
        if cookie != STUN_MAGIC_COOKIE {
            return None;
        }
        let transaction_id: [u8; STUN_TRANSACTION_ID_LEN] =
            packet.get(8..STUN_HEADER_LEN)?.try_into().ok()?;

        let mlen = usize::from(u16::from_be_bytes(packet.get(2..4)?.try_into().ok()?));
        let attrs_end = STUN_HEADER_LEN.checked_add(mlen)?;
        let mut offset = STUN_HEADER_LEN;
        let mut change_request_flags = 0u32;
        while offset + STUN_ATTRIBUTE_HEADER_LEN <= attrs_end
            && offset + STUN_ATTRIBUTE_HEADER_LEN <= packet.len()
        {
            let header = packet.get(offset..offset + STUN_ATTRIBUTE_HEADER_LEN)?;
            let attr_type = u16::from_be_bytes([*header.first()?, *header.get(1)?]);
            let attr_len = usize::from(u16::from_be_bytes([*header.get(2)?, *header.get(3)?]));
            let value_start = offset + STUN_ATTRIBUTE_HEADER_LEN;
            let value_end = value_start.checked_add(attr_len)?;
            if value_end > attrs_end {
                return None;
            }
            if attr_type == CHANGE_REQUEST && attr_len == usize::from(STUN_CHANGE_REQUEST_VALUE_LEN)
            {
                let raw: [u8; 4] = packet.get(value_start..value_end)?.try_into().ok()?;
                change_request_flags = u32::from_be_bytes(raw);
            }
            offset = value_end + padding_for(attr_len);
        }
        Some(BindingRequestInfo {
            transaction_id,
            change_request_flags,
        })
    }

    /// Build an XOR-MAPPED-ADDRESS attribute value for the given socket.
    fn encode_xor_mapped(
        mapped: SocketAddr,
        transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
    ) -> Vec<u8> {
        let mut value = Vec::new();
        value.push(0); // reserved
        let cookie_high = (STUN_MAGIC_COOKIE >> 16) as u16;
        match mapped {
            SocketAddr::V4(addr) => {
                value.push(STUN_FAMILY_IPV4);
                let port = addr.port() ^ cookie_high;
                value.extend_from_slice(&port.to_be_bytes());
                let ip = u32::from(*addr.ip()) ^ STUN_MAGIC_COOKIE;
                value.extend_from_slice(&ip.to_be_bytes());
            }
            SocketAddr::V6(addr) => {
                value.push(STUN_FAMILY_IPV6);
                let port = addr.port() ^ cookie_high;
                value.extend_from_slice(&port.to_be_bytes());
                let mut xor_key = [0u8; STUN_IPV6_ADDR_LEN];
                xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
                xor_key[4..].copy_from_slice(transaction_id);
                for (idx, byte) in addr.ip().octets().iter().enumerate() {
                    value.push(byte ^ xor_key[idx]);
                }
            }
        }
        value
    }

    /// Build a plain MAPPED-ADDRESS-style attribute value (used by
    /// RESPONSE-ORIGIN and OTHER-ADDRESS).
    fn encode_plain(mapped: SocketAddr) -> Vec<u8> {
        let mut value = Vec::new();
        value.push(0);
        match mapped {
            SocketAddr::V4(addr) => {
                value.push(STUN_FAMILY_IPV4);
                value.extend_from_slice(&addr.port().to_be_bytes());
                value.extend_from_slice(&addr.ip().octets());
            }
            SocketAddr::V6(addr) => {
                value.push(STUN_FAMILY_IPV6);
                value.extend_from_slice(&addr.port().to_be_bytes());
                value.extend_from_slice(&addr.ip().octets());
            }
        }
        value
    }

    /// Append a single STUN attribute to `out`.
    fn push_attribute(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
        out.extend_from_slice(&attr_type.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
        let pad = padding_for(value.len());
        for _ in 0..pad {
            out.push(0);
        }
    }

    /// Build a Binding Success Response with the given attributes.
    fn encode_binding_response(
        transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
        mapped: SocketAddr,
        response_origin: Option<SocketAddr>,
        other_address: Option<SocketAddr>,
    ) -> Vec<u8> {
        let mut attributes = Vec::new();
        push_attribute(
            &mut attributes,
            XOR_MAPPED_ADDRESS,
            &encode_xor_mapped(mapped, transaction_id),
        );
        if let Some(origin) = response_origin {
            push_attribute(&mut attributes, RESPONSE_ORIGIN, &encode_plain(origin));
        }
        if let Some(other) = other_address {
            push_attribute(&mut attributes, OTHER_ADDRESS, &encode_plain(other));
        }

        let mut packet = Vec::with_capacity(STUN_HEADER_LEN + attributes.len());
        packet.extend_from_slice(&BINDING_SUCCESS_RESPONSE.to_be_bytes());
        packet.extend_from_slice(&(attributes.len() as u16).to_be_bytes());
        packet.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        packet.extend_from_slice(transaction_id);
        packet.extend(attributes);
        packet
    }

    /// Build a detection config with short timeouts so retry-exhausted
    /// tests run quickly.
    fn quick_config(primary: SocketAddr, secondary: SocketAddr) -> NatDetectionConfig {
        NatDetectionConfig {
            primary,
            secondary,
            per_request_timeout: Duration::from_millis(150),
            retries: 1,
        }
    }

    // ---------- legacy single-server tests ----------

    #[tokio::test]
    async fn external_address_reads_xor_mapped_address_from_stun_response() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();
        let external_address = SocketAddr::from(([203, 0, 113, 10], 54321));

        tokio::spawn(async move {
            let mut request = [0u8; STUN_RESPONSE_MAX_LEN];
            let (received, peer) = server.recv_from(&mut request).await.unwrap();
            let transaction_id: [u8; STUN_TRANSACTION_ID_LEN] =
                request[8..STUN_HEADER_LEN].try_into().unwrap();
            let response = encode_binding_response(&transaction_id, external_address, None, None);
            assert_eq!(
                u16::from_be_bytes([request[0], request[1]]),
                BINDING_REQUEST
            );
            assert_eq!(received, STUN_HEADER_LEN);
            server.send_to(&response, peer).await.unwrap();
        });

        let observed = NatTraversal::external_address(&server_address.to_string())
            .await
            .unwrap();

        assert_eq!(observed, external_address);
    }

    #[tokio::test]
    async fn detect_nat_type_reports_open_when_mapping_matches_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();

        tokio::spawn(async move {
            let mut request = [0u8; STUN_RESPONSE_MAX_LEN];
            let (_received, peer) = server.recv_from(&mut request).await.unwrap();
            let transaction_id: [u8; STUN_TRANSACTION_ID_LEN] =
                request[8..STUN_HEADER_LEN].try_into().unwrap();
            let response = encode_binding_response(&transaction_id, peer, None, None);
            server.send_to(&response, peer).await.unwrap();
        });

        let nat_type = NatTraversal::detect_nat_type(&server_address.to_string())
            .await
            .unwrap();

        assert_eq!(nat_type, NatType::Open);
    }

    #[test]
    fn decode_binding_response_rejects_wrong_transaction_id() {
        let transaction_id = [0xAA; STUN_TRANSACTION_ID_LEN];
        let response = encode_binding_response(
            &transaction_id,
            SocketAddr::from(([198, 51, 100, 7], 22000)),
            None,
            None,
        );
        let wrong_transaction_id = [0xBB; STUN_TRANSACTION_ID_LEN];

        let result = decode_binding_response(&response, &wrong_transaction_id);

        assert!(result.is_err());
    }

    #[test]
    fn decode_binding_response_extracts_response_origin_and_other_address() {
        let transaction_id = [0xCC; STUN_TRANSACTION_ID_LEN];
        let mapped = SocketAddr::from(([198, 51, 100, 7], 22000));
        let origin = SocketAddr::from(([198, 51, 100, 1], 3478));
        let other = SocketAddr::from(([198, 51, 100, 2], 3479));
        let response = encode_binding_response(&transaction_id, mapped, Some(origin), Some(other));

        let parsed = decode_binding_response(&response, &transaction_id).unwrap();

        assert_eq!(parsed.mapped, mapped);
        assert_eq!(parsed.response_origin, Some(origin));
        assert_eq!(parsed.other_address, Some(other));
    }

    // ---------- RFC 5780 two-server detection tests ----------

    #[tokio::test]
    async fn detects_open_internet_when_xmapped_matches_socket_addr() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = probe_socket.local_addr().unwrap();

        let primary_addr = primary.addr();
        primary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped: local,
                response_origin: Some(primary_addr),
                other_address: None,
            })
        });
        let secondary_addr = secondary.addr();
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped: local,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await
                .unwrap();
        assert_eq!(result, NatType::Open);
    }

    #[tokio::test]
    async fn detects_full_cone_when_change_request_succeeds() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let primary_addr = primary.addr();
        let secondary_addr = secondary.addr();
        // The primary advertises its alternate as the secondary's address
        // — that's where Test II will appear to come from.
        let alternate = secondary_addr;
        // Externally-mapped address, deliberately different from the
        // socket's local address so the algorithm does not short-circuit
        // to `Open`.
        let mapped = SocketAddr::from(([203, 0, 113, 50], 60001));

        primary.spawn(move |info| {
            if info.change_request_flags == 0 {
                Some(MockAction::Reply {
                    mapped,
                    response_origin: Some(primary_addr),
                    other_address: Some(alternate),
                })
            } else {
                // Test II — pretend we replied from the alternate address.
                Some(MockAction::Reply {
                    mapped,
                    response_origin: Some(alternate),
                    other_address: Some(primary_addr),
                })
            }
        });
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await
                .unwrap();
        assert_eq!(result, NatType::FullCone);
    }

    #[tokio::test]
    async fn detects_symmetric_when_xmapped_differs_between_servers() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let primary_addr = primary.addr();
        let secondary_addr = secondary.addr();
        let alternate = SocketAddr::from(([127, 0, 0, 9], 3479));

        // Test I and Test III mapped addresses differ — the NAT is
        // creating a fresh mapping per destination.
        let mapped_primary = SocketAddr::from(([203, 0, 113, 50], 60001));
        let mapped_secondary = SocketAddr::from(([203, 0, 113, 50], 60002));

        primary.spawn(move |info| {
            if info.change_request_flags == 0 {
                Some(MockAction::Reply {
                    mapped: mapped_primary,
                    response_origin: Some(primary_addr),
                    other_address: Some(alternate),
                })
            } else {
                // Test II — drop, simulating the NAT filtering the
                // unsolicited response from a new IP+port.
                Some(MockAction::Drop)
            }
        });
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped: mapped_secondary,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await
                .unwrap();
        assert_eq!(result, NatType::Symmetric);
    }

    #[tokio::test]
    async fn detects_restricted_cone_when_test_iv_succeeds() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let primary_addr = primary.addr();
        let secondary_addr = secondary.addr();
        let alternate = SocketAddr::from(([127, 0, 0, 9], 3479));
        let mapped = SocketAddr::from(([203, 0, 113, 50], 60001));

        primary.spawn(move |info| match info.change_request_flags {
            0 => Some(MockAction::Reply {
                mapped,
                response_origin: Some(primary_addr),
                other_address: Some(alternate),
            }),
            // Test II (change IP + port) — drop. NAT does not let
            // unsolicited traffic from a new IP through.
            flags if flags & CHANGE_REQUEST_FLAG_CHANGE_IP != 0 => Some(MockAction::Drop),
            // Test IV (change port only) — reply. Filtering is
            // address-restricted but not port-restricted.
            _ => Some(MockAction::Reply {
                mapped,
                response_origin: Some(primary_addr),
                other_address: Some(alternate),
            }),
        });
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await
                .unwrap();
        assert_eq!(result, NatType::RestrictedCone);
    }

    #[tokio::test]
    async fn detects_port_restricted_cone_when_test_iv_times_out() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let primary_addr = primary.addr();
        let secondary_addr = secondary.addr();
        let alternate = SocketAddr::from(([127, 0, 0, 9], 3479));
        let mapped = SocketAddr::from(([203, 0, 113, 50], 60001));

        primary.spawn(move |info| {
            if info.change_request_flags == 0 {
                Some(MockAction::Reply {
                    mapped,
                    response_origin: Some(primary_addr),
                    other_address: Some(alternate),
                })
            } else {
                // Any CHANGE-REQUEST drops. Test II and Test IV both
                // time out — the NAT requires both IP and port to
                // match: port-restricted.
                Some(MockAction::Drop)
            }
        });
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await
                .unwrap();
        assert_eq!(result, NatType::PortRestrictedCone);
    }

    #[tokio::test]
    async fn times_out_when_primary_server_unreachable() {
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Bind sockets, capture their addresses, then drop so the ports
        // are closed. Sending to them yields ICMP unreachable on some
        // platforms and silent drop on others — both surface as a
        // timeout from the probe's perspective.
        let dead_primary = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_primary_addr = dead_primary.local_addr().unwrap();
        drop(dead_primary);
        let dead_secondary = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_secondary_addr = dead_secondary.local_addr().unwrap();
        drop(dead_secondary);

        let result = detect_nat_type_rfc5780(
            &probe_socket,
            &quick_config(dead_primary_addr, dead_secondary_addr),
        )
        .await;
        assert!(matches!(result, Err(NatDetectionError::Timeout)));
    }

    #[tokio::test]
    async fn flags_no_change_request_support_when_origin_matches_primary() {
        let primary = MockStun::bind().await;
        let secondary = MockStun::bind().await;
        let probe_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let primary_addr = primary.addr();
        let secondary_addr = secondary.addr();
        let alternate = SocketAddr::from(([127, 0, 0, 9], 3479));
        let mapped = SocketAddr::from(([203, 0, 113, 50], 60001));

        // Primary replies to both Test I and Test II from the same
        // address — CHANGE-REQUEST was ignored.
        primary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped,
                response_origin: Some(primary_addr),
                other_address: Some(alternate),
            })
        });
        secondary.spawn(move |_info| {
            Some(MockAction::Reply {
                mapped,
                response_origin: Some(secondary_addr),
                other_address: None,
            })
        });

        let result =
            detect_nat_type_rfc5780(&probe_socket, &quick_config(primary_addr, secondary_addr))
                .await;
        assert!(matches!(
            result,
            Err(NatDetectionError::NoChangeRequestSupport)
        ));
    }
}
