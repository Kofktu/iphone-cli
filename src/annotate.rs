//! `iph screenshot --annotate`: the screenshot with the elements of the same
//! reading drawn on it, each numbered, and a legend that ties each number to
//! the element's id, role and label.

use serde_json::{Value, json};

use crate::{
    ax::tree::{Element, Screen},
    geometry::Geometry,
    png::Image,
};

/// Roles a user acts on.
const INTERACTIVE: [&str; 9] = [
    "button",
    "switch",
    "link",
    "text_field",
    "search_field",
    "secure_text_field",
    "key",
    "tab",
    "adjustable",
];

/// 5x7 digits, one row per byte, the high bit of five on the left.
const DIGITS: [[u8; 7]; 10] = [
    [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
    [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
    [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
    [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
    [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
    [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
    [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
    [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
    [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
    [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
];

/// Neighbours get different colours, all dark enough for white digits.
const PALETTE: [[u8; 3]; 6] = [[230, 25, 75], [0, 130, 200], [60, 160, 60], [245, 130, 48], [145, 30, 180], [0, 128, 128]];

/// The elements to draw, in reading order: top to bottom in rows of 8
/// points, then left to right. By default the on-screen elements a user can
/// act on; with `all` every on-screen element. Either way only those
/// covering under half the screen, so windows and backgrounds do not bury
/// the rest.
pub fn pick(screen: &Screen, all: bool) -> Vec<&Element> {
    let screen_area = screen.width * screen.height;
    let mut picked: Vec<&Element> = screen
        .elements
        .iter()
        .filter(|e| e.on_screen)
        .filter(|e| e.frame.is_some_and(|f| f.w > 1.0 && f.h > 1.0 && f.w * f.h < 0.5 * screen_area))
        .filter(|e| all || INTERACTIVE.contains(&e.role.as_str()) || e.traits.iter().any(|t| t == "button"))
        .collect();
    let row = |e: &Element| e.frame.map_or((0, 0), |f| ((f.y / 8.0).round() as i64, f.x.round() as i64));
    picked.sort_by_key(|e| row(e));
    picked
}

/// What `draw` takes from each element of `pick`: which one it is and where.
pub fn boxes(screen: &Screen, all: bool) -> Vec<(&str, &str, Option<[i64; 4]>)> {
    pick(screen, all)
        .into_iter()
        .map(|e| {
            let frame = e.frame.map(|f| [f.x, f.y, f.w, f.h].map(|v| v.round() as i64));
            (e.id.as_str(), e.role.as_str(), frame)
        })
        .collect()
}

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Px {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// Width and height of the badge for number `n`, one digit cell being
/// `unit` pixels per font dot.
pub fn badge_size(n: usize, unit: i64) -> (i64, i64) {
    let digits = n.to_string().len() as i64;
    (digits * 6 * unit + unit, 7 * unit + 2 * unit)
}

/// Where the badge goes: inside the frame's top-left corner when the frame
/// is big enough to keep most of itself visible, otherwise just outside it,
/// above or else below, so a small control's own look is not covered.
/// Always inside the canvas.
pub fn place_badge(frame: Px, badge: (i64, i64), canvas: (i64, i64)) -> (i64, i64) {
    let (bw, bh) = badge;
    let small = frame.w < 2 * bw || frame.h < 2 * bh;
    let (x, y) = if !small {
        (frame.x, frame.y)
    } else if frame.y - bh >= 0 {
        (frame.x, frame.y - bh)
    } else if frame.y + frame.h + bh <= canvas.1 {
        (frame.x, frame.y + frame.h)
    } else {
        (frame.x, frame.y)
    };
    (x.clamp(0, (canvas.0 - bw).max(0)), y.clamp(0, (canvas.1 - bh).max(0)))
}

fn stroke(image: &mut Image, r: Px, t: i64, rgb: [u8; 3]) {
    let (x1, y1) = (r.x + r.w, r.y + r.h);
    image.fill(r.x, r.y, x1, r.y + t, rgb, 255);
    image.fill(r.x, y1 - t, x1, y1, rgb, 255);
    image.fill(r.x, r.y, r.x + t, y1, rgb, 255);
    image.fill(x1 - t, r.y, x1, y1, rgb, 255);
}

fn badge(image: &mut Image, (x, y): (i64, i64), n: usize, unit: i64, rgb: [u8; 3]) {
    let (bw, bh) = badge_size(n, unit);
    image.fill(x, y, x + bw, y + bh, rgb, 255);
    for (i, ch) in n.to_string().bytes().enumerate() {
        for (row, bits) in DIGITS[usize::from(ch - b'0')].iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 == 1 {
                    let px = x + unit + i as i64 * 6 * unit + col * unit;
                    let py = y + unit + row as i64 * unit;
                    image.fill(px, py, px + unit, py + unit, [255, 255, 255], 255);
                }
            }
        }
    }
}

/// Draw the elements on the image, numbered from 1, and return the legend.
pub fn draw(image: &mut Image, geometry: &Geometry, elements: &[&Element]) -> Vec<Value> {
    let unit = (2.0 * geometry.scale).round().max(1.0) as i64;
    let line = geometry.scale.round().max(1.0) as i64;
    let canvas = (i64::from(image.width), i64::from(image.height));
    let mut legend = Vec::with_capacity(elements.len());
    for (i, e) in elements.iter().enumerate() {
        let Some(f) = e.frame else { continue };
        let n = i + 1;
        let r = Px { x: geometry.px(f.x), y: geometry.px(f.y), w: geometry.px(f.w), h: geometry.px(f.h) };
        let rgb = PALETTE[i % PALETTE.len()];
        image.fill(r.x, r.y, r.x + r.w, r.y + r.h, rgb, 40);
        stroke(image, r, line, rgb);
        badge(image, place_badge(r, badge_size(n, unit), canvas), n, unit, rgb);
        legend.push(json!({ "n": n, "id": e.id, "role": e.role, "label": e.label, "frame": f }));
    }
    legend
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(id: &str, role: &str, traits: &[&str], frame: (f64, f64, f64, f64), on_screen: bool) -> Value {
        json!({
            "id": id, "role": role, "traits": traits, "on_screen": on_screen,
            "frame": {"x": frame.0, "y": frame.1, "w": frame.2, "h": frame.3},
        })
    }

    fn screen(elements: Vec<Value>) -> Screen {
        serde_json::from_value(json!({ "width": 400.0, "height": 800.0, "apps": [], "elements": elements })).unwrap()
    }

    #[test]
    fn the_default_set_is_small_on_screen_things_a_user_can_act_on_in_reading_order() {
        let s = screen(vec![
            element("app", "application", &[], (0.0, 0.0, 400.0, 800.0), true),
            element("row2", "text", &["button"], (0.0, 200.0, 400.0, 44.0), true),
            element("sw", "switch", &[], (330.0, 101.0, 50.0, 30.0), true),
            element("title", "header", &[], (20.0, 60.0, 200.0, 30.0), true),
            element("back", "button", &["button", "back_button"], (8.0, 99.0, 60.0, 30.0), true),
            element("gone", "button", &["button"], (0.0, 900.0, 400.0, 44.0), false),
            element("line", "button", &["button"], (0.0, 300.0, 400.0, 0.5), true),
            element("panel", "other", &[], (0.0, 400.0, 400.0, 420.0), true),
        ]);
        let ids = |v: Vec<&Element>| v.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(pick(&s, false)), ["back", "sw", "row2"]);
        assert_eq!(ids(pick(&s, true)), ["title", "back", "sw", "row2"]);
    }

    #[test]
    fn badges_go_outside_small_frames_and_stay_on_the_canvas() {
        let canvas = (1200, 2400);
        let badge = badge_size(7, 6);
        assert_eq!(badge, (42, 54));
        // A row: inside its corner.
        assert_eq!(place_badge(Px { x: 0, y: 600, w: 1200, h: 132 }, badge, canvas), (0, 600));
        // A switch: above it.
        assert_eq!(place_badge(Px { x: 990, y: 300, w: 150, h: 90 }, badge, canvas), (990, 246));
        // At the top edge: below it.
        assert_eq!(place_badge(Px { x: 990, y: 10, w: 150, h: 90 }, badge, canvas), (990, 100));
        // At the right edge: pulled back in.
        assert_eq!(place_badge(Px { x: 1190, y: 300, w: 10, h: 10 }, badge, canvas), (1158, 246));
        assert_eq!(badge_size(12, 6).0, 2 * 36 + 6);
    }

    #[test]
    fn a_ticking_value_or_label_changes_no_box_but_a_move_does() {
        let at = |label: &str, value: &str, y: f64| {
            let mut e = element("b", "button", &[], (10.0, y, 80.0, 40.0), true);
            e["label"] = json!(label);
            e["value"] = json!(value);
            screen(vec![e, element("t", "text", &[], (10.0, 200.0, 120.0, 30.0), true)])
        };
        let before = at("00:01.27", "1", 100.0);
        for all in [false, true] {
            assert_eq!(boxes(&before, all), boxes(&at("00:01.31", "2", 100.0), all));
            assert_ne!(boxes(&before, all), boxes(&at("00:01.27", "1", 104.0), all));
        }
    }

    #[test]
    fn each_drawn_element_has_a_legend_entry_and_its_frame_on_the_image() {
        let s = screen(vec![
            element("a", "button", &["button"], (10.0, 10.0, 100.0, 50.0), true),
            element("b", "switch", &[], (200.0, 300.0, 50.0, 30.0), true),
        ]);
        let mut image = Image::new(800, 1600, vec![255; 800 * 1600 * 3]);
        let g = Geometry::fit((400.0, 800.0), (800, 1600)).unwrap();
        let legend = draw(&mut image, &g, &pick(&s, false));
        assert_eq!(legend.len(), 2);
        assert_eq!(legend[1]["n"], 2);
        assert_eq!(legend[1]["id"], "b");
        assert_eq!(legend[1]["frame"]["x"], 200.0);
        assert_eq!(image.pixel(20, 20), PALETTE[0], "the first badge sits in a's corner");
        assert_eq!(image.pixel(400, 630), PALETTE[1], "b's left edge, in pixels");
        assert_eq!(image.pixel(799, 1599), [255, 255, 255]);
    }
}
