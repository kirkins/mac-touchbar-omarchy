//! The level-control wire protocol: what a slider asks the desktop
//! receiver to do and what comes back. The socket clients live with the
//! daemon (`tiny-dfr`'s `level_control`); the protocol types live here
//! because the slider widgets' drawing and probe logic are expressed in
//! terms of them, and a preview renders the same request/reply shapes
//! without any socket.

/// The volume slider's documented ceiling: PipeWire accepts above 100,
/// and the shipped slider keeps 130 as the hard maximum.
pub const MAX_VOLUME_PERCENT: u8 = 130;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Get,
    Set(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub percent: u8,
    pub muted: bool,
}
