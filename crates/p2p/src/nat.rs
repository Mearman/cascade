//! STUN-based NAT traversal helpers.
//!
//! The STUN client implements RFC 5389 Binding Requests and parses
//! XOR-MAPPED-ADDRESS from the response. Full NAT classification needs a STUN
//! server pair with change-request support; with the single-server API exposed
//! here, Cascade reports `Public` when the mapped address matches the local
//! socket and conservatively reports `Symmetric` otherwise so callers know to
//! use relay fallback.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::time::{Duration, timeout};

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS_RESPONSE: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_HEADER_LEN: usize = 20;
const STUN_TRANSACTION_ID_LEN: usize = 12;
const STUN_ATTRIBUTE_HEADER_LEN: usize = 4;
const STUN_IPV6_ADDR_LEN: usize = 16;
const STUN_IPV4_ATTRIBUTE_VALUE_LEN: usize = 8;
const STUN_IPV6_ATTRIBUTE_VALUE_LEN: usize = 20;
const STUN_FAMILY_IPV4: u8 = 0x01;
const STUN_FAMILY_IPV6: u8 = 0x02;
const STUN_RESPONSE_MAX_LEN: usize = 1500;
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

/// NAT type detected by STUN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatType {
    /// No NAT — directly reachable.
    Public,
    /// Full cone NAT — mapped port reachable from any external host.
    FullCone,
    /// Restricted cone NAT — only reachable from hosts that have received packets.
    RestrictedCone,
    /// Port-restricted cone NAT — like restricted but port-specific.
    PortRestrictedCone,
    /// Symmetric NAT — different mapping per destination. Requires relay.
    Symmetric,
}

/// NAT traversal coordinator.
#[derive(Debug, Clone, Copy)]
pub struct NatTraversal;

impl NatTraversal {
    /// Detect the local NAT type using STUN.
    pub async fn detect_nat_type(stun_server: &str) -> Result<NatType> {
        let (local_address, external_address) = stun_binding_request(stun_server).await?;
        if local_address.ip() == external_address.ip()
            && local_address.port() == external_address.port()
        {
            Ok(NatType::Public)
        } else {
            Ok(NatType::Symmetric)
        }
    }

    /// Get the external address as seen by a STUN server.
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
    let request = encode_binding_request(&transaction_id);
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
    let external_address = decode_binding_response(response_slice, &transaction_id)?;
    let local_address = socket
        .local_addr()
        .context("reading STUN socket local address")?;

    Ok((local_address, external_address))
}

async fn resolve_stun_server(stun_server: &str) -> Result<SocketAddr> {
    let mut addresses = tokio::net::lookup_host(stun_server).await?;
    addresses
        .next()
        .ok_or_else(|| anyhow::anyhow!("STUN server resolved to no socket addresses"))
}

fn encode_binding_request(transaction_id: &[u8; STUN_TRANSACTION_ID_LEN]) -> [u8; STUN_HEADER_LEN] {
    let mut request = [0u8; STUN_HEADER_LEN];
    request[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    request[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    request[8..STUN_HEADER_LEN].copy_from_slice(transaction_id);
    request
}

fn decode_binding_response(
    response: &[u8],
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
) -> Result<SocketAddr> {
    if response.len() < STUN_HEADER_LEN {
        anyhow::bail!("STUN response too short");
    }

    let message_type = u16::from_be_bytes(
        response
            .get(0..2)
            .ok_or_else(|| anyhow::anyhow!("STUN response too short for message type"))?
            .try_into()?,
    );
    if message_type != BINDING_SUCCESS_RESPONSE {
        anyhow::bail!("unexpected STUN message type {message_type:#06x}");
    }

    let message_len = usize::from(u16::from_be_bytes(
        response
            .get(2..4)
            .ok_or_else(|| anyhow::anyhow!("STUN response too short for message length"))?
            .try_into()?,
    ));
    let magic_cookie = u32::from_be_bytes(
        response
            .get(4..8)
            .ok_or_else(|| anyhow::anyhow!("STUN response too short for magic cookie"))?
            .try_into()?,
    );
    if magic_cookie != STUN_MAGIC_COOKIE {
        anyhow::bail!("invalid STUN magic cookie {magic_cookie:#010x}");
    }

    if response
        .get(8..STUN_HEADER_LEN)
        .ok_or_else(|| anyhow::anyhow!("STUN response too short for transaction ID"))?
        != transaction_id
    {
        anyhow::bail!("STUN transaction ID mismatch");
    }

    if response.len() < STUN_HEADER_LEN + message_len {
        anyhow::bail!("STUN attributes truncated");
    }

    let mut offset = STUN_HEADER_LEN;
    let attributes_end = STUN_HEADER_LEN + message_len;
    while offset + STUN_ATTRIBUTE_HEADER_LEN <= attributes_end {
        let attr_header: &[u8; STUN_ATTRIBUTE_HEADER_LEN] = response
            .get(offset..offset + STUN_ATTRIBUTE_HEADER_LEN)
            .ok_or_else(|| anyhow::anyhow!("STUN attribute header out of bounds"))?
            .try_into()?;
        let attribute_type = u16::from_be_bytes([attr_header[0], attr_header[1]]);
        let attribute_len = usize::from(u16::from_be_bytes([attr_header[2], attr_header[3]]));
        let value_start = offset + STUN_ATTRIBUTE_HEADER_LEN;
        let value_end = value_start + attribute_len;
        if value_end > attributes_end {
            anyhow::bail!("STUN attribute truncated");
        }

        if attribute_type == XOR_MAPPED_ADDRESS {
            let value = response
                .get(value_start..value_end)
                .ok_or_else(|| anyhow::anyhow!("STUN XOR-MAPPED-ADDRESS value out of bounds"))?;
            return decode_xor_mapped_address(value, transaction_id);
        }

        offset = value_end + padding_for(attribute_len);
    }

    anyhow::bail!("STUN response did not include XOR-MAPPED-ADDRESS")
}

fn decode_xor_mapped_address(
    value: &[u8],
    transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
) -> Result<SocketAddr> {
    if value.len() < STUN_IPV4_ATTRIBUTE_VALUE_LEN {
        anyhow::bail!("XOR-MAPPED-ADDRESS attribute too short");
    }

    let family = value
        .get(1)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("XOR-MAPPED-ADDRESS too short for family byte"))?;
    let x_port = u16::from_be_bytes(
        value
            .get(2..4)
            .ok_or_else(|| anyhow::anyhow!("XOR-MAPPED-ADDRESS too short for port"))?
            .try_into()?,
    );
    let port = x_port ^ ((STUN_MAGIC_COOKIE >> 16) as u16);

    match family {
        STUN_FAMILY_IPV4 => {
            if value.len() != STUN_IPV4_ATTRIBUTE_VALUE_LEN {
                anyhow::bail!("invalid IPv4 XOR-MAPPED-ADDRESS length");
            }
            let x_addr = u32::from_be_bytes(
                value
                    .get(4..8)
                    .ok_or_else(|| anyhow::anyhow!("XOR-MAPPED-ADDRESS too short for IPv4 address"))?
                    .try_into()?,
            );
            let addr = x_addr ^ STUN_MAGIC_COOKIE;
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port))
        }
        STUN_FAMILY_IPV6 => {
            if value.len() != STUN_IPV6_ATTRIBUTE_VALUE_LEN {
                anyhow::bail!("invalid IPv6 XOR-MAPPED-ADDRESS length");
            }
            let mut xor_key = [0u8; STUN_IPV6_ADDR_LEN];
            xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
            xor_key[4..].copy_from_slice(transaction_id);

            let ipv6_bytes: &[u8; STUN_IPV6_ADDR_LEN] = value
                .get(4..4 + STUN_IPV6_ADDR_LEN)
                .ok_or_else(|| anyhow::anyhow!("XOR-MAPPED-ADDRESS too short for IPv6 address"))?
                .try_into()?;
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
        _ => anyhow::bail!("unsupported XOR-MAPPED-ADDRESS family {family}"),
    }
}

const fn padding_for(attribute_len: usize) -> usize {
    (STUN_ATTRIBUTE_HEADER_LEN - (attribute_len % STUN_ATTRIBUTE_HEADER_LEN))
        % STUN_ATTRIBUTE_HEADER_LEN
}

fn transaction_id() -> Result<[u8; STUN_TRANSACTION_ID_LEN]> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let pid = u128::from(std::process::id());
    let mixed = timestamp ^ (pid << 64);
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

    use tokio::net::UdpSocket;

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
            let response = encode_binding_response(&transaction_id, external_address);
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
    async fn detect_nat_type_reports_public_when_mapping_matches_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();

        tokio::spawn(async move {
            let mut request = [0u8; STUN_RESPONSE_MAX_LEN];
            let (_received, peer) = server.recv_from(&mut request).await.unwrap();
            let transaction_id: [u8; STUN_TRANSACTION_ID_LEN] =
                request[8..STUN_HEADER_LEN].try_into().unwrap();
            let response = encode_binding_response(&transaction_id, peer);
            server.send_to(&response, peer).await.unwrap();
        });

        let nat_type = NatTraversal::detect_nat_type(&server_address.to_string())
            .await
            .unwrap();

        assert_eq!(nat_type, NatType::Public);
    }

    #[test]
    fn decode_binding_response_rejects_wrong_transaction_id() {
        let transaction_id = [0xAA; STUN_TRANSACTION_ID_LEN];
        let response = encode_binding_response(
            &transaction_id,
            SocketAddr::from(([198, 51, 100, 7], 22000)),
        );
        let wrong_transaction_id = [0xBB; STUN_TRANSACTION_ID_LEN];

        let result = decode_binding_response(&response, &wrong_transaction_id);

        assert!(result.is_err());
    }

    fn encode_binding_response(
        transaction_id: &[u8; STUN_TRANSACTION_ID_LEN],
        mapped_address: SocketAddr,
    ) -> Vec<u8> {
        let mut attribute_value = Vec::new();
        attribute_value.push(0);
        match mapped_address {
            SocketAddr::V4(address) => {
                attribute_value.push(STUN_FAMILY_IPV4);
                let port = address.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
                attribute_value.extend_from_slice(&port.to_be_bytes());
                let ip = u32::from(*address.ip()) ^ STUN_MAGIC_COOKIE;
                attribute_value.extend_from_slice(&ip.to_be_bytes());
            }
            SocketAddr::V6(address) => {
                attribute_value.push(STUN_FAMILY_IPV6);
                let port = address.port() ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
                attribute_value.extend_from_slice(&port.to_be_bytes());
                let mut xor_key = [0u8; STUN_IPV6_ADDR_LEN];
                xor_key[..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
                xor_key[4..].copy_from_slice(transaction_id);
                for (index, byte) in address.ip().octets().iter().enumerate() {
                    attribute_value.push(byte ^ xor_key[index]);
                }
            }
        }

        let message_len = STUN_ATTRIBUTE_HEADER_LEN + attribute_value.len();
        let mut response = Vec::with_capacity(STUN_HEADER_LEN + message_len);
        response.extend_from_slice(&BINDING_SUCCESS_RESPONSE.to_be_bytes());
        response.extend_from_slice(&(message_len as u16).to_be_bytes());
        response.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        response.extend_from_slice(transaction_id);
        response.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        response.extend_from_slice(&(attribute_value.len() as u16).to_be_bytes());
        response.extend_from_slice(&attribute_value);
        response
    }
}
