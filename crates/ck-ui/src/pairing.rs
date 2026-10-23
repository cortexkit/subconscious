//! The phone pairing code and its terminal layout.

use qrcodegen::{DataTooLong, QrCode, QrCodeEcc};

use crate::time::Timestamp;

pub mod flow;

pub const QUIET: i32 = 2;
pub const STEPS_W: i32 = 48;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub device_name: String,
    pub source_addr: String,
    pub fingerprint: String,
}

/// A code returned by the service and the words kept while the window resizes.
/// The matrix is always encoded here, never supplied by a fixture or caller.
#[derive(Clone)]
pub struct PairingState {
    code: QrCode,
    /// The window callosum opened for this code; empty for a code drawn
    /// without one.
    pub window_id: String,
    /// The access group the window was opened with. A refreshed list never
    /// changes it.
    pub group: String,
    pub operation_count: Option<u64>,
    pub opened_at: Timestamp,
    pub expires_at: Timestamp,
    pub claim: Option<Claim>,
    pub foreign_attempt: Option<String>,
    pub words: [String; 4],
    pub word_sel: usize,
}

// Written by hand so that neither the payload, which carries the one-time
// pairing secret, nor the words the operator types can reach a log.
impl std::fmt::Debug for PairingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingState")
            .field("symbol_size", &self.code.size())
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl PairingState {
    pub fn new(
        payload: &str,
        group: String,
        operation_count: Option<u64>,
        opened_at: Timestamp,
        expires_at: Timestamp,
    ) -> Result<Self, DataTooLong> {
        Ok(Self {
            code: encode(payload)?,
            window_id: String::new(),
            group,
            operation_count,
            opened_at,
            expires_at,
            claim: None,
            foreign_attempt: None,
            words: std::array::from_fn(|_| String::new()),
            word_sel: 0,
        })
    }

    pub fn code(&self) -> &QrCode {
        &self.code
    }

    pub fn minutes(&self) -> i64 {
        ((self.expires_at.unix_millis() - self.opened_at.unix_millis() + 30_000) / 60_000).max(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub code_x: i32,
    pub content_y: i32,
    pub steps_x: i32,
}

/// The modules across a version 6 symbol, the size of a typical pairing
/// code. The overlay keeps this much room for the code until a code arrives.
pub const TYPICAL_MODULES: i32 = 41;

/// The minimum excludes vertical padding, but always reserves the four chrome rows.
pub fn minimum(code: &QrCode, steps_height: i32) -> (i32, i32) {
    minimum_for(code.size(), steps_height)
}

/// [`minimum`] for a symbol `modules` across.
pub fn minimum_for(modules: i32, steps_height: i32) -> (i32, i32) {
    let symbol = modules + 2 * QUIET;
    (symbol + 59, ((symbol + 1) / 2).max(steps_height) + 6)
}

pub fn layout(w: i32, h: i32, code: &QrCode, steps_height: i32) -> Option<Layout> {
    layout_for(w, h, code.size(), steps_height)
}

/// [`layout`] for a symbol `modules` across.
pub fn layout_for(w: i32, h: i32, modules: i32, steps_height: i32) -> Option<Layout> {
    let (width, min_h) = minimum_for(modules, steps_height);
    if w < width || h < min_h {
        return None;
    }
    let content = min_h - 6;
    for (y, top, bottom) in [(3, 1, 2), (2, 1, 2), (2, 1, 0), (2, 0, 0)] {
        let height = content + 2 + top + bottom;
        if y + height <= h - 2 {
            let x = (w - width) / 2;
            return Some(Layout {
                x,
                y,
                width,
                height,
                code_x: x + 3,
                content_y: y + 1 + top,
                steps_x: x + 3 + modules + 2 * QUIET + 4,
            });
        }
    }
    None
}

/// Encode the pairing-service payload with low error correction and automatic ECC boost.
pub fn encode(payload: &str) -> Result<QrCode, DataTooLong> {
    QrCode::encode_text(payload, QrCodeEcc::Low)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_fixture_cell_for_cell() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/fixture.json")).unwrap();
        let pairing = &fixture["pairing"];
        let code = encode(pairing["payload"].as_str().unwrap()).unwrap();
        let rows = pairing["qr"].as_array().unwrap();
        assert_eq!(code.size() as usize, rows.len());
        for (y, row) in rows.iter().enumerate() {
            let row = row.as_str().unwrap();
            assert_eq!(row.len(), rows.len());
            for (x, cell) in row.bytes().enumerate() {
                assert_eq!(code.get_module(x as i32, y as i32), cell == b'1', "{x},{y}");
            }
        }
    }
}
