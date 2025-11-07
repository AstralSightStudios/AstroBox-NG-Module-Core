use anyhow::Result;
use dhcproto::{Decodable, Encodable, v4};
use packet_crafter::{Packet, headers::Header};
use std::net::Ipv4Addr;

use crate::tools::to_hex_string;

fn push_u8(buf: &mut Vec<u8>, value: u8) {
    buf.push(value);
}

fn push_u16(buf: &mut Vec<u8>, value: u16) {
    buf.extend_from_slice(&value.to_be_bytes());
}

fn push_ipv4(buf: &mut Vec<u8>, addr: Ipv4Addr) {
    buf.extend_from_slice(&addr.octets());
}

fn compute_checksum(buffer: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < buffer.len() {
        sum += u32::from(buffer[i]) << 8 | u32::from(buffer[i + 1]);
        i += 2;
    }
    if i < buffer.len() {
        sum += u32::from(buffer[i]) << 8;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn maybe_build_reply(network_packet: &[u8]) -> Result<Option<Vec<u8>>> {
    let parsed = match Packet::parse(network_packet) {
        Ok(pkt) => pkt,
        Err(err) => {
            log::trace!("[Dhcp] failed to parse packet: {err}");
            return Ok(None);
        }
    };

    let ip = match parsed.get_ip_header() {
        Some(header) => header,
        None => return Ok(None),
    };
    let udp = match parsed.get_udp_header() {
        Some(header) => header,
        None => return Ok(None),
    };

    if *udp.get_src_port() != 68 || *udp.get_dst_port() != 67 {
        return Ok(None);
    }

    let offset = (ip.get_length() + 8) as usize;
    if network_packet.len() <= offset {
        return Ok(None);
    }
    let dhcp_payload = &network_packet[offset..];

    #[cfg(debug_assertions)]
    log::debug!("[Dhcp] request payload: {}", to_hex_string(dhcp_payload));

    let payload_vec = dhcp_payload.to_vec();
    let mut decoder = v4::Decoder::new(&payload_vec);
    let dhcp_msg = v4::Message::decode(&mut decoder)?;
    if dhcp_msg.opcode() != v4::Opcode::BootRequest {
        return Ok(None);
    }

    let mut reply = dhcp_msg.clone();
    reply.set_opcode(v4::Opcode::BootReply);
    reply.set_secs(0);
    reply.set_flags(0.into());
    reply.set_ciaddr(Ipv4Addr::UNSPECIFIED);
    reply.set_yiaddr(Ipv4Addr::new(10, 1, 10, 2));
    reply.set_siaddr(Ipv4Addr::new(10, 1, 10, 1));
    reply.set_giaddr(Ipv4Addr::UNSPECIFIED);

    let mut opts = v4::DhcpOptions::new();
    if let Some(v4::DhcpOption::MessageType(req_type)) =
        dhcp_msg.opts().get(v4::OptionCode::MessageType)
    {
        match req_type {
            v4::MessageType::Discover => {
                opts.insert(v4::DhcpOption::MessageType(v4::MessageType::Offer));
            }
            v4::MessageType::Request => {
                opts.insert(v4::DhcpOption::MessageType(v4::MessageType::Ack));
            }
            _ => {}
        }
    }
    opts.insert(v4::DhcpOption::SubnetMask(Ipv4Addr::new(255, 255, 255, 0)));
    opts.insert(v4::DhcpOption::Router(vec![Ipv4Addr::new(10, 1, 10, 1)]));
    opts.insert(v4::DhcpOption::AddressLeaseTime(269_352_960));
    opts.insert(v4::DhcpOption::ServerIdentifier(Ipv4Addr::new(
        10, 1, 10, 1,
    )));
    reply.set_opts(opts);

    let mut dhcp_payload = Vec::new();
    reply.encode(&mut v4::Encoder::new(&mut dhcp_payload))?;

    let udp_len = (dhcp_payload.len() + 8) as u16;
    let src_addr = Ipv4Addr::new(255, 255, 255, 255);
    let dst_addr = Ipv4Addr::new(10, 1, 10, 1);

    let mut udp_header = Vec::with_capacity(8);
    push_u16(&mut udp_header, 0x43);
    push_u16(&mut udp_header, 0x44);
    push_u16(&mut udp_header, udp_len);
    push_u16(&mut udp_header, 0); // checksum placeholder

    let mut pseudo = Vec::with_capacity(12 + udp_header.len() + dhcp_payload.len());
    push_ipv4(&mut pseudo, src_addr);
    push_ipv4(&mut pseudo, dst_addr);
    push_u8(&mut pseudo, 0);
    push_u8(&mut pseudo, 0x11);
    push_u16(&mut pseudo, udp_len);
    pseudo.extend_from_slice(&udp_header);
    pseudo.extend_from_slice(&dhcp_payload);

    let udp_checksum = compute_checksum(&pseudo);
    udp_header[6..8].copy_from_slice(&udp_checksum.to_be_bytes());

    let total_len = (20 + udp_header.len() + dhcp_payload.len()) as u16;
    let mut ip_header = Vec::with_capacity(20);
    push_u8(&mut ip_header, 0x45);
    push_u8(&mut ip_header, 0x00);
    push_u16(&mut ip_header, total_len);
    push_u16(&mut ip_header, 0); // identification
    push_u16(&mut ip_header, 0); // flags + offset
    push_u8(&mut ip_header, 0x40); // TTL
    push_u8(&mut ip_header, 0x11); // UDP
    push_u16(&mut ip_header, 0); // checksum placeholder
    push_ipv4(&mut ip_header, src_addr);
    push_ipv4(&mut ip_header, dst_addr);

    let ip_checksum = compute_checksum(&ip_header);
    ip_header[10..12].copy_from_slice(&ip_checksum.to_be_bytes());

    ip_header.extend_from_slice(&udp_header);
    ip_header.extend_from_slice(&dhcp_payload);

    #[cfg(debug_assertions)]
    log::debug!("[Dhcp] reply payload: {}", to_hex_string(&ip_header));

    Ok(Some(ip_header))
}
