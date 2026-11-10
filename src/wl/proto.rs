//! Opcodes and enum values for the Wayland protocols notify-rs speaks. They are
//! checked against tests/fixtures/wayland_protocol.txt (transcribed from the
//! protocol XML) by tests/wayland_proto.rs.

pub mod display {
    pub const SYNC: u16 = 0;
    pub const GET_REGISTRY: u16 = 1;
    pub const ERROR: u16 = 0;
    pub const DELETE_ID: u16 = 1;
}
pub mod registry {
    pub const BIND: u16 = 0;
    pub const GLOBAL: u16 = 0;
    pub const GLOBAL_REMOVE: u16 = 1;
}
pub mod callback {
    pub const DONE: u16 = 0;
}
pub mod compositor {
    pub const CREATE_SURFACE: u16 = 0;
    pub const CREATE_REGION: u16 = 1;
}
pub mod region {
    pub const DESTROY: u16 = 0;
    pub const ADD: u16 = 1;
}
pub mod shm {
    pub const CREATE_POOL: u16 = 0;
    pub const FORMAT: u16 = 0;
    pub const FORMAT_ARGB8888: u32 = 0;
}
pub mod shm_pool {
    pub const CREATE_BUFFER: u16 = 0;
    pub const DESTROY: u16 = 1;
}
pub mod buffer {
    pub const DESTROY: u16 = 0;
    pub const RELEASE: u16 = 0;
}
pub mod seat {
    pub const GET_POINTER: u16 = 0;
    pub const RELEASE: u16 = 3;
    pub const CAPABILITIES: u16 = 0;
    pub const CAP_POINTER: u32 = 1;
}
pub mod pointer {
    pub const RELEASE: u16 = 1;
    pub const ENTER: u16 = 0;
    pub const LEAVE: u16 = 1;
    pub const MOTION: u16 = 2;
    pub const BUTTON: u16 = 3;
    pub const AXIS: u16 = 4;
    pub const AXIS_VERTICAL: u32 = 0;
    pub const BUTTON_PRESSED: u32 = 1;
}
pub mod output {
    pub const SCALE: u16 = 3;
    pub const RELEASE: u16 = 0;
}
pub mod surface {
    pub const DESTROY: u16 = 0;
    pub const ATTACH: u16 = 1;
    pub const SET_INPUT_REGION: u16 = 5;
    pub const COMMIT: u16 = 6;
    pub const SET_BUFFER_SCALE: u16 = 8;
    pub const DAMAGE_BUFFER: u16 = 9;
    pub const ENTER: u16 = 0;
    pub const LEAVE: u16 = 1;
    pub const PREFERRED_BUFFER_SCALE: u16 = 2;
}
pub mod layer_shell {
    pub const GET_LAYER_SURFACE: u16 = 0;
    pub const LAYER_TOP: u32 = 2;
    pub const LAYER_OVERLAY: u32 = 3;
}
pub mod layer_surface {
    pub const SET_SIZE: u16 = 0;
    pub const SET_ANCHOR: u16 = 1;
    pub const SET_EXCLUSIVE_ZONE: u16 = 2;
    pub const SET_MARGIN: u16 = 3;
    pub const SET_KEYBOARD_INTERACTIVITY: u16 = 4;
    pub const ACK_CONFIGURE: u16 = 6;
    pub const DESTROY: u16 = 7;
    pub const CONFIGURE: u16 = 0;
    pub const CLOSED: u16 = 1;
    pub const ANCHOR_TOP: u32 = 1;
    pub const ANCHOR_BOTTOM: u32 = 2;
    pub const ANCHOR_LEFT: u32 = 4;
    pub const ANCHOR_RIGHT: u32 = 8;
    pub const KEYBOARD_NONE: u32 = 0;
}
pub mod viewporter {
    pub const DESTROY: u16 = 0;
    pub const GET_VIEWPORT: u16 = 1;
}
pub mod viewport {
    pub const DESTROY: u16 = 0;
    pub const SET_DESTINATION: u16 = 2;
}
pub mod fractional_scale_manager {
    pub const GET_FRACTIONAL_SCALE: u16 = 1;
}
pub mod fractional_scale {
    pub const DESTROY: u16 = 0;
    pub const PREFERRED_SCALE: u16 = 0;
}
pub mod cursor_shape_manager {
    pub const GET_POINTER: u16 = 1;
}
pub mod cursor_shape_device {
    pub const DESTROY: u16 = 0;
    pub const SET_SHAPE: u16 = 1;
    pub const SHAPE_DEFAULT: u32 = 1;
    pub const SHAPE_POINTER: u32 = 4;
}
