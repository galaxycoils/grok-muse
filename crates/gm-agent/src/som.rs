use gm_core::error::{GmError, Result};
use gm_core::types::{AxElement, AxSnapshot};
use image::{ExtendedColorType, Rgb, RgbImage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SomMark {
    pub badge_id: u32,
    pub ax_token: String,
    pub role: String,
    pub label: Option<String>,
    pub center: (i32, i32),
}

#[derive(Debug, Clone)]
pub struct SomAnnotation {
    pub jpeg_bytes: Vec<u8>,
    pub marks: Vec<SomMark>,
    pub legend_text: String,
}

// Built-in 5x7 bitmap font for digits 0-9 and '#'
// Each character is 5 bits wide across 7 vertical rows
const FONT_5X7: [&[u8; 7]; 11] = [
    // 0
    &[
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    // 1
    &[
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    // 2
    &[
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    // 3
    &[
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    // 4
    &[
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    // 5
    &[
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    // 6
    &[
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    // 7
    &[
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    // 8
    &[
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    // 9
    &[
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ],
    // # (index 10)
    &[
        0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
    ],
];

const COLOR_PALETTE: [[u8; 3]; 8] = [
    [255, 59, 48],  // Red
    [52, 199, 89],  // Green
    [0, 122, 255],  // Blue
    [255, 204, 0],  // Yellow
    [175, 82, 222], // Purple
    [255, 149, 0],  // Orange
    [90, 200, 250], // Cyan
    [255, 45, 85],  // Pink
];

pub fn is_interactive_element(el: &AxElement) -> bool {
    if el.frame.width <= 0.0 || el.frame.height <= 0.0 {
        return false;
    }
    let r = el.role.to_lowercase();
    r.contains("button")
        || r.contains("textfield")
        || r.contains("text_field")
        || r.contains("textentry")
        || r.contains("link")
        || r.contains("checkbox")
        || r.contains("check_box")
        || r.contains("radiobutton")
        || r.contains("radio_button")
        || r.contains("menuitem")
        || r.contains("menu_item")
        || r.contains("popupbutton")
        || r.contains("popup_button")
        || r.contains("combobox")
        || r.contains("combo_box")
        || r.contains("slider")
        || r.contains("tab")
        || r.contains("cell")
        || r.contains("searchfield")
}

fn get_glyph_bitmap(ch: char) -> Option<&'static [u8; 7]> {
    match ch {
        '0'..='9' => {
            let idx = (ch as u8 - b'0') as usize;
            Some(FONT_5X7[idx])
        }
        '#' => Some(FONT_5X7[10]),
        _ => None,
    }
}

fn draw_badge_text(
    img: &mut RgbImage,
    text: &str,
    x_start: i32,
    y_start: i32,
    text_color: Rgb<u8>,
) {
    let mut cursor_x = x_start;
    for ch in text.chars() {
        if let Some(bitmap) = get_glyph_bitmap(ch) {
            for (row_idx, &row_bits) in bitmap.iter().enumerate() {
                let y = y_start + row_idx as i32;
                if y < 0 || y >= img.height() as i32 {
                    continue;
                }
                for col_idx in 0..5 {
                    if (row_bits & (1 << (4 - col_idx))) != 0 {
                        let x = cursor_x + col_idx;
                        if x >= 0 && x < img.width() as i32 {
                            img.put_pixel(x as u32, y as u32, text_color);
                        }
                    }
                }
            }
            cursor_x += 6; // 5 px glyph + 1 px spacing
        } else {
            cursor_x += 4;
        }
    }
}

/// Annotates a raw frame or generates a synthetic dark canvas frame with Set-of-Marks visual badges.
pub fn annotate_frame_with_som(
    frame_bytes: Option<&[u8]>,
    fallback_dims: (u32, u32),
    snapshot: &AxSnapshot,
) -> Result<SomAnnotation> {
    let (fw, fh) = fallback_dims;
    let def_w = if fw > 0 { fw } else { 1280 };
    let def_h = if fh > 0 { fh } else { 800 };

    let mut img = if let Some(bytes) = frame_bytes {
        match image::load_from_memory(bytes) {
            Ok(dyn_img) => dyn_img.to_rgb8(),
            Err(_) => RgbImage::from_pixel(def_w, def_h, Rgb([30, 30, 30])),
        }
    } else {
        RgbImage::from_pixel(def_w, def_h, Rgb([30, 30, 30]))
    };

    let img_w = img.width() as i32;
    let img_h = img.height() as i32;

    // Filter interactive elements with non-zero bounds
    let interactive_elements: Vec<&AxElement> = snapshot
        .elements
        .iter()
        .filter(|el| is_interactive_element(el))
        .collect();

    let mut marks = Vec::new();
    let mut legend_text = String::from("## Set-of-Marks UI Legend\n");

    for (idx, el) in interactive_elements.iter().enumerate() {
        let badge_id = (idx + 1) as u32;
        let color_rgb = COLOR_PALETTE[idx % COLOR_PALETTE.len()];
        let border_color = Rgb(color_rgb);

        let rx = el.frame.x as i32;
        let ry = el.frame.y as i32;
        let rw = (el.frame.width as i32).max(1);
        let rh = (el.frame.height as i32).max(1);

        let x1 = rx.clamp(0, img_w - 1);
        let y1 = ry.clamp(0, img_h - 1);
        let x2 = (rx + rw).clamp(0, img_w - 1);
        let y2 = (ry + rh).clamp(0, img_h - 1);

        // Draw crisp 2px colored bounding rectangle
        for t in 0..2 {
            let left = (x1 + t).min(x2);
            let right = (x2 - t).max(left);
            let top = (y1 + t).min(y2);
            let bottom = (y2 - t).max(top);

            for x in left..=right {
                if top < img_h {
                    img.put_pixel(x as u32, top as u32, border_color);
                }
                if bottom < img_h {
                    img.put_pixel(x as u32, bottom as u32, border_color);
                }
            }
            for y in top..=bottom {
                if left < img_w {
                    img.put_pixel(left as u32, y as u32, border_color);
                }
                if right < img_w {
                    img.put_pixel(right as u32, y as u32, border_color);
                }
            }
        }

        // Draw solid top-left numbered tag badge
        let badge_str = format!("{badge_id}");
        let text_chars_len = badge_str.len() as i32;
        let badge_box_w = (text_chars_len * 6) + 5;
        let badge_box_h = 11;

        // Position badge at top-left of the bounding box
        let badge_x = x1.clamp(0, (img_w - badge_box_w).max(0));
        let badge_y = if y1 >= badge_box_h {
            y1 - badge_box_h
        } else {
            y1
        };

        // Fill badge solid background (dark backdrop for contrast)
        let badge_bg = Rgb([15, 15, 15]);
        for by in badge_y..(badge_y + badge_box_h).min(img_h) {
            for bx in badge_x..(badge_x + badge_box_w).min(img_w) {
                img.put_pixel(bx as u32, by as u32, badge_bg);
            }
        }

        // Draw badge outline in color
        for bx in badge_x..(badge_x + badge_box_w).min(img_w) {
            if badge_y < img_h {
                img.put_pixel(bx as u32, badge_y as u32, border_color);
            }
            let by_bot = (badge_y + badge_box_h - 1).min(img_h - 1);
            img.put_pixel(bx as u32, by_bot as u32, border_color);
        }
        for by in badge_y..(badge_y + badge_box_h).min(img_h) {
            if badge_x < img_w {
                img.put_pixel(badge_x as u32, by as u32, border_color);
            }
            let bx_rt = (badge_x + badge_box_w - 1).min(img_w - 1);
            img.put_pixel(bx_rt as u32, by as u32, border_color);
        }

        // Draw digits in crisp white
        draw_badge_text(
            &mut img,
            &badge_str,
            badge_x + 3,
            badge_y + 2,
            Rgb([255, 255, 255]),
        );

        let center_x = (rx + rw / 2).clamp(0, img_w);
        let center_y = (ry + rh / 2).clamp(0, img_h);

        let mark = SomMark {
            badge_id,
            ax_token: el.token.clone(),
            role: el.role.clone(),
            label: el.label.clone(),
            center: (center_x, center_y),
        };

        let label_desc = mark.label.as_deref().unwrap_or("<unlabeled>");
        legend_text.push_str(&format!(
            "- #{}: [{}] \"{}\" (token: {}, center: ({}, {}))\n",
            mark.badge_id, mark.role, label_desc, mark.ax_token, mark.center.0, mark.center.1
        ));

        marks.push(mark);
    }

    // Encode to JPEG (quality 80)
    let mut jpeg_bytes = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, 80);
    encoder
        .encode(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|e| GmError::Platform(format!("Failed to encode SoM JPEG frame: {e}")))?;

    Ok(SomAnnotation {
        jpeg_bytes,
        marks,
        legend_text,
    })
}

/// Resolves a badge reference (e.g. `"#1"`, `"1"`, `"som_1"`, `"som#1"`) against the marks list.
pub fn resolve_som_badge<'a>(token: &str, marks: &'a [SomMark]) -> Option<&'a SomMark> {
    let clean = token
        .trim()
        .trim_start_matches("som_")
        .trim_start_matches("som#")
        .trim_start_matches("som")
        .trim_start_matches('#')
        .trim();

    if let Ok(id) = clean.parse::<u32>() {
        marks.iter().find(|m| m.badge_id == id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::types::Rect;

    #[test]
    fn test_annotate_frame_with_som_produces_decodable_jpeg() {
        let snapshot = AxSnapshot {
            window_id: 100,
            app_name: "Safari".into(),
            window_title: "GitHub".into(),
            bounds: Rect::new(0.0, 0.0, 800.0, 600.0),
            elements: vec![
                AxElement {
                    token: "btn_submit".into(),
                    role: "AXButton".into(),
                    label: Some("Submit Pull Request".into()),
                    value: None,
                    frame: Rect::new(50.0, 50.0, 120.0, 36.0),
                    is_enabled: true,
                    is_secure: false,
                },
                AxElement {
                    token: "txt_comment".into(),
                    role: "AXTextField".into(),
                    label: Some("Leave a comment".into()),
                    value: None,
                    frame: Rect::new(50.0, 110.0, 300.0, 80.0),
                    is_enabled: true,
                    is_secure: false,
                },
            ],
            timestamp_ms: 1000,
        };

        let som = annotate_frame_with_som(None, (800, 600), &snapshot).expect("annotation failed");
        assert_eq!(som.marks.len(), 2);
        assert_eq!(som.marks[0].badge_id, 1);
        assert_eq!(som.marks[0].ax_token, "btn_submit");
        assert_eq!(som.marks[1].badge_id, 2);
        assert_eq!(som.marks[1].ax_token, "txt_comment");

        // Verify that the output bytes are a valid JPEG
        let decoded = image::load_from_memory(&som.jpeg_bytes).expect("must decode valid JPEG");
        assert_eq!(decoded.width(), 800);
        assert_eq!(decoded.height(), 600);

        // Verify legend text contents
        assert!(
            som.legend_text
                .contains("- #1: [AXButton] \"Submit Pull Request\"")
        );
        assert!(
            som.legend_text
                .contains("- #2: [AXTextField] \"Leave a comment\"")
        );
    }

    #[test]
    fn test_resolve_som_badge_formats() {
        let marks = vec![
            SomMark {
                badge_id: 1,
                ax_token: "el_btn_1".into(),
                role: "AXButton".into(),
                label: Some("OK".into()),
                center: (100, 200),
            },
            SomMark {
                badge_id: 2,
                ax_token: "el_link_2".into(),
                role: "AXLink".into(),
                label: Some("Help".into()),
                center: (300, 400),
            },
        ];

        // All variations of badge 1
        assert_eq!(
            resolve_som_badge("#1", &marks).unwrap().ax_token,
            "el_btn_1"
        );
        assert_eq!(resolve_som_badge("1", &marks).unwrap().ax_token, "el_btn_1");
        assert_eq!(
            resolve_som_badge("som_1", &marks).unwrap().ax_token,
            "el_btn_1"
        );
        assert_eq!(
            resolve_som_badge("som#1", &marks).unwrap().ax_token,
            "el_btn_1"
        );

        // Badge 2
        assert_eq!(
            resolve_som_badge("#2", &marks).unwrap().ax_token,
            "el_link_2"
        );
        assert_eq!(
            resolve_som_badge("som_2", &marks).unwrap().ax_token,
            "el_link_2"
        );

        // Non-existent badge
        assert!(resolve_som_badge("#99", &marks).is_none());
        assert!(resolve_som_badge("invalid_token", &marks).is_none());
    }
}
