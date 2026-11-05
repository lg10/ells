pub mod emu;
pub mod encode;

pub use emu::Emulator;
pub use encode::{key_to_bytes, mouse_to_bytes, MouseAction, MouseButton};
pub use vt100;
