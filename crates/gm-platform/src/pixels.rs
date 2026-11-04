use gm_core::{
    error::{GmError, Result},
    types::{FrameCapture, FrameData},
};
pub(crate) fn jpeg(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    edge: Option<u32>,
    quality: Option<u8>,
) -> Result<FrameCapture> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 64_000_000 {
        return Err(GmError::Platform(
            "Invalid or excessive capture dimensions".into(),
        ));
    }
    let image = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| GmError::Platform("Invalid screen pixel buffer".into()))?;
    let mut image = image::DynamicImage::ImageRgba8(image).to_rgb8();
    let maximum = edge.unwrap_or(1920).clamp(64, 4096);
    if width.max(height) > maximum {
        let scale = f64::from(maximum) / f64::from(width.max(height));
        image = image::imageops::resize(
            &image,
            (f64::from(width) * scale).round().max(1.0) as u32,
            (f64::from(height) * scale).round().max(1.0) as u32,
            image::imageops::FilterType::Triangle,
        );
    }
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(
        &mut bytes,
        quality.unwrap_or(70).clamp(1, 100),
    )
    .encode_image(&image)
    .map_err(|e| GmError::Platform(e.to_string()))?;
    Ok(FrameCapture {
        width: image.width(),
        height: image.height(),
        scale_factor: f64::from(width) / f64::from(image.width()),
        desktop_bounds: Some(gm_core::types::Rect::new(
            0.0,
            0.0,
            f64::from(width),
            f64::from(height),
        )),
        data: FrameData::InlineBytes(bytes),
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    })
}
