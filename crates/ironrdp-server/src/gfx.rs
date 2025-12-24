//! EGFX (Graphics Pipeline Extension) server factory trait
//!
//! This module provides the factory trait for creating EGFX handlers
//! that implement H.264 video streaming via the Graphics Pipeline Extension.

use ironrdp_egfx::server::GraphicsPipelineHandler;

/// Factory trait for creating EGFX graphics pipeline handlers
///
/// Implementors provide a handler that receives EGFX callbacks
/// (capability negotiation, frame acknowledgments, etc.) and can
/// send H.264 video frames to the client.
pub trait GfxServerFactory: Send {
    /// Create a new graphics pipeline handler
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler>;
}
