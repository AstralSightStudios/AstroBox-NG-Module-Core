use std::{
    fs::File,
    io::{Error, ErrorKind, Result},
    pin::Pin,
    task::{Context, Poll},
    time::{SystemTime, UNIX_EPOCH},
};

use pcap_file::pcap::{PcapPacket, PcapWriter};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

use crate::tools::{hex_stream_to_bytes, to_hex_string};

use super::meter::BandwidthMeter;

pub struct MiWearTunDevice {
    pub rx: mpsc::Receiver<Vec<u8>>,
    pub tx_send: PollSender<Vec<u8>>,
    pub capture: Option<PcapWriter<File>>,
    pub meter: BandwidthMeter,
}

impl AsyncRead for MiWearTunDevice {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(packet)) => {
                if packet.len() > buf.remaining() {
                    log::warn!(
                        "[MiWearTunDevice] incoming packet truncated ({} > {})",
                        packet.len(),
                        buf.remaining()
                    );
                    buf.put_slice(&packet[..buf.remaining()]);
                } else {
                    buf.put_slice(&packet);
                }
                self.meter.add_read(packet.len());
                if let Some(capture) = self.capture.as_mut() {
                    let mut ethernet = hex_stream_to_bytes("000000000000a5a5a5a5a5a50800").unwrap();
                    ethernet.extend_from_slice(&packet);
                    let packet = PcapPacket {
                        timestamp: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default(),
                        orig_len: ethernet.len() as u32,
                        data: ethernet.into(),
                    };
                    if let Err(err) = capture.write_packet(&packet) {
                        log::warn!("[MiWearTunDevice] failed to capture inbound packet: {err}");
                    }
                }
                #[cfg(debug_assertions)]
                log::debug!(
                    "[MiWearTunDevice] read {} bytes {}",
                    packet.len(),
                    to_hex_string(&packet)
                );
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Err(Error::new(
                ErrorKind::BrokenPipe,
                "network ingress channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MiWearTunDevice {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        let outbound = buf.to_vec();
        #[cfg(debug_assertions)]
        log::debug!(
            "[MiWearTunDevice] write {} bytes {}",
            outbound.len(),
            to_hex_string(&outbound)
        );
        self.meter.add_written(outbound.len());
        if let Some(capture) = self.capture.as_mut() {
            let mut ethernet = hex_stream_to_bytes("a5a5a5a5a5a50000000000000800").unwrap();
            ethernet.extend_from_slice(buf);
            let packet = PcapPacket {
                timestamp: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
                orig_len: ethernet.len() as u32,
                data: ethernet.into(),
            };
            if let Err(err) = capture.write_packet(&packet) {
                log::warn!("[MiWearTunDevice] failed to capture outbound packet: {err}");
            }
        }
        match self.tx_send.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                if let Err(_) = self.tx_send.send_item(outbound) {
                    Poll::Ready(Err(Error::new(
                        ErrorKind::BrokenPipe,
                        "network egress channel closed",
                    )))
                } else {
                    Poll::Ready(Ok(buf.len()))
                }
            }
            Poll::Ready(Err(_)) => Poll::Ready(Err(Error::new(
                ErrorKind::BrokenPipe,
                "network egress channel closed",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}
