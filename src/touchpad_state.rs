use std::sync::atomic::{AtomicI32, AtomicBool, Ordering};

pub struct TouchpadState {
    pub x: AtomicI32,
    pub touching: AtomicBool,
}

impl TouchpadState {
    pub fn new() -> Self {
        Self {
            x: AtomicI32::new(0),
            touching: AtomicBool::new(false),
        }
    }
}
