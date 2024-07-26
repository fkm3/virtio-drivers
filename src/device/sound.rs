//! Driver for VirtIO Sound devices.

#[cfg(test)]
mod fake;

use alloc::vec;
use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
use bitflags::bitflags;
use core::{
    fmt::{self, Debug, Display, Formatter},
    hint::spin_loop,
    mem::size_of,
    ops::RangeInclusive,
};
use log::{error, info, warn};
use num_enum::{FromPrimitive, IntoPrimitive};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

use crate::queue::owning::OwningQueue;
use crate::{
    queue::VirtQueue,
    transport::Transport,
    volatile::{volread, ReadOnly},
    Error, Hal, Result, PAGE_SIZE,
};

use super::common::Feature;

const RSP_SIZE: usize = 128;

/// Audio driver based on virtio v1.2.
///
/// Supports synchronous blocking and asynchronous non-blocking audio playback.
///
/// Currently, only audio playback functionality has been implemented.
pub struct VirtIOSound<H: Hal, T: Transport> {
    transport: T,

    control_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    event_queue: OwningQueue<H, { QUEUE_SIZE as usize }, { size_of::<VirtIOSndEvent>() }>,
    tx_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    rx_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,

    negotiated_features: Feature,

    jacks: u32,
    streams: u32,
    chmaps: u32,

    pcm_infos: Option<Vec<VirtIOSndPcmInfo>>,
    jack_infos: Option<Vec<VirtIOSndJackInfo>>,
    chmap_infos: Option<Vec<VirtIOSndChmapInfo>>,

    // pcm params
    pcm_parameters: Vec<PcmParameters>,

    queue_buf_send: Box<[u8]>,
    queue_buf_recv: Box<[u8]>,

    set_up: bool,

    token_rsp: BTreeMap<u16, Box<[u8; RSP_SIZE]>>, // includes pcm_xfer response msg

    pcm_states: Vec<PCMState>,

    token_buf: BTreeMap<u16, Vec<u8>>, // store token and its input buf
}

impl<H: Hal, T: Transport> VirtIOSound<H, T> {
    /// Create a new VirtIO-Sound driver.
    pub fn new(mut transport: T) -> Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);
        info!(
            "[sound device] negotiated_features: {:?}",
            negotiated_features
        );

        let control_queue = VirtQueue::new(
            &mut transport,
            CONTROL_QUEUE_IDX,
            negotiated_features.contains(Feature::RING_INDIRECT_DESC),
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?;
        let event_queue = OwningQueue::new(VirtQueue::new(
            &mut transport,
            EVENT_QUEUE_IDX,
            negotiated_features.contains(Feature::RING_INDIRECT_DESC),
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?)?;
        let tx_queue = VirtQueue::new(
            &mut transport,
            TX_QUEUE_IDX,
            negotiated_features.contains(Feature::RING_INDIRECT_DESC),
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?;
        let rx_queue = VirtQueue::new(
            &mut transport,
            RX_QUEUE_IDX,
            negotiated_features.contains(Feature::RING_INDIRECT_DESC),
            negotiated_features.contains(Feature::RING_EVENT_IDX),
        )?;

        // read configuration space
        let config_ptr = transport.config_space::<VirtIOSoundConfig>()?;
        // SAFETY: config_ptr is a valid pointer to the device configuration space.
        let (jacks, streams, chmaps) = unsafe {
            (
                volread!(config_ptr, jacks),
                volread!(config_ptr, streams),
                volread!(config_ptr, chmaps),
            )
        };
        info!(
            "[sound device] config: jacks: {}, streams: {}, chmaps: {}",
            jacks, streams, chmaps
        );

        let queue_buf_send = FromZeroes::new_box_slice_zeroed(PAGE_SIZE);
        let queue_buf_recv = FromZeroes::new_box_slice_zeroed(PAGE_SIZE);

        // set pcm params to default
        let mut pcm_parameters = vec![];
        for _ in 0..streams {
            pcm_parameters.push(PcmParameters::default());
        }

        transport.finish_init();

        if event_queue.should_notify() {
            transport.notify(EVENT_QUEUE_IDX);
        }

        Ok(VirtIOSound {
            transport,
            control_queue,
            event_queue,
            tx_queue,
            rx_queue,
            negotiated_features,
            jacks,
            streams,
            chmaps,
            pcm_infos: None,
            jack_infos: None,
            chmap_infos: None,
            queue_buf_send,
            queue_buf_recv,
            pcm_parameters,
            set_up: false,
            token_rsp: BTreeMap::new(),
            pcm_states: vec![],
            token_buf: BTreeMap::new(),
        })
    }

    /// Total jack num.
    pub fn jacks(&self) -> u32 {
        self.jacks
    }

    /// Total stream num.
    pub fn streams(&self) -> u32 {
        self.streams
    }

    /// Total chmap num.
    pub fn chmaps(&self) -> u32 {
        self.chmaps
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.transport.ack_interrupt()
    }

    fn request<Req: AsBytes, Rsp: FromBytes>(&mut self, req: Req) -> Result<Rsp> {
        self.control_queue.add_notify_wait_pop(
            &[req.as_bytes()],
            &mut [self.queue_buf_recv.as_bytes_mut()],
            &mut self.transport,
        )?;
        Ok(Rsp::read_from_prefix(&self.queue_buf_recv).unwrap())
    }

    /// Set up the driver, initate pcm_infos and jacks_infos
    fn set_up(&mut self) {
        // init jack info
        if let Ok(jack_infos) = self.jack_info(0, self.jacks) {
            for jack_info in &jack_infos {
                info!("[sound device] jack_info: {}", jack_info);
            }
            self.jack_infos = Some(jack_infos);
        } else {
            self.jack_infos = Some(vec![]);
            warn!("[sound device] There are no available jacks!");
        }
        // init pcm info
        if let Ok(pcm_infos) = self.pcm_info(0, self.streams) {
            for pcm_info in &pcm_infos {
                info!("[sound device] pcm_info: {}", pcm_info);
            }
            self.pcm_infos = Some(pcm_infos);
        } else {
            self.pcm_infos = Some(vec![]);
            warn!("[sound device] There are no available streams!");
        }
        // init chmap info
        if let Ok(chmap_infos) = self.chmap_info(0, self.chmaps) {
            for chmap_info in &chmap_infos {
                info!("[sound device] chmap_info: {}", chmap_info);
            }
            self.chmap_infos = Some(chmap_infos);
        } else {
            self.chmap_infos = Some(vec![]);
            warn!("[sound device] There are no available chmaps!");
        }
        // set pcm state to default
        for _ in 0..self.streams {
            self.pcm_states.push(PCMState::default());
        }
        self.event_queue.set_dev_notify(true);
    }

    /// Query information about the available jacks.
    fn jack_info(&mut self, jack_start_id: u32, jack_count: u32) -> Result<Vec<VirtIOSndJackInfo>> {
        if self.jacks == 0 {
            return Err(Error::ConfigSpaceTooSmall);
        }
        if jack_start_id + jack_count > self.jacks {
            error!("jack_start_id + jack_count > jacks! There are not enough jacks to be queried!");
            return Err(Error::IoError);
        }
        let hdr: VirtIOSndHdr = self.request(VirtIOSndQueryInfo {
            hdr: ItemInformationRequestType::RJackInfo.into(),
            start_id: jack_start_id,
            count: jack_count,
            size: size_of::<VirtIOSndJackInfo>() as u32,
        })?;
        if hdr != RequestStatusCode::Ok.into() {
            return Err(Error::IoError);
        }
        // read struct VirtIOSndJackInfo
        let mut jack_infos = vec![];
        for i in 0..jack_count as usize {
            const HDR_SIZE: usize = size_of::<VirtIOSndHdr>();
            const JACK_INFO_SIZE: usize = size_of::<VirtIOSndJackInfo>();
            let start_byte_idx = HDR_SIZE + i * JACK_INFO_SIZE;
            let end_byte_idx = HDR_SIZE + (i + 1) * JACK_INFO_SIZE;
            let jack_info =
                VirtIOSndJackInfo::read_from(&self.queue_buf_recv[start_byte_idx..end_byte_idx])
                    .unwrap();
            jack_infos.push(jack_info)
        }
        Ok(jack_infos)
    }

    /// Query information about the available streams.
    fn pcm_info(
        &mut self,
        stream_start_id: u32,
        stream_count: u32,
    ) -> Result<Vec<VirtIOSndPcmInfo>> {
        if self.streams == 0 {
            return Err(Error::ConfigSpaceTooSmall);
        }
        if stream_start_id + stream_count > self.streams {
            error!("stream_start_id + stream_count > streams! There are not enough streams to be queried!");
            return Err(Error::IoError);
        }
        let request_hdr = VirtIOSndHdr::from(ItemInformationRequestType::RPcmInfo);
        let hdr: VirtIOSndHdr = self.request(VirtIOSndQueryInfo {
            hdr: request_hdr,
            start_id: stream_start_id,
            count: stream_count,
            size: size_of::<VirtIOSndPcmInfo>() as u32,
        })?;
        if hdr != RequestStatusCode::Ok.into() {
            return Err(Error::IoError);
        }
        // read struct VirtIOSndPcmInfo
        let mut pcm_infos = vec![];
        for i in 0..stream_count as usize {
            const HDR_SIZE: usize = size_of::<VirtIOSndHdr>();
            const PCM_INFO_SIZE: usize = size_of::<VirtIOSndPcmInfo>();
            let start_byte_idx = HDR_SIZE + i * PCM_INFO_SIZE;
            let end_byte_idx = HDR_SIZE + (i + 1) * PCM_INFO_SIZE;
            let pcm_info =
                VirtIOSndPcmInfo::read_from(&self.queue_buf_recv[start_byte_idx..end_byte_idx])
                    .unwrap();
            pcm_infos.push(pcm_info);
        }
        Ok(pcm_infos)
    }

    /// Query information about the available chmaps.
    fn chmap_info(
        &mut self,
        chmaps_start_id: u32,
        chmaps_count: u32,
    ) -> Result<Vec<VirtIOSndChmapInfo>> {
        if self.chmaps == 0 {
            return Err(Error::ConfigSpaceTooSmall);
        }
        if chmaps_start_id + chmaps_count > self.chmaps {
            error!("chmaps_start_id + chmaps_count > self.chmaps");
            return Err(Error::IoError);
        }
        let hdr: VirtIOSndHdr = self.request(VirtIOSndQueryInfo {
            hdr: ItemInformationRequestType::RChmapInfo.into(),
            start_id: chmaps_start_id,
            count: chmaps_count,
            size: size_of::<VirtIOSndChmapInfo>() as u32,
        })?;
        if hdr != RequestStatusCode::Ok.into() {
            return Err(Error::IoError);
        }
        let mut chmap_infos = vec![];
        for i in 0..chmaps_count as usize {
            const OFFSET: usize = size_of::<VirtIOSndHdr>();
            let start_byte = OFFSET + i * size_of::<VirtIOSndChmapInfo>();
            let end_byte = OFFSET + (i + 1) * size_of::<VirtIOSndChmapInfo>();
            let chmap_info =
                VirtIOSndChmapInfo::read_from(&self.queue_buf_recv[start_byte..end_byte]).unwrap();
            chmap_infos.push(chmap_info);
        }
        Ok(chmap_infos)
    }

    /// If the VIRTIO_SND_JACK_F_REMAP feature bit is set in the jack information, then the driver can send a
    /// control request to change the association and/or sequence number for the specified jack ID.
    /// # Arguments
    ///
    /// * `jack_id` - A u32 int which is in the range of [0, jacks)
    pub fn jack_remap(&mut self, jack_id: u32, association: u32, sequence: u32) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if self.jacks == 0 {
            error!("[sound device] There is no available jacks!");
            return Err(Error::InvalidParam);
        }
        if jack_id >= self.jacks {
            error!("jack_id >= self.jacks! Make sure jack_id is in the range of [0, jacks - 1)!");
            return Err(Error::InvalidParam);
        }

        let jack_features = JackFeatures::from_bits_retain(
            self.jack_infos
                .as_ref()
                .unwrap()
                .get(jack_id as usize)
                .unwrap()
                .features,
        );
        if !jack_features.contains(JackFeatures::REMAP) {
            error!("The jack selected does not support VIRTIO_SND_JACK_F_REMAP!");
            return Err(Error::Unsupported);
        }
        let hdr: VirtIOSndHdr = self.request(VirtIOSndJackRemap {
            hdr: VirtIOSndJackHdr {
                hdr: CommandCode::RJackRemap.into(),
                jack_id,
            },
            association,
            sequence,
        })?;
        if hdr == RequestStatusCode::Ok.into() {
            Ok(())
        } else {
            Err(Error::Unsupported)
        }
    }

    /// Set selected stream parameters for the specified stream ID.
    pub fn pcm_set_params(
        &mut self,
        stream_id: u32,
        buffer_bytes: u32,
        period_bytes: u32,
        features: PcmFeatures,
        channels: u8,
        format: PcmFormat,
        rate: PcmRate,
    ) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        let request_hdr = VirtIOSndHdr::from(CommandCode::RPcmSetParams);
        let rsp: VirtIOSndHdr = self.request(VirtIOSndPcmSetParams {
            hdr: VirtIOSndPcmHdr {
                hdr: request_hdr,
                stream_id,
            },
            buffer_bytes,
            period_bytes,
            features: features.bits(),
            channels,
            format: format.into(),
            rate: rate.into(),
            _padding: 0,
        })?;
        // rsp is just a header, so it can be compared with VirtIOSndHdr
        if rsp == VirtIOSndHdr::from(RequestStatusCode::Ok) {
            self.pcm_parameters[stream_id as usize] = PcmParameters {
                setup: true,
                buffer_bytes,
                period_bytes,
                features,
                channels,
                format,
                rate,
            };
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Prepare a stream with specified stream ID.
    pub fn pcm_prepare(&mut self, stream_id: u32) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        let request_hdr = VirtIOSndHdr::from(CommandCode::RPcmPrepare);
        let rsp: VirtIOSndHdr = self.request(VirtIOSndPcmHdr {
            hdr: request_hdr,
            stream_id,
        })?;
        // rsp is just a header, so it can be compared with VirtIOSndHdr
        if rsp == VirtIOSndHdr::from(RequestStatusCode::Ok) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Release a stream with specified stream ID.
    pub fn pcm_release(&mut self, stream_id: u32) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        let request_hdr = VirtIOSndHdr::from(CommandCode::RPcmRelease);
        let rsp: VirtIOSndHdr = self.request(VirtIOSndPcmHdr {
            hdr: request_hdr,
            stream_id,
        })?;
        // rsp is just a header, so it can be compared with VirtIOSndHdr
        if rsp == VirtIOSndHdr::from(RequestStatusCode::Ok) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Start a stream with specified stream ID.
    pub fn pcm_start(&mut self, stream_id: u32) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        let request_hdr = VirtIOSndHdr::from(CommandCode::RPcmStart);
        let rsp: VirtIOSndHdr = self.request(VirtIOSndPcmHdr {
            hdr: request_hdr,
            stream_id,
        })?;
        // rsp is just a header, so it can be compared with VirtIOSndHdr
        if rsp == VirtIOSndHdr::from(RequestStatusCode::Ok) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Stop a stream with specified stream ID.
    pub fn pcm_stop(&mut self, stream_id: u32) -> Result {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        let request_hdr = VirtIOSndHdr::from(CommandCode::RPcmStop);
        let rsp: VirtIOSndHdr = self.request(VirtIOSndPcmHdr {
            hdr: request_hdr,
            stream_id,
        })?;
        // rsp is just a header, so it can be compared with VirtIOSndHdr
        if rsp == VirtIOSndHdr::from(RequestStatusCode::Ok) {
            Ok(())
        } else {
            Err(Error::IoError)
        }
    }

    /// Transfer PCM frame to device, based on the stream type(OUTPUT/INPUT).
    ///
    /// Currently supports only output stream.
    ///
    /// This is a blocking method that will not return until the audio playback is complete.
    pub fn pcm_xfer(&mut self, stream_id: u32, frames: &[u8]) -> Result {
        const U32_SIZE: usize = size_of::<u32>();
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if !self.pcm_parameters[stream_id as usize].setup {
            warn!("Please set parameters for a stream before using it!");
            return Err(Error::IoError);
        }

        let mut outputs = vec![0; 32];
        let buffer_size = self.pcm_parameters[stream_id as usize].buffer_bytes as usize;
        // If the size of the music is smaller than the buffer size, then one buffer is sufficient for playback.
        if frames.len() <= buffer_size {
            let token = unsafe {
                self.tx_queue
                    .add(&[&stream_id.to_le_bytes(), frames], &mut [&mut outputs])
                    .unwrap()
            };
            self.transport.notify(TX_QUEUE_IDX);
            while !self.tx_queue.can_pop() {
                spin_loop()
            }
            unsafe {
                self.tx_queue
                    .pop_used(
                        token,
                        &[&stream_id.to_le_bytes(), frames],
                        &mut [&mut outputs],
                    )
                    .unwrap();
            }
            return Ok(());
        } else if buffer_size < frames.len() && frames.len() <= buffer_size * 2 {
            let token1 = unsafe {
                self.tx_queue
                    .add(
                        &[&stream_id.to_le_bytes(), &frames[..buffer_size]],
                        &mut [&mut outputs],
                    )
                    .unwrap()
            };
            let token2 = unsafe {
                self.tx_queue
                    .add(
                        &[&stream_id.to_le_bytes(), &frames[buffer_size..]],
                        &mut [&mut outputs],
                    )
                    .unwrap()
            };
            self.transport.notify(TX_QUEUE_IDX);
            while !self.tx_queue.can_pop() {
                spin_loop()
            }
            unsafe {
                self.tx_queue
                    .pop_used(
                        token1,
                        &[&stream_id.to_le_bytes(), &frames[..buffer_size]],
                        &mut [&mut outputs],
                    )
                    .unwrap();
            }
            while !self.tx_queue.can_pop() {
                spin_loop()
            }
            unsafe {
                self.tx_queue
                    .pop_used(
                        token2,
                        &[&stream_id.to_le_bytes(), &frames[buffer_size..]],
                        &mut [&mut outputs],
                    )
                    .unwrap();
            }
            return Ok(());
        }
        // Safe because the device reads the buffer one at a time, ensuring that it's impossible for outputs to be occupied simultaneously.
        let mut token1 = unsafe {
            self.tx_queue
                .add(
                    &[&stream_id.to_le_bytes(), &frames[..buffer_size]],
                    &mut [&mut outputs],
                )
                .unwrap()
        };
        let mut token2 = unsafe {
            self.tx_queue
                .add(
                    &[
                        &stream_id.to_le_bytes(),
                        &frames[buffer_size..2 * buffer_size],
                    ],
                    &mut [&mut outputs],
                )
                .unwrap()
        };

        let mut last_start1 = 0;
        let mut last_end1 = buffer_size;
        let mut last_start2 = buffer_size;
        let mut last_end2 = 2 * buffer_size;

        let xfer_times = if frames.len() % buffer_size == 0 {
            frames.len() / buffer_size
        } else {
            frames.len() / buffer_size + 1
        };

        let mut turn1 = true;

        for i in 2..xfer_times {
            if self.tx_queue.should_notify() {
                self.transport.notify(TX_QUEUE_IDX);
            }

            let start_byte = i * buffer_size;
            let end_byte = if i != xfer_times - 1 {
                (i + 1) * buffer_size
            } else {
                frames.len()
            };
            if turn1 {
                while !self.tx_queue.can_pop() {
                    spin_loop();
                }
                // Safe because token1 corresponds to buf1, and there's only one output buffer outputs, maintaining the same relationship as when add() was initially performed.
                // The following unsafe ones are similarly.
                unsafe {
                    self.tx_queue
                        .pop_used(
                            token1,
                            &[&stream_id.to_le_bytes(), &frames[last_start1..last_end1]],
                            &mut [&mut outputs],
                        )
                        .unwrap(); // This operation will never return Err(_), so using unwarp() is reasonable.
                }
                turn1 = false;
                token1 = unsafe {
                    self.tx_queue
                        .add(
                            &[&stream_id.to_le_bytes(), &frames[start_byte..end_byte]],
                            &mut [&mut outputs],
                        )
                        .unwrap()
                };
                last_start1 = start_byte;
                last_end1 = end_byte;
            } else {
                while !self.tx_queue.can_pop() {
                    spin_loop();
                }
                unsafe {
                    self.tx_queue
                        .pop_used(
                            token2,
                            &[&stream_id.to_le_bytes(), &frames[last_start2..last_end2]],
                            &mut [&mut outputs],
                        )
                        .unwrap(); // This operation will never return Err(_), so using unwarp() is reasonable.
                }
                turn1 = true;
                token2 = unsafe {
                    self.tx_queue
                        .add(
                            &[&stream_id.to_le_bytes(), &frames[start_byte..end_byte]],
                            &mut [&mut outputs],
                        )
                        .unwrap()
                };
                last_start2 = start_byte;
                last_end2 = end_byte;
            }
        }
        // wait for the last buffer
        if turn1 {
            while !self.tx_queue.can_pop() {
                spin_loop()
            }
            unsafe {
                self.tx_queue
                    .pop_used(
                        token1,
                        &[&stream_id.to_le_bytes(), &frames[last_start1..last_end1]],
                        &mut [&mut outputs],
                    )
                    .unwrap();
            }
        } else {
            while !self.tx_queue.can_pop() {
                spin_loop()
            }
            unsafe {
                self.tx_queue
                    .pop_used(
                        token2,
                        &[&stream_id.to_le_bytes(), &frames[last_start2..last_end2]],
                        &mut [&mut outputs],
                    )
                    .unwrap();
            }
        }
        Ok(())
    }

    /// Transfer PCM frame to device, based on the stream type(OUTPUT/INPUT).
    ///
    /// Currently supports only output stream.
    ///
    /// This is a non-blocking method that returns a token.
    ///
    /// The length of the `frames` must be equal to the buffer size set for the stream corresponding to the `stream_id`.
    pub fn pcm_xfer_nb(&mut self, stream_id: u32, frames: &[u8]) -> Result<u16> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if !self.pcm_parameters[stream_id as usize].setup {
            warn!("Please set parameters for a stream before using it!");
            return Err(Error::IoError);
        }
        const U32_SIZE: usize = size_of::<u32>();
        let buffer_size: usize = self.pcm_parameters[stream_id as usize].buffer_bytes as usize;
        assert_eq!(buffer_size, frames.len());
        let mut buf = vec![0; U32_SIZE + buffer_size];
        buf[..U32_SIZE].copy_from_slice(&stream_id.to_le_bytes());
        buf[U32_SIZE..U32_SIZE + buffer_size].copy_from_slice(frames);
        let mut rsp = Box::new([0; RSP_SIZE]);
        let token = unsafe { self.tx_queue.add(&[&buf], &mut [rsp.as_mut()])? };
        if self.tx_queue.should_notify() {
            self.transport.notify(TX_QUEUE_IDX);
        }
        self.token_buf.insert(token, buf);
        self.token_rsp.insert(token, rsp);
        Ok(token)
    }

    /// The PCM frame transmission corresponding to the given token has been completed.
    pub fn pcm_xfer_ok(&mut self, token: u16) -> Result {
        assert!(self.token_buf.contains_key(&token));
        assert!(self.token_rsp.contains_key(&token));
        let mut rsp = self.token_rsp[&token].clone();
        if unsafe {
            self.tx_queue
                .pop_used(token, &[&self.token_buf[&token]], &mut [rsp.as_mut()])
        }
        .is_err()
        {
            Err(Error::IoError)
        } else {
            self.token_buf.remove(&token);
            self.token_rsp.remove(&token);
            Ok(())
        }
    }

    /// Get all output streams.
    pub fn output_streams(&mut self) -> Vec<u32> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        self.pcm_infos
            .as_ref()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, info)| info.direction == VIRTIO_SND_D_OUTPUT)
            .map(|(idx, _)| idx as u32)
            .collect()
    }

    /// Get all input streams.
    pub fn input_streams(&mut self) -> Vec<u32> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        self.pcm_infos
            .as_ref()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, info)| info.direction == VIRTIO_SND_D_INPUT)
            .map(|(idx, _)| idx as u32)
            .collect()
    }

    /// Get the rates that a stream supports.
    pub fn rates_supported(&mut self, stream_id: u32) -> Result<PcmRates> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if stream_id >= self.pcm_infos.as_ref().unwrap().len() as u32 {
            return Err(Error::InvalidParam);
        }
        Ok(PcmRates::from_bits_retain(
            self.pcm_infos.as_ref().unwrap()[stream_id as usize].rates,
        ))
    }

    /// Get the formats that a stream supports.
    pub fn formats_supported(&mut self, stream_id: u32) -> Result<PcmFormats> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if stream_id >= self.pcm_infos.as_ref().unwrap().len() as u32 {
            return Err(Error::InvalidParam);
        }
        Ok(PcmFormats::from_bits_retain(
            self.pcm_infos.as_ref().unwrap()[stream_id as usize].formats,
        ))
    }

    /// Get channel range that a stream supports.
    pub fn channel_range_supported(&mut self, stream_id: u32) -> Result<RangeInclusive<u8>> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if stream_id >= self.pcm_infos.as_ref().unwrap().len() as u32 {
            return Err(Error::InvalidParam);
        }
        let pcm_info = &self.pcm_infos.as_ref().unwrap()[stream_id as usize];
        Ok(pcm_info.channels_min..=pcm_info.channels_max)
    }

    /// Get features that a stream supports.
    pub fn features_supported(&mut self, stream_id: u32) -> Result<PcmFeatures> {
        if !self.set_up {
            self.set_up();
            self.set_up = true;
        }
        if stream_id >= self.pcm_infos.as_ref().unwrap().len() as u32 {
            return Err(Error::InvalidParam);
        }
        let pcm_info = &self.pcm_infos.as_ref().unwrap()[stream_id as usize];
        Ok(PcmFeatures::from_bits_retain(pcm_info.features))
    }

    /// Get the latest notification.
    pub fn latest_notification(&mut self) -> Result<Option<Notification>> {
        // If the device has written notifications to the event_queue,
        // then the oldest notification should be at the front of the queue.
        self.event_queue.poll(&mut self.transport, |buffer| {
            Ok(VirtIOSndEvent::read_from(buffer).map(|event| Notification {
                notification_type: NotificationType::from(event.hdr.command_code),
                data: event.data,
            }))
        })
    }
}

/// The status of the PCM stream.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
enum PCMState {
    #[default]
    SetParams,
    Prepare,
    Release,
    Start,
    Stop,
}

const QUEUE_SIZE: u16 = 32;
const CONTROL_QUEUE_IDX: u16 = 0;
const EVENT_QUEUE_IDX: u16 = 1;
const TX_QUEUE_IDX: u16 = 2;
const RX_QUEUE_IDX: u16 = 3;

const SUPPORTED_FEATURES: Feature = Feature::RING_INDIRECT_DESC.union(Feature::RING_EVENT_IDX);

bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    #[repr(transparent)]
    struct JackFeatures: u32 {
        /// Jack remapping support.
        const REMAP = 1 << 0;
    }
}

bitflags! {
    /// Supported PCM stream features.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    #[repr(transparent)]
    pub struct PcmFeatures: u32 {
        /// Supports sharing a host memory with a guest.
        const SHMEM_HOST = 1 << 0;
        /// Supports sharing a guest memory with a host.
        const SHMEM_GUEST = 1 << 1;
        /// Supports polling mode for message-based transport.
        const MSG_POLLING = 1 << 2;
        /// Supports elapsed period notifications for shared memory transport.
        const EVT_SHMEM_PERIODS = 1 << 3;
        /// Supports underrun/overrun notifications.
        const EVT_XRUNS = 1 << 4;
    }
}

bitflags! {
    /// Supported PCM sample formats.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    #[repr(transparent)]
    pub struct PcmFormats: u64 {
        /// IMA ADPCM format.
        const IMA_ADPCM = 1 << 0;
        /// Mu-law format.
        const MU_LAW = 1 << 1;
        /// A-law format.
        const A_LAW = 1 << 2;
        /// Signed 8-bit format.
        const S8 = 1 << 3;
        /// Unsigned 8-bit format.
        const U8 = 1 << 4;
        /// Signed 16-bit format.
        const S16 = 1 << 5;
        /// Unsigned 16-bit format.
        const U16 = 1 << 6;
        /// Signed 18.3-bit format.
        const S18_3 = 1 << 7;
        /// Unsigned 18.3-bit format.
        const U18_3 = 1 << 8;
        /// Signed 20.3-bit format.
        const S20_3 = 1 << 9;
        /// Unsigned 20.3-bit format.
        const U20_3 = 1 << 10;
        /// Signed 24.3-bit format.
        const S24_3 = 1 << 11;
        /// Unsigned 24.3-bit format.
        const U24_3 = 1 << 12;
        /// Signed 20-bit format.
        const S20 = 1 << 13;
        /// Unsigned 20-bit format.
        const U20 = 1 << 14;
        /// Signed 24-bit format.
        const S24 = 1 << 15;
        /// Unsigned 24-bit format.
        const U24 = 1 << 16;
        /// Signed 32-bit format.
        const S32 = 1 << 17;
        /// Unsigned 32-bit format.
        const U32 = 1 << 18;
        /// 32-bit floating-point format.
        const FLOAT = 1 << 19;
        /// 64-bit floating-point format.
        const FLOAT64 = 1 << 20;
        /// DSD unsigned 8-bit format.
        const DSD_U8 = 1 << 21;
        /// DSD unsigned 16-bit format.
        const DSD_U16 = 1 << 22;
        /// DSD unsigned 32-bit format.
        const DSD_U32 = 1 << 23;
        /// IEC958 subframe format.
        const IEC958_SUBFRAME = 1 << 24;
    }
}

/// A single PCM sample format.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum PcmFormat {
    /// IMA ADPCM format.
    #[default]
    ImaAdpcm = 0,
    /// Mu-law format.
    MuLaw = 1,
    /// A-law format.
    ALaw = 2,
    /// Signed 8-bit format.
    S8 = 3,
    /// Unsigned 8-bit format.
    U8 = 4,
    /// Signed 16-bit format.
    S16 = 5,
    /// Unsigned 16-bit format.
    U16 = 6,
    /// Signed 18.3-bit format.
    S18_3 = 7,
    /// Unsigned 18.3-bit format.
    U18_3 = 8,
    /// Signed 20.3-bit format.
    S20_3 = 9,
    /// Unsigned 20.3-bit format.
    U20_3 = 10,
    /// Signed 24.3-bit format.
    S24_3 = 11,
    /// Unsigned 24.3-bit format.
    U24_3 = 12,
    /// Signed 20-bit format.
    S20 = 13,
    /// Unsigned 20-bit format.
    U20 = 14,
    /// Signed 24-bit format.
    S24 = 15,
    /// Unsigned 24-bit format.
    U24 = 16,
    /// Signed 32-bit format.
    S32 = 17,
    /// Unsigned 32-bit format.
    U32 = 18,
    /// 32-bit floating-point format.
    FLOAT = 19,
    /// 64-bit floating-point format.
    FLOAT64 = 20,
    /// DSD unsigned 8-bit format.
    DsdU8 = 21,
    /// DSD unsigned 16-bit format.
    DsdU16 = 22,
    /// DSD unsigned 32-bit format.
    DsdU32 = 23,
    /// IEC958 subframe format.
    Iec958Subframe = 24,
}

impl From<PcmFormat> for PcmFormats {
    fn from(format: PcmFormat) -> Self {
        match format {
            PcmFormat::ImaAdpcm => PcmFormats::IMA_ADPCM,
            PcmFormat::MuLaw => PcmFormats::MU_LAW,
            PcmFormat::ALaw => PcmFormats::A_LAW,
            PcmFormat::S8 => PcmFormats::S8,
            PcmFormat::U8 => PcmFormats::U8,
            PcmFormat::S16 => PcmFormats::S16,
            PcmFormat::U16 => PcmFormats::U16,
            PcmFormat::S18_3 => PcmFormats::S18_3,
            PcmFormat::U18_3 => PcmFormats::U18_3,
            PcmFormat::S20_3 => PcmFormats::S20_3,
            PcmFormat::U20_3 => PcmFormats::U20_3,
            PcmFormat::S24_3 => PcmFormats::S24_3,
            PcmFormat::U24_3 => PcmFormats::U24_3,
            PcmFormat::S20 => PcmFormats::S20,
            PcmFormat::U20 => PcmFormats::U20,
            PcmFormat::S24 => PcmFormats::S24,
            PcmFormat::U24 => PcmFormats::U24,
            PcmFormat::S32 => PcmFormats::S32,
            PcmFormat::U32 => PcmFormats::U32,
            PcmFormat::FLOAT => PcmFormats::FLOAT,
            PcmFormat::FLOAT64 => PcmFormats::FLOAT64,
            PcmFormat::DsdU8 => PcmFormats::DSD_U8,
            PcmFormat::DsdU16 => PcmFormats::DSD_U16,
            PcmFormat::DsdU32 => PcmFormats::DSD_U32,
            PcmFormat::Iec958Subframe => PcmFormats::IEC958_SUBFRAME,
        }
    }
}

impl From<PcmFormat> for u8 {
    fn from(format: PcmFormat) -> u8 {
        format as _
    }
}

bitflags! {
    /// Supported PCM frame rates.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    #[repr(transparent)]
    pub struct PcmRates: u64 {
        /// 5512 Hz PCM rate.
        const RATE_5512 = 1 << 0;
        /// 8000 Hz PCM rate.
        const RATE_8000 = 1 << 1;
        /// 11025 Hz PCM rate.
        const RATE_11025 = 1 << 2;
        /// 16000 Hz PCM rate.
        const RATE_16000 = 1 << 3;
        /// 22050 Hz PCM rate.
        const RATE_22050 = 1 << 4;
        /// 32000 Hz PCM rate.
        const RATE_32000 = 1 << 5;
        /// 44100 Hz PCM rate.
        const RATE_44100 = 1 << 6;
        /// 48000 Hz PCM rate.
        const RATE_48000 = 1 << 7;
        /// 64000 Hz PCM rate.
        const RATE_64000 = 1 << 8;
        /// 88200 Hz PCM rate.
        const RATE_88200 = 1 << 9;
        /// 96000 Hz PCM rate.
        const RATE_96000 = 1 << 10;
        /// 176400 Hz PCM rate.
        const RATE_176400 = 1 << 11;
        /// 192000 Hz PCM rate.
        const RATE_192000 = 1 << 12;
        /// 384000 Hz PCM rate.
        const RATE_384000 = 1 << 13;
    }
}

/// A PCM frame rate.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum PcmRate {
    /// 5512 Hz PCM rate.
    #[default]
    Rate5512 = 0,
    /// 8000 Hz PCM rate.
    Rate8000 = 1,
    /// 11025 Hz PCM rate.
    Rate11025 = 2,
    /// 16000 Hz PCM rate.
    Rate16000 = 3,
    /// 22050 Hz PCM rate.
    Rate22050 = 4,
    /// 32000 Hz PCM rate.
    Rate32000 = 5,
    /// 44100 Hz PCM rate.
    Rate44100 = 6,
    /// 48000 Hz PCM rate.
    Rate48000 = 7,
    /// 64000 Hz PCM rate.
    Rate64000 = 8,
    /// 88200 Hz PCM rate.
    Rate88200 = 9,
    /// 96000 Hz PCM rate.
    Rate96000 = 10,
    /// 176400 Hz PCM rate.
    Rate176400 = 11,
    /// 192000 Hz PCM rate.
    Rate192000 = 12,
    /// 384000 Hz PCM rate.
    Rate384000 = 13,
}

impl From<PcmRate> for PcmRates {
    fn from(rate: PcmRate) -> Self {
        match rate {
            PcmRate::Rate5512 => Self::RATE_5512,
            PcmRate::Rate8000 => Self::RATE_8000,
            PcmRate::Rate11025 => Self::RATE_11025,
            PcmRate::Rate16000 => Self::RATE_16000,
            PcmRate::Rate22050 => Self::RATE_22050,
            PcmRate::Rate32000 => Self::RATE_32000,
            PcmRate::Rate44100 => Self::RATE_44100,
            PcmRate::Rate48000 => Self::RATE_48000,
            PcmRate::Rate64000 => Self::RATE_64000,
            PcmRate::Rate88200 => Self::RATE_88200,
            PcmRate::Rate96000 => Self::RATE_96000,
            PcmRate::Rate176400 => Self::RATE_176400,
            PcmRate::Rate192000 => Self::RATE_192000,
            PcmRate::Rate384000 => Self::RATE_384000,
        }
    }
}

impl From<PcmRate> for u8 {
    fn from(rate: PcmRate) -> Self {
        rate as _
    }
}

#[repr(C)]
struct VirtIOSoundConfig {
    jacks: ReadOnly<u32>,
    streams: ReadOnly<u32>,
    chmaps: ReadOnly<u32>,
}

/// In virtIO v1.2, this enum should be called "Code".
///
/// To avoid ambiguity in its meaning, I use the term "CommandCode" here.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, FromPrimitive, IntoPrimitive, PartialEq)]
enum CommandCode {
    /* jack control request types */
    RJackInfo = 1,
    RJackRemap,

    /* PCM control request types */
    RPcmInfo = 0x0100,
    RPcmSetParams,
    RPcmPrepare,
    RPcmRelease,
    RPcmStart,
    RPcmStop,

    /* channel map control request types */
    RChmapInfo = 0x0200,

    /* jack event types */
    EvtJackConnected = 0x1000,
    EvtJackDisconnected,

    /* PCM event types */
    EvtPcmPeriodElapsed = 0x1100,
    EvtPcmXrun,

    /* common status codes */
    /// success
    #[num_enum(default)]
    SOk = 0x8000,
    /// a control message is malformed or contains invalid parameters
    SBadMsg,
    /// requested operation or parameters are not supported
    SNotSupp,
    ///  an I/O error occurred
    SIoErr,
}

/// Enum representing the types of item information requests.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemInformationRequestType {
    /// Represents a jack information request.
    RJackInfo = 1,
    /// Represents a PCM information request.
    RPcmInfo = 0x0100,
    /// Represents a channel map information request.
    RChmapInfo = 0x0200,
}

impl From<ItemInformationRequestType> for VirtIOSndHdr {
    fn from(value: ItemInformationRequestType) -> Self {
        VirtIOSndHdr {
            command_code: value.into(),
        }
    }
}

impl From<ItemInformationRequestType> for u32 {
    fn from(request_type: ItemInformationRequestType) -> u32 {
        request_type as _
    }
}

#[derive(IntoPrimitive)]
#[repr(u32)]
enum RequestStatusCode {
    /* common status codes */
    Ok = 0x8000,
    BadMsg,
    NotSupp,
    IoErr,
}

impl From<RequestStatusCode> for VirtIOSndHdr {
    fn from(value: RequestStatusCode) -> Self {
        VirtIOSndHdr {
            command_code: value as _,
        }
    }
}

/// A common header
#[repr(C)]
#[derive(AsBytes, Clone, Debug, Eq, FromBytes, FromZeroes, PartialEq)]
struct VirtIOSndHdr {
    command_code: u32,
}

impl From<CommandCode> for VirtIOSndHdr {
    fn from(value: CommandCode) -> Self {
        VirtIOSndHdr {
            command_code: value.into(),
        }
    }
}

#[repr(C)]
#[derive(FromBytes, FromZeroes)]
/// An event notification
struct VirtIOSndEvent {
    hdr: VirtIOSndHdr,
    data: u32,
}

/// The notification type.
#[repr(u32)]
#[derive(Debug, FromPrimitive, Copy, Clone, PartialEq, Eq)]
pub enum NotificationType {
    /// A hardware buffer period has elapsed, the period size is controlled using the `period_bytes` field.
    #[num_enum(default)]
    PcmPeriodElapsed = 0x1100,
    /// An underflow for the output stream or an overflow for the inputstream has occurred.
    PcmXrun,
    /// An external device has been connected to the jack.
    JackConnected = 0x1000,
    /// An external device has been disconnected from the jack.
    JackDisconnected,
}

/// Notification from sound device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    notification_type: NotificationType,
    data: u32,
}

impl Notification {
    /// Get the resource index.
    pub fn data(&self) -> u32 {
        self.data
    }

    /// Get the notification type.
    pub fn notification_type(&self) -> NotificationType {
        self.notification_type
    }
}

const VIRTIO_SND_D_OUTPUT: u8 = 0;
const VIRTIO_SND_D_INPUT: u8 = 1;

#[repr(C)]
#[derive(AsBytes, Debug, FromBytes, FromZeroes)]
struct VirtIOSndQueryInfo {
    /// specifies a particular item request type (VIRTIO_SND_R_*_INFO)
    hdr: VirtIOSndHdr,
    /// specifies the starting identifier for the item
    /// (the range of available identifiers is limited
    ///  by the total number of particular items
    ///  that is indicated in the device configuration space)
    start_id: u32,
    /// specifies the number of items for which information is requested
    ///  (the total number of particular items is indicated
    ///  in the device configuration space).
    count: u32,
    /// specifies the size of the structure containing information for one item
    /// (used for backward compatibility)
    size: u32,
}

#[repr(C)]
#[derive(AsBytes, Debug, FromBytes, FromZeroes)]
struct VirtIOSndQueryInfoRsp {
    hdr: VirtIOSndHdr,
    info: VirtIOSndInfo,
}

/// Field `hda_fn_nid` indicates a function group node identifier.
#[repr(C)]
#[derive(AsBytes, Clone, Debug, Eq, FromBytes, FromZeroes, PartialEq)]
pub struct VirtIOSndInfo {
    hda_fn_nid: u32,
}

#[repr(C)]
#[derive(AsBytes, Clone, Debug, FromBytes, FromZeroes)]
struct VirtIOSndJackHdr {
    hdr: VirtIOSndHdr,
    /// specifies a jack identifier from 0 to jacks - 1
    jack_id: u32,
}

/// Jack infomation.
#[repr(C)]
#[derive(AsBytes, Clone, Eq, FromBytes, FromZeroes, PartialEq)]
pub struct VirtIOSndJackInfo {
    hdr: VirtIOSndInfo,
    features: u32,
    /// indicates a pin default configuration value
    hda_reg_defconf: u32,
    /// indicates a pin capabilities value
    hda_reg_caps: u32,
    /// indicates the current jack connection status (1 - connected, 0 - disconnected)
    connected: u8,

    _padding: [u8; 7],
}

impl Debug for VirtIOSndJackInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("VirtIOSndJackInfo")
            .field("hdr", &self.hdr)
            .field("features", &JackFeatures::from_bits_retain(self.features))
            .field("hda_reg_defconf", &self.hda_reg_defconf)
            .field("hda_reg_caps", &self.hda_reg_caps)
            .field("connected", &self.connected)
            .field("_padding", &self._padding)
            .finish()
    }
}

impl Display for VirtIOSndJackInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let connected_status = if self.connected == 1 {
            "CONNECTED"
        } else {
            "DISCONNECTED"
        };
        write!(
            f,
            "features: {:?}, hda_reg_defconf: {}, hda_reg_caps: {}, connected: {}",
            JackFeatures::from_bits_retain(self.features),
            self.hda_reg_defconf,
            self.hda_reg_caps,
            connected_status
        )
    }
}

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
struct VirtIOSndJackInfoRsp {
    hdr: VirtIOSndHdr,
    body: VirtIOSndJackInfo,
}

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
struct VirtIOSndJackRemap {
    hdr: VirtIOSndJackHdr,
    association: u32,
    sequence: u32,
}

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
struct VirtIOSndPcmHdr {
    /// specifies request type (VIRTIO_SND_R_PCM_*)
    hdr: VirtIOSndHdr,
    /// specifies a PCM stream identifier from 0 to streams - 1
    stream_id: u32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PcmStreamFeatures {
    ShmemHost = 0,
    ShmemGuest,
    MsgPolling,
    EvtShmemPeriods,
    EvtXruns,
}

impl From<PcmStreamFeatures> for u32 {
    fn from(value: PcmStreamFeatures) -> Self {
        value as _
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u64)]
enum PcmSampleFormat {
    /* analog formats (width / physical width) */
    ImaAdpcm = 0, /* 4 / 4 bits */
    MuLaw,        /* 8 / 8 bits */
    ALaw,         /* 8 / 8 bits */
    S8,           /* 8 / 8 bits */
    U8,           /* 8 / 8 bits */
    S16,          /* 16 / 16 bits */
    U16,          /* 16 / 16 bits */
    S18_3,        /* 18 / 24 bits */
    U18_3,        /* 18 / 24 bits */
    S20_3,        /* 20 / 24 bits */
    U20_3,        /* 20 / 24 bits */
    S24_3,        /* 24 / 24 bits */
    U24_3,        /* 24 / 24 bits */
    S20,          /* 20 / 32 bits */
    U20,          /* 20 / 32 bits */
    S24,          /* 24 / 32 bits */
    U24,          /* 24 / 32 bits */
    S32,          /* 32 / 32 bits */
    U32,          /* 32 / 32 bits */
    Float,        /* 32 / 32 bits */
    Float64,      /* 64 / 64 bits */
    /* digital formats (width / physical width) */
    DsdU8,          /* 8 / 8 bits */
    DsdU16,         /* 16 / 16 bits */
    DsdU32,         /* 32 / 32 bits */
    Iec958Subframe, /* 32 / 32 bits */
}

impl From<PcmSampleFormat> for u64 {
    fn from(value: PcmSampleFormat) -> Self {
        value as _
    }
}

/// PCM information.
#[repr(C)]
#[derive(AsBytes, Clone, Eq, FromBytes, FromZeroes, PartialEq)]
pub struct VirtIOSndPcmInfo {
    hdr: VirtIOSndInfo,
    features: u32, /* 1 << VIRTIO_SND_PCM_F_XXX */
    formats: u64,  /* 1 << VIRTIO_SND_PCM_FMT_XXX */
    rates: u64,    /* 1 << VIRTIO_SND_PCM_RATE_XXX */
    /// indicates the direction of data flow (VIRTIO_SND_D_*)
    direction: u8,
    /// indicates a minimum number of supported channels
    channels_min: u8,
    /// indicates a maximum number of supported channels
    channels_max: u8,
    _padding: [u8; 5],
}

impl Debug for VirtIOSndPcmInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("VirtIOSndPcmInfo")
            .field("hdr", &self.hdr)
            .field("features", &PcmFeatures::from_bits_retain(self.features))
            .field("formats", &PcmFormats::from_bits_retain(self.formats))
            .field("rates", &PcmRates::from_bits_retain(self.rates))
            .field("direction", &self.direction)
            .field("channels_min", &self.channels_min)
            .field("channels_max", &self.channels_max)
            .field("_padding", &self._padding)
            .finish()
    }
}

impl Display for VirtIOSndPcmInfo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let direction = if self.direction == VIRTIO_SND_D_INPUT {
            "INPUT"
        } else {
            "OUTPUT"
        };
        write!(
            f,
            "features: {:?}, rates: {:?}, formats: {:?}, direction: {}",
            PcmFeatures::from_bits_retain(self.features),
            PcmRates::from_bits_retain(self.rates),
            PcmFormats::from_bits_retain(self.formats),
            direction
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PcmParameters {
    setup: bool,
    buffer_bytes: u32,
    period_bytes: u32,
    features: PcmFeatures,
    channels: u8,
    format: PcmFormat,
    rate: PcmRate,
}

#[repr(C)]
#[derive(AsBytes, FromBytes, FromZeroes)]
struct VirtIOSndPcmSetParams {
    hdr: VirtIOSndPcmHdr, /* .code = VIRTIO_SND_R_PCM_SET_PARAMS */
    buffer_bytes: u32,
    period_bytes: u32,
    features: u32, /* 1 << VIRTIO_SND_PCM_F_XXX */
    channels: u8,
    format: u8,
    rate: u8,

    _padding: u8,
}

/// An I/O header
#[repr(C)]
#[derive(AsBytes)]
struct VirtIOSndPcmXfer {
    srteam_id: u32,
}

/// An I/O status
#[repr(C)]
#[derive(FromBytes, FromZeroes)]
struct VirtIOSndPcmStatus {
    status: u32,
    latency_bytes: u32,
}

#[derive(FromPrimitive, Debug)]
#[repr(u8)]
enum ChannelPosition {
    #[num_enum(default)]
    /// undefined
    None = 0,
    /// silent
    Na,
    /// mono stream
    Mono,
    /// front left
    Fl,
    /// front right
    Fr,
    /// rear left
    Rl,
    /// rear right
    Rr,
    /// front center
    Fc,
    /// low frequency (LFE)
    Lfe,
    /// side left
    Sl,
    /// side right
    Sr,
    /// rear center
    Rc,
    /// front left center
    Flc,
    /// front right center
    Frc,
    /// rear left center
    Rlc,
    /// rear right center
    Rrc,
    /// front left wide
    Flw,
    /// front right wide
    Frw,
    /// front left high
    Flh,
    /// front center high
    Fch,
    /// front right high
    Frh,
    /// top center
    Tc,
    /// top front left
    Tfl,
    /// top front right
    Tfr,
    /// top front center
    Tfc,
    /// top rear left
    Trl,
    /// top rear right
    Trr,
    /// top rear center
    Trc,
    /// top front left center
    Tflc,
    /// top front right center
    Tfrc,
    /// top side left
    Tsl,
    /// top side right
    Tsr,
    /// left LFE
    Llfe,
    /// right LFE
    Rlfe,
    /// bottom center
    Bc,
    /// bottom left center
    Blc,
    /// bottom right center
    Brc,
}

/// maximum possible number of channels
const VIRTIO_SND_CHMAP_MAX_SIZE: usize = 18;

#[repr(C)]
#[derive(AsBytes, Clone, Debug, FromBytes, FromZeroes)]
struct VirtIOSndChmapInfo {
    hdr: VirtIOSndInfo,
    direction: u8,
    channels: u8,
    positions: [u8; VIRTIO_SND_CHMAP_MAX_SIZE],
}

impl Display for VirtIOSndChmapInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let direction = if self.direction == VIRTIO_SND_D_INPUT {
            "INPUT"
        } else {
            "OUTPUT"
        };
        write!(
            f,
            "direction: {}, channels: {}, postions: [",
            direction, self.channels
        )?;
        for i in 0..usize::from(self.channels) {
            if i != 0 {
                write!(f, ", ")?;
            }
            write!(f, "{:?}", ChannelPosition::from(self.positions[i]))?;
        }
        write!(f, "]")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hal::fake::FakeHal,
        transport::{
            fake::{FakeTransport, QueueStatus, State},
            DeviceType,
        },
        volatile::ReadOnly,
    };
    use alloc::{sync::Arc, vec};
    use core::ptr::NonNull;
    use fake::FakeSoundDevice;
    use std::sync::Mutex;

    #[test]
    fn config() {
        let mut config_space = VirtIOSoundConfig {
            jacks: ReadOnly::new(3),
            streams: ReadOnly::new(4),
            chmaps: ReadOnly::new(2),
        };
        let state = Arc::new(Mutex::new(State {
            queues: vec![
                QueueStatus::default(),
                QueueStatus::default(),
                QueueStatus::default(),
                QueueStatus::default(),
            ],
            ..Default::default()
        }));
        let transport = FakeTransport {
            device_type: DeviceType::Sound,
            max_queue_size: 32,
            device_features: 0,
            config_space: NonNull::from(&mut config_space),
            state: state.clone(),
        };
        let sound =
            VirtIOSound::<FakeHal, FakeTransport<VirtIOSoundConfig>>::new(transport).unwrap();
        assert_eq!(sound.jacks(), 3);
        assert_eq!(sound.streams(), 4);
        assert_eq!(sound.chmaps(), 2);
    }

    #[test]
    fn empty_info() {
        let (fake, transport) = FakeSoundDevice::new(vec![], vec![], vec![]);
        let mut sound =
            VirtIOSound::<FakeHal, FakeTransport<VirtIOSoundConfig>>::new(transport).unwrap();
        let handle = fake.spawn();

        assert_eq!(sound.jacks(), 0);
        assert_eq!(sound.streams(), 0);
        assert_eq!(sound.chmaps(), 0);
        assert_eq!(sound.output_streams(), vec![]);
        assert_eq!(sound.input_streams(), vec![]);

        fake.terminate();
        handle.join().unwrap();
    }

    #[test]
    fn stream_info() {
        let (fake, transport) = FakeSoundDevice::new(
            vec![VirtIOSndJackInfo {
                hdr: VirtIOSndInfo { hda_fn_nid: 0 },
                features: 0,
                hda_reg_defconf: 0,
                hda_reg_caps: 0,
                connected: 0,
                _padding: Default::default(),
            }],
            vec![
                VirtIOSndPcmInfo {
                    hdr: VirtIOSndInfo { hda_fn_nid: 0 },
                    features: 0,
                    formats: 0,
                    rates: 0,
                    direction: VIRTIO_SND_D_OUTPUT,
                    channels_min: 0,
                    channels_max: 0,
                    _padding: Default::default(),
                },
                VirtIOSndPcmInfo {
                    hdr: VirtIOSndInfo { hda_fn_nid: 0 },
                    features: 0,
                    formats: 0,
                    rates: 0,
                    direction: VIRTIO_SND_D_INPUT,
                    channels_min: 0,
                    channels_max: 0,
                    _padding: Default::default(),
                },
            ],
            vec![VirtIOSndChmapInfo {
                hdr: VirtIOSndInfo { hda_fn_nid: 0 },
                direction: 0,
                channels: 0,
                positions: [0; 18],
            }],
        );
        let mut sound =
            VirtIOSound::<FakeHal, FakeTransport<VirtIOSoundConfig>>::new(transport).unwrap();
        let handle = fake.spawn();

        assert_eq!(sound.output_streams(), vec![0]);
        assert_eq!(sound.input_streams(), vec![1]);

        fake.terminate();
        handle.join().unwrap();
    }
}
