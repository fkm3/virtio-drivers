//! Driver for VirtIO socket devices.
#![deny(unsafe_op_in_unsafe_fn)]

use super::error::SocketError;
use super::protocol::{Feature, VirtioVsockConfig, VirtioVsockHdr, VirtioVsockOp, VsockAddr};
use crate::hal::Hal;
use crate::queue::VirtQueue;
use crate::transport::Transport;
use crate::volatile::volread;
use crate::Result;
use alloc::boxed::Box;
use core::mem::size_of;
use core::ptr::{null_mut, NonNull};
use log::{debug, info};
use zerocopy::{AsBytes, FromBytes};

pub(crate) const RX_QUEUE_IDX: u16 = 0;
pub(crate) const TX_QUEUE_IDX: u16 = 1;
const EVENT_QUEUE_IDX: u16 = 2;

pub(crate) const QUEUE_SIZE: usize = 8;

/// The size in bytes of each buffer used in the RX virtqueue.
const RX_BUFFER_SIZE: usize = 512;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub dst: VsockAddr,
    pub src_port: u32,
    /// The last `buf_alloc` value the peer sent to us, indicating how much receive buffer space in
    /// bytes it has allocated for packet bodies.
    peer_buf_alloc: u32,
    /// The last `fwd_cnt` value the peer sent to us, indicating how many bytes of packet bodies it
    /// has finished processing.
    peer_fwd_cnt: u32,
    /// The number of bytes of packet bodies which we have sent to the peer.
    tx_cnt: u32,
    /// The number of bytes of packet bodies which we have received from the peer and handled.
    fwd_cnt: u32,
    /// Whether we have recently requested credit from the peer.
    ///
    /// This is set to true when we send a `VIRTIO_VSOCK_OP_CREDIT_REQUEST`, and false when we
    /// receive a `VIRTIO_VSOCK_OP_CREDIT_UPDATE`.
    has_pending_credit_request: bool,
}

impl ConnectionInfo {
    pub fn new(destination: VsockAddr, src_port: u32) -> Self {
        Self {
            dst: destination,
            src_port,
            ..Default::default()
        }
    }

    /// Updates this connection info with the peer buffer allocation and forwarded count from the
    /// given event.
    pub fn update_for_event(&mut self, event: &VsockEvent) {
        self.peer_buf_alloc = event.buffer_status.buffer_allocation;
        self.peer_fwd_cnt = event.buffer_status.forward_count;

        if let VsockEventType::CreditUpdate = event.event_type {
            self.has_pending_credit_request = false;
        }
    }

    /// Increases the forwarded count recorded for this connection by the given number of bytes.
    ///
    /// This should be called once received data has been passed to the client, so there is buffer
    /// space available for more.
    pub fn done_forwarding(&mut self, length: usize) {
        self.fwd_cnt += length as u32;
    }

    fn peer_free(&self) -> u32 {
        self.peer_buf_alloc - (self.tx_cnt - self.peer_fwd_cnt)
    }

    fn new_header(&self, src_cid: u64) -> VirtioVsockHdr {
        VirtioVsockHdr {
            src_cid: src_cid.into(),
            dst_cid: self.dst.cid.into(),
            src_port: self.src_port.into(),
            dst_port: self.dst.port.into(),
            fwd_cnt: self.fwd_cnt.into(),
            ..Default::default()
        }
    }
}

/// An event received from a VirtIO socket device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsockEvent {
    /// The source of the event, i.e. the peer who sent it.
    pub source: VsockAddr,
    /// The destination of the event, i.e. the CID and port on our side.
    pub destination: VsockAddr,
    /// The peer's buffer status for the connection.
    pub buffer_status: VsockBufferStatus,
    /// The type of event.
    pub event_type: VsockEventType,
}

impl VsockEvent {
    /// Returns whether the event matches the given connection.
    pub fn matches_connection(&self, connection_info: &ConnectionInfo, guest_cid: u64) -> bool {
        self.source == connection_info.dst
            && self.destination.cid == guest_cid
            && self.destination.port == connection_info.src_port
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsockBufferStatus {
    pub buffer_allocation: u32,
    pub forward_count: u32,
}

/// The reason why a vsock connection was closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    /// The peer has either closed the connection in response to our shutdown request, or forcibly
    /// closed it of its own accord.
    Reset,
    /// The peer asked to shut down the connection.
    Shutdown,
}

/// Details of the type of an event received from a VirtIO socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VsockEventType {
    /// The connection was successfully established.
    Connected,
    /// The connection was closed.
    Disconnected {
        /// The reason for the disconnection.
        reason: DisconnectReason,
    },
    /// Data was received on the connection.
    Received {
        /// The length of the data in bytes.
        length: usize,
    },
    /// The peer requests us to send a credit update.
    CreditRequest,
    /// The peer just sent us a credit update with nothing else.
    CreditUpdate,
}

/// Driver for a VirtIO socket device.
pub struct VirtIOSocket<H: Hal, T: Transport> {
    transport: T,
    /// Virtqueue to receive packets.
    rx: VirtQueue<H, { QUEUE_SIZE }>,
    tx: VirtQueue<H, { QUEUE_SIZE }>,
    /// Virtqueue to receive events from the device.
    event: VirtQueue<H, { QUEUE_SIZE }>,
    /// The guest_cid field contains the guest’s context ID, which uniquely identifies
    /// the device for its lifetime. The upper 32 bits of the CID are reserved and zeroed.
    guest_cid: u64,
    rx_queue_buffers: [NonNull<[u8; RX_BUFFER_SIZE]>; QUEUE_SIZE],
}

impl<H: Hal, T: Transport> Drop for VirtIOSocket<H, T> {
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(RX_QUEUE_IDX);
        self.transport.queue_unset(TX_QUEUE_IDX);
        self.transport.queue_unset(EVENT_QUEUE_IDX);

        for buffer in self.rx_queue_buffers {
            // Safe because we obtained the RX buffer pointer from Box::into_raw, and it won't be
            // used anywhere else after the driver is destroyed.
            unsafe { drop(Box::from_raw(buffer.as_ptr())) };
        }
    }
}

impl<H: Hal, T: Transport> VirtIOSocket<H, T> {
    /// Create a new VirtIO Vsock driver.
    pub fn new(mut transport: T) -> Result<Self> {
        transport.begin_init(|features| {
            let features = Feature::from_bits_truncate(features);
            info!("Device features: {:?}", features);
            // negotiate these flags only
            let supported_features = Feature::empty();
            (features & supported_features).bits()
        });

        let config = transport.config_space::<VirtioVsockConfig>()?;
        info!("config: {:?}", config);
        // Safe because config is a valid pointer to the device configuration space.
        let guest_cid = unsafe {
            volread!(config, guest_cid_low) as u64 | (volread!(config, guest_cid_high) as u64) << 32
        };
        info!("guest cid: {guest_cid:?}");

        let mut rx = VirtQueue::new(&mut transport, RX_QUEUE_IDX)?;
        let tx = VirtQueue::new(&mut transport, TX_QUEUE_IDX)?;
        let event = VirtQueue::new(&mut transport, EVENT_QUEUE_IDX)?;

        // Allocate and add buffers for the RX queue.
        let mut rx_queue_buffers = [null_mut(); QUEUE_SIZE];
        for i in 0..QUEUE_SIZE {
            let mut buffer: Box<[u8; RX_BUFFER_SIZE]> = FromBytes::new_box_zeroed();
            // Safe because the buffer lives as long as the queue, as specified in the function
            // safety requirement, and we don't access it until it is popped.
            let token = unsafe { rx.add(&[], &mut [buffer.as_mut_slice()]) }?;
            assert_eq!(i, token.into());
            rx_queue_buffers[i] = Box::into_raw(buffer);
        }
        let rx_queue_buffers = rx_queue_buffers.map(|ptr| NonNull::new(ptr).unwrap());

        transport.finish_init();
        if rx.should_notify() {
            transport.notify(RX_QUEUE_IDX);
        }

        Ok(Self {
            transport,
            rx,
            tx,
            event,
            guest_cid,
            rx_queue_buffers,
        })
    }

    /// Returns the CID which has been assigned to this guest.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Sends a request to connect to the given destination.
    ///
    /// This returns as soon as the request is sent; you should wait until `poll_recv` returns a
    /// `VsockEventType::Connected` event indicating that the peer has accepted the connection
    /// before sending data.
    pub fn connect(&mut self, destination: VsockAddr, src_port: u32) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Request.into(),
            src_cid: self.guest_cid.into(),
            dst_cid: destination.cid.into(),
            src_port: src_port.into(),
            dst_port: destination.port.into(),
            ..Default::default()
        };
        // Sends a header only packet to the tx queue to connect the device to the listening
        // socket at the given destination.
        self.send_packet_to_tx_queue(&header, &[])?;

        Ok(())
    }

    /// Requests the peer to send us a credit update for the given connection.
    fn request_credit(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::CreditRequest.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Sends the buffer to the destination.
    pub fn send(&mut self, buffer: &[u8], connection_info: &mut ConnectionInfo) -> Result {
        self.check_peer_buffer_is_sufficient(connection_info, buffer.len())?;

        let len = buffer.len() as u32;
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Rw.into(),
            len: len.into(),
            buf_alloc: 0.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        connection_info.tx_cnt += len;
        self.send_packet_to_tx_queue(&header, buffer)
    }

    fn check_peer_buffer_is_sufficient(
        &mut self,
        connection_info: &mut ConnectionInfo,
        buffer_len: usize,
    ) -> Result {
        if connection_info.peer_free() as usize >= buffer_len {
            Ok(())
        } else {
            // Request an update of the cached peer credit, if we haven't already done so, and tell
            // the caller to try again later.
            if !connection_info.has_pending_credit_request {
                self.request_credit(connection_info)?;
                connection_info.has_pending_credit_request = true;
            }
            Err(SocketError::InsufficientBufferSpaceInPeer.into())
        }
    }

    /// Tells the peer how much buffer space we have to receive data.
    pub fn credit_update(&mut self, connection_info: &ConnectionInfo, buffer_size: u32) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::CreditUpdate.into(),
            buf_alloc: buffer_size.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Polls the vsock device to receive data or other updates.
    ///
    /// A buffer must be provided to put the data in if there is some to
    /// receive.
    pub fn poll_recv(&mut self, buffer: &mut [u8]) -> Result<Option<VsockEvent>> {
        // Handle entries from the RX virtqueue until we find one that generates an event.
        let event = self.poll_rx_queue(buffer)?;

        if self.rx.should_notify() {
            self.transport.notify(RX_QUEUE_IDX);
        }

        Ok(event)
    }

    /// Requests to shut down the connection cleanly.
    ///
    /// This returns as soon as the request is sent; you should wait until `poll_recv` returns a
    /// `VsockEventType::Disconnected` event if you want to know that the peer has acknowledged the
    /// shutdown.
    pub fn shutdown(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Shutdown.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Forcibly closes the connection without waiting for the peer.
    pub fn force_close(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Rst.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])?;
        Ok(())
    }

    fn send_packet_to_tx_queue(&mut self, header: &VirtioVsockHdr, buffer: &[u8]) -> Result {
        let _len = self.tx.add_notify_wait_pop(
            &[header.as_bytes(), buffer],
            &mut [],
            &mut self.transport,
        )?;
        Ok(())
    }

    /// Polls the RX virtqueue for the next event.
    ///
    /// Returns `Ok(None)` if the virtqueue is empty, possibly after processing some packets which
    /// don't result in any events to return.
    fn poll_rx_queue(&mut self, body: &mut [u8]) -> Result<Option<VsockEvent>> {
        let Some(header) = self.pop_packet_from_rx_queue(body)? else {
            return Ok(None);
        };

        let op = header.op()?;

        let buffer_status = VsockBufferStatus {
            buffer_allocation: header.buf_alloc.into(),
            forward_count: header.fwd_cnt.into(),
        };
        let source = header.source();
        let destination = header.destination();

        match op {
            VirtioVsockOp::Request => {
                header.check_data_is_empty()?;
                // TODO: Send a Rst, or support listening.
                Ok(None)
            }
            VirtioVsockOp::Response => {
                header.check_data_is_empty()?;
                Ok(Some(VsockEvent {
                    source,
                    destination,
                    buffer_status,
                    event_type: VsockEventType::Connected,
                }))
            }
            VirtioVsockOp::CreditUpdate => {
                header.check_data_is_empty()?;
                Ok(Some(VsockEvent {
                    source,
                    destination,
                    buffer_status,
                    event_type: VsockEventType::CreditUpdate,
                }))
            }
            VirtioVsockOp::Rst | VirtioVsockOp::Shutdown => {
                header.check_data_is_empty()?;

                info!("Disconnected from the peer");

                let reason = if op == VirtioVsockOp::Rst {
                    DisconnectReason::Reset
                } else {
                    DisconnectReason::Shutdown
                };
                Ok(Some(VsockEvent {
                    source,
                    destination,
                    buffer_status,
                    event_type: VsockEventType::Disconnected { reason },
                }))
            }
            VirtioVsockOp::Rw => Ok(Some(VsockEvent {
                source,
                destination,
                buffer_status,
                event_type: VsockEventType::Received {
                    length: header.len() as usize,
                },
            })),
            VirtioVsockOp::CreditRequest => {
                header.check_data_is_empty()?;
                Ok(Some(VsockEvent {
                    source,
                    destination,
                    buffer_status,
                    event_type: VsockEventType::CreditRequest,
                }))
            }
            VirtioVsockOp::Invalid => Err(SocketError::InvalidOperation.into()),
        }
    }

    /// Pops one packet from the RX queue, if there is one pending. Returns the header, and copies
    /// the body into the given buffer.
    ///
    /// Returns `None` if there is no pending packet, or an error if the body is bigger than the
    /// buffer supplied.
    fn pop_packet_from_rx_queue(&mut self, body: &mut [u8]) -> Result<Option<VirtioVsockHdr>> {
        let Some(token) = self.rx.peek_used() else {
            return Ok(None);
        };

        // Safe because we maintain a consistent mapping of tokens to buffers, so we pass the same
        // buffer to `pop_used` as we previously passed to `add` for the token. Once we add the
        // buffer back to the RX queue then we don't access it again until next time it is popped.
        let header = unsafe {
            let buffer = self.rx_queue_buffers[usize::from(token)].as_mut();
            let _len = self.rx.pop_used(token, &[], &mut [buffer])?;

            // Read the header and body from the buffer. Don't check the result yet, because we need
            // to add the buffer back to the queue either way.
            let header_result = read_header_and_body(buffer, body);

            // Add the buffer back to the RX queue.
            let new_token = self.rx.add(&[], &mut [buffer])?;
            // If the RX buffer somehow gets assigned a different token, then our safety assumptions
            // are broken and we can't safely continue to do anything with the device.
            assert_eq!(new_token, token);

            header_result
        }?;

        debug!("Received packet {:?}. Op {:?}", header, header.op());
        Ok(Some(header))
    }
}

fn read_header_and_body(buffer: &[u8], body: &mut [u8]) -> Result<VirtioVsockHdr> {
    let header = VirtioVsockHdr::read_from_prefix(buffer).ok_or(SocketError::BufferTooShort)?;
    let body_length = header.len() as usize;
    let data_end = size_of::<VirtioVsockHdr>()
        .checked_add(body_length)
        .ok_or(SocketError::InvalidNumber)?;
    let data = buffer
        .get(size_of::<VirtioVsockHdr>()..data_end)
        .ok_or(SocketError::BufferTooShort)?;
    body.get_mut(0..body_length)
        .ok_or(SocketError::OutputBufferTooShort(body_length))?
        .copy_from_slice(data);
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hal::fake::FakeHal,
        transport::{
            fake::{FakeTransport, QueueStatus, State},
            DeviceStatus, DeviceType,
        },
        volatile::ReadOnly,
    };
    use alloc::{sync::Arc, vec};
    use core::ptr::NonNull;
    use std::sync::Mutex;

    #[test]
    fn config() {
        let mut config_space = VirtioVsockConfig {
            guest_cid_low: ReadOnly::new(66),
            guest_cid_high: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State {
            status: DeviceStatus::empty(),
            driver_features: 0,
            guest_page_size: 0,
            interrupt_pending: false,
            queues: vec![
                QueueStatus::default(),
                QueueStatus::default(),
                QueueStatus::default(),
            ],
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Socket,
            max_queue_size: 32,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let socket =
            VirtIOSocket::<FakeHal, FakeTransport<VirtioVsockConfig>>::new(transport).unwrap();
        assert_eq!(socket.guest_cid(), 0x00_0000_0042);
    }
}
