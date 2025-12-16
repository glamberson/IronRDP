//! Server-side EGFX implementation
//!
//! This module provides server-side support for the Graphics Pipeline Extension
//! (MS-RDPEGFX), enabling H.264/AVC420 video streaming to RDP clients.
//!
//! # Architecture
//!
//! The server follows this message flow:
//!
//! ```text
//! Client                                  Server
//!    |                                       |
//!    |--- CapabilitiesAdvertise ------------>|
//!    |                                       | (negotiate capabilities)
//!    |<----------- CapabilitiesConfirm ------|
//!    |<----------- ResetGraphics ------------|
//!    |<----------- CreateSurface ------------|
//!    |<----------- MapSurfaceToOutput -------|
//!    |                                       |
//!    |  (For each frame:)                    |
//!    |<----------- StartFrame ---------------|
//!    |<----------- WireToSurface1 -----------|  (H.264 data)
//!    |<----------- EndFrame -----------------|
//!    |                                       |
//!    |--- FrameAcknowledge ----------------->|  (flow control)
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use ironrdp_egfx::server::{GraphicsPipelineServer, GraphicsPipelineHandler};
//!
//! struct MyHandler;
//!
//! impl GraphicsPipelineHandler for MyHandler {
//!     fn capabilities_advertise(&mut self, pdu: CapabilitiesAdvertisePdu) {
//!         // Client sent capabilities
//!     }
//!
//!     fn on_ready(&mut self, surface_id: u16, width: u16, height: u16) {
//!         // Server is ready to send frames
//!     }
//!
//!     fn on_frame_ack(&mut self, frame_id: u32) {
//!         // Client acknowledged frame
//!     }
//! }
//!
//! let server = GraphicsPipelineServer::new(Box::new(MyHandler), 1920, 1080);
//! ```

use std::collections::VecDeque;

use ironrdp_core::{decode, impl_as_any};
use ironrdp_dvc::{DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::{decode_err, PduResult};
use tracing::{debug, trace, warn};

use crate::pdu::{
    Avc420Region, CacheImportOfferPdu, CapabilitiesAdvertisePdu, CapabilitiesConfirmPdu,
    CapabilitiesV81Flags, CapabilitySet, Codec1Type, CreateSurfacePdu, EndFramePdu,
    FrameAcknowledgePdu, GfxPdu, MapSurfaceToOutputPdu, PixelFormat, ResetGraphicsPdu,
    StartFramePdu, Timestamp, WireToSurface1Pdu, encode_avc420_bitmap_stream,
};
use crate::CHANNEL_NAME;

/// Maximum frames in flight before applying backpressure
const DEFAULT_MAX_FRAMES_IN_FLIGHT: u32 = 3;

/// Handler trait for server-side EGFX events
///
/// Implement this trait to receive callbacks when the EGFX channel state changes
/// or when client messages are received.
pub trait GraphicsPipelineHandler: Send {
    /// Called when the client advertises its capabilities
    ///
    /// The server will respond with `CapabilitiesConfirm` based on
    /// [`preferred_capabilities()`](Self::preferred_capabilities).
    fn capabilities_advertise(&mut self, pdu: CapabilitiesAdvertisePdu);

    /// Called when the client acknowledges a frame
    ///
    /// This is used for flow control. The default implementation does nothing.
    fn frame_acknowledge(&mut self, pdu: FrameAcknowledgePdu) {
        trace!(?pdu);
    }

    /// Called when the client offers to import cache entries
    ///
    /// The default implementation does nothing.
    fn cache_import_offer(&mut self, pdu: CacheImportOfferPdu) {
        trace!(?pdu);
    }

    /// Returns the server's preferred capabilities
    ///
    /// Override this to customize which codecs the server supports.
    /// The default enables AVC420 (H.264) with V8.1 capabilities.
    fn preferred_capabilities(&self) -> Vec<CapabilitySet> {
        vec![CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        }]
    }

    /// Called when the EGFX channel is ready to send frames
    ///
    /// At this point, the surface has been created and mapped to output.
    /// The server can begin sending H.264 frames.
    fn on_ready(&mut self, _surface_id: u16, _width: u16, _height: u16) {}

    /// Called when a frame has been acknowledged by the client
    ///
    /// This callback is separate from `frame_acknowledge` to provide
    /// a simpler interface for flow control tracking.
    fn on_frame_ack(&mut self, _frame_id: u32) {}

    /// Called when the EGFX channel is closed
    fn on_close(&mut self) {}
}

/// Server state machine states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    /// Waiting for client CapabilitiesAdvertise
    WaitingForCapabilities,
    /// Channel is ready, can send frames
    Ready,
    /// Channel has been closed
    Closed,
}

/// Server for the Graphics Pipeline Virtual Channel (EGFX)
///
/// This server handles capability negotiation, surface management,
/// and H.264 frame transmission to RDP clients.
pub struct GraphicsPipelineServer {
    handler: Box<dyn GraphicsPipelineHandler>,

    // State management
    state: ServerState,
    negotiated_caps: Option<CapabilitySet>,

    // Surface management
    surface_id: u16,
    width: u16,
    height: u16,

    // Frame flow control
    frame_id: u32,
    frames_in_flight: u32,
    max_frames_in_flight: u32,

    // Output queue for PDUs that need to be sent
    output_queue: VecDeque<GfxPdu>,
}

impl GraphicsPipelineServer {
    /// Create a new GraphicsPipelineServer
    ///
    /// # Arguments
    ///
    /// * `handler` - Handler for EGFX events
    /// * `width` - Initial desktop width
    /// * `height` - Initial desktop height
    pub fn new(handler: Box<dyn GraphicsPipelineHandler>, width: u16, height: u16) -> Self {
        Self {
            handler,
            state: ServerState::WaitingForCapabilities,
            negotiated_caps: None,
            surface_id: 0,
            width,
            height,
            frame_id: 0,
            frames_in_flight: 0,
            max_frames_in_flight: DEFAULT_MAX_FRAMES_IN_FLIGHT,
            output_queue: VecDeque::new(),
        }
    }

    /// Set the maximum frames in flight before backpressure is applied
    pub fn set_max_frames_in_flight(&mut self, max: u32) {
        self.max_frames_in_flight = max;
    }

    /// Check if the server is ready to send frames
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == ServerState::Ready
    }

    /// Check if backpressure should be applied
    ///
    /// Returns `true` if too many frames are in flight and the caller
    /// should drop or delay new frames.
    #[must_use]
    pub fn should_backpressure(&self) -> bool {
        self.frames_in_flight >= self.max_frames_in_flight
    }

    /// Get the number of frames currently in flight (awaiting ACK)
    #[must_use]
    pub fn frames_in_flight(&self) -> u32 {
        self.frames_in_flight
    }

    /// Get the current surface ID
    #[must_use]
    pub fn surface_id(&self) -> u16 {
        self.surface_id
    }

    /// Get the current desktop dimensions
    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Update the desktop size
    ///
    /// This should be called when the desktop is resized. The server will
    /// need to send new ResetGraphics and CreateSurface PDUs.
    pub fn set_dimensions(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Get the next frame ID
    fn next_frame_id(&mut self) -> u32 {
        let id = self.frame_id;
        self.frame_id = self.frame_id.wrapping_add(1);
        id
    }

    /// Queue an H.264 AVC420 frame for transmission
    ///
    /// # Arguments
    ///
    /// * `h264_data` - H.264 encoded data in AVC format (use `annex_b_to_avc` if needed)
    /// * `regions` - List of regions describing the frame
    /// * `timestamp_ms` - Frame timestamp in milliseconds
    ///
    /// # Returns
    ///
    /// `Some(frame_id)` if the frame was queued, `None` if backpressure is active
    /// or the server is not ready.
    pub fn send_avc420_frame(
        &mut self,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> Option<u32> {
        if !self.is_ready() {
            debug!("EGFX not ready, dropping frame");
            return None;
        }

        if self.should_backpressure() {
            trace!(
                frames_in_flight = self.frames_in_flight,
                "EGFX backpressure active"
            );
            return None;
        }

        let frame_id = self.next_frame_id();

        // Build the bitmap data
        let bitmap_data = encode_avc420_bitmap_stream(regions, h264_data);

        // Determine destination rectangle from regions
        let dest_rect = if let Some(first) = regions.first() {
            let mut left = first.left;
            let mut top = first.top;
            let mut right = first.right;
            let mut bottom = first.bottom;

            for r in regions.iter().skip(1) {
                left = left.min(r.left);
                top = top.min(r.top);
                right = right.max(r.right);
                bottom = bottom.max(r.bottom);
            }

            InclusiveRectangle { left, top, right, bottom }
        } else {
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: self.width.saturating_sub(1),
                bottom: self.height.saturating_sub(1),
            }
        };

        // Convert timestamp_ms to Timestamp struct
        let timestamp = Timestamp {
            milliseconds: (timestamp_ms % 1000) as u16,
            seconds: ((timestamp_ms / 1000) % 60) as u8,
            minutes: ((timestamp_ms / 60000) % 60) as u8,
            hours: ((timestamp_ms / 3600000) % 24) as u16,
        };

        // Queue the frame PDUs
        self.output_queue.push_back(GfxPdu::StartFrame(StartFramePdu {
            timestamp,
            frame_id,
        }));

        self.output_queue.push_back(GfxPdu::WireToSurface1(WireToSurface1Pdu {
            surface_id: self.surface_id,
            codec_id: Codec1Type::Avc420,
            pixel_format: PixelFormat::XRgb,
            destination_rectangle: dest_rect,
            bitmap_data,
        }));

        self.output_queue.push_back(GfxPdu::EndFrame(EndFramePdu { frame_id }));

        self.frames_in_flight += 1;

        trace!(
            frame_id,
            frames_in_flight = self.frames_in_flight,
            "Queued EGFX frame"
        );

        Some(frame_id)
    }

    /// Drain the output queue and return PDUs to send
    ///
    /// Call this method to get pending PDUs that need to be sent to the client.
    /// Returns a vector of boxed PDUs suitable for DVC transmission.
    pub fn drain_output(&mut self) -> Vec<DvcMessage> {
        self.output_queue
            .drain(..)
            .map(|pdu| Box::new(pdu) as DvcMessage)
            .collect()
    }

    /// Check if there are pending PDUs to send
    #[must_use]
    pub fn has_pending_output(&self) -> bool {
        !self.output_queue.is_empty()
    }

    /// Handle capability negotiation and surface setup
    fn handle_capabilities_advertise(&mut self, pdu: CapabilitiesAdvertisePdu) {
        debug!(?pdu, "Received CapabilitiesAdvertise");

        // Let handler process the capabilities
        self.handler.capabilities_advertise(pdu.clone());

        // Select the best capability set
        let server_caps = self.handler.preferred_capabilities();
        let negotiated = self.negotiate_capabilities(&pdu.0, &server_caps);

        self.negotiated_caps = Some(negotiated.clone());

        // Queue CapabilitiesConfirm
        self.output_queue
            .push_back(GfxPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(negotiated)));

        // Queue ResetGraphics
        self.output_queue
            .push_back(GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: u32::from(self.width),
                height: u32::from(self.height),
                monitors: Vec::new(),
            }));

        // Queue CreateSurface
        self.output_queue
            .push_back(GfxPdu::CreateSurface(CreateSurfacePdu {
                surface_id: self.surface_id,
                width: self.width,
                height: self.height,
                pixel_format: PixelFormat::XRgb,
            }));

        // Queue MapSurfaceToOutput
        self.output_queue
            .push_back(GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: self.surface_id,
                output_origin_x: 0,
                output_origin_y: 0,
            }));

        // Transition to ready state
        self.state = ServerState::Ready;

        // Notify handler
        self.handler.on_ready(self.surface_id, self.width, self.height);

        debug!(
            surface_id = self.surface_id,
            width = self.width,
            height = self.height,
            "EGFX server ready"
        );
    }

    /// Negotiate capabilities between client and server
    #[allow(clippy::unused_self)] // May use self for more sophisticated negotiation in the future
    fn negotiate_capabilities(
        &self,
        client_caps: &[CapabilitySet],
        server_caps: &[CapabilitySet],
    ) -> CapabilitySet {
        // Find the highest version supported by both
        // Priority: V10_1 > V10 > V8_1 > V8 > V6_1 > V5 > V4_1 > V4

        // For now, just return the first server capability
        // A more sophisticated implementation would intersect client and server caps
        if let Some(cap) = server_caps.first() {
            // Check if client supports this capability version
            for client_cap in client_caps {
                if core::mem::discriminant(client_cap) == core::mem::discriminant(cap) {
                    return cap.clone();
                }
            }
        }

        // Fallback to V8_1 with AVC420
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        }
    }

    /// Handle frame acknowledgment
    fn handle_frame_acknowledge(&mut self, pdu: FrameAcknowledgePdu) {
        trace!(?pdu, "Received FrameAcknowledge");

        // Decrement frames in flight
        if self.frames_in_flight > 0 {
            self.frames_in_flight -= 1;
        }

        // Notify handler
        self.handler.frame_acknowledge(pdu.clone());
        self.handler.on_frame_ack(pdu.frame_id);
    }
}

impl_as_any!(GraphicsPipelineServer);

impl DvcProcessor for GraphicsPipelineServer {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        debug!("EGFX channel started");
        // Server doesn't send anything at start - waits for client CapabilitiesAdvertise
        Ok(vec![])
    }

    fn close(&mut self, _channel_id: u32) {
        debug!("EGFX channel closed");
        self.state = ServerState::Closed;
        self.handler.on_close();
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let pdu = decode(payload).map_err(|e| decode_err!(e))?;

        match pdu {
            GfxPdu::CapabilitiesAdvertise(pdu) => {
                self.handle_capabilities_advertise(pdu);
            }
            GfxPdu::FrameAcknowledge(pdu) => {
                self.handle_frame_acknowledge(pdu);
            }
            GfxPdu::CacheImportOffer(pdu) => {
                self.handler.cache_import_offer(pdu);
            }
            _ => {
                warn!(?pdu, "Unhandled client GFX PDU");
            }
        }

        // Return any queued output
        Ok(self.drain_output())
    }
}

impl DvcServerProcessor for GraphicsPipelineServer {}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHandler {
        ready: bool,
        acked_frames: Vec<u32>,
    }

    impl TestHandler {
        fn new() -> Self {
            Self {
                ready: false,
                acked_frames: Vec::new(),
            }
        }
    }

    impl GraphicsPipelineHandler for TestHandler {
        fn capabilities_advertise(&mut self, _pdu: CapabilitiesAdvertisePdu) {}

        fn on_ready(&mut self, _surface_id: u16, _width: u16, _height: u16) {
            self.ready = true;
        }

        fn on_frame_ack(&mut self, frame_id: u32) {
            self.acked_frames.push(frame_id);
        }
    }

    #[test]
    fn test_server_creation() {
        let handler = Box::new(TestHandler::new());
        let server = GraphicsPipelineServer::new(handler, 1920, 1080);

        assert!(!server.is_ready());
        assert_eq!(server.frames_in_flight(), 0);
        assert_eq!(server.dimensions(), (1920, 1080));
    }

    #[test]
    fn test_server_not_ready() {
        let handler = Box::new(TestHandler::new());
        let mut server = GraphicsPipelineServer::new(handler, 1920, 1080);

        // Should return None when not ready
        let h264_data = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        let regions = vec![Avc420Region::full_frame(1920, 1080, 22)];

        let result = server.send_avc420_frame(&h264_data, &regions, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_backpressure() {
        let handler = Box::new(TestHandler::new());
        let mut server = GraphicsPipelineServer::new(handler, 1920, 1080);

        // Force ready state for testing
        server.state = ServerState::Ready;
        server.set_max_frames_in_flight(2);

        let h264_data = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        let regions = vec![Avc420Region::full_frame(1920, 1080, 22)];

        // First two frames should succeed
        assert!(server.send_avc420_frame(&h264_data, &regions, 0).is_some());
        assert!(server.send_avc420_frame(&h264_data, &regions, 16).is_some());

        // Third should fail due to backpressure
        // frames_in_flight == max_frames_in_flight, so we should be backpressured
        assert!(server.should_backpressure());
        assert!(server.send_avc420_frame(&h264_data, &regions, 33).is_none());
    }
}
