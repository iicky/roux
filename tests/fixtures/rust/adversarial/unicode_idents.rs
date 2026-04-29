//! Non-ASCII identifiers — valid Rust per RFC 2457.

pub fn αβγ() -> i32 {
    42
}

pub struct Δelta {
    pub π: f64,
}

pub fn 日本語_function() -> &'static str {
    "hello"
}
