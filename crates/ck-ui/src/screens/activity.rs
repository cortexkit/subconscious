//! The Activity tab. Until its screen is built it draws only the tab bar and a
//! note; the live app doesn't serve the tab, so only the design fixture
//! reaches it.

use crate::draw::Canvas;
use crate::model::Tab;

use super::{unbuilt, View};

pub fn draw(c: &mut Canvas, v: &View) {
    unbuilt(c, v, Tab::Activity);
}
