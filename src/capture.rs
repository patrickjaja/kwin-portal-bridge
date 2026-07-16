use std::io::Cursor;

use crate::kwin::KWinBackend;
use crate::model::{ScreenInfo, ScreenshotCapture, ScreenshotResult};
use crate::portal::PortalBackend;
use anyhow::{Context, Result, bail};
use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{ImageBuffer, RgbImage, Rgba};
use lamco_pipewire::{FrameBuffer, PixelFormat, VideoFrame};

pub struct CaptureBackend;

const SCREENSHOT_JPEG_QUALITY: u8 = 75;
const MAX_LONG_EDGE: u32 = 1568;
const MAX_PIXELS: u32 = 1_150_000;
// const MAX_LONG_EDGE: u32 = 2576;
// const MAX_PIXELS: u32 = 3_500_000;

impl CaptureBackend {
    pub fn new() -> Self {
        Self
    }

    pub async fn capture_still_frame(
        &self,
        display: Option<&str>,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<ScreenshotResult> {
        let screens = kwin.list_screens()?;
        let screen = resolve_screen(&screens, display)?;
        portal.capture_still_image(screen).await
    }

    pub async fn capture_zoom(
        &self,
        display: Option<&str>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        portal: &PortalBackend,
        kwin: &KWinBackend,
    ) -> Result<ScreenshotCapture> {
        if w <= 0 || h <= 0 {
            bail!("zoom region must have positive width and height");
        }

        let screens = kwin.list_screens()?;
        let screen = resolve_screen(&screens, display)?;
        portal.capture_zoom_image(screen, x, y, w, h).await
    }
}

pub(crate) fn screenshot_result_from_frame(
    screen: &ScreenInfo,
    frame: &VideoFrame,
) -> Result<ScreenshotResult> {
    let rgb = rgb_from_frame(frame)?;
    let encoded = encode_resized_jpeg_base64(
        &rgb,
        compute_target_dims(
            screen.geometry.width as u32,
            screen.geometry.height as u32,
            screen.scale.unwrap_or(1.0),
        ),
    )?;

    Ok(ScreenshotResult {
        base64: encoded.base64,
        width: encoded.width,
        height: encoded.height,
        display_width: screen.geometry.width as u32,
        display_height: screen.geometry.height as u32,
        display_id: screen.id.clone(),
        origin_x: screen.geometry.x,
        origin_y: screen.geometry.y,
    })
}

#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub(crate) fn png_base64_from_frame(frame: &VideoFrame) -> Result<String> {
    let rgba = rgba_from_frame(frame)?;
    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(frame.width, frame.height, rgba)
        .ok_or_else(|| anyhow::anyhow!("failed to construct RGBA image buffer"))?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .context("failed to encode PNG from captured frame")?;
    Ok(base64::engine::general_purpose::STANDARD.encode(png))
}

pub(crate) fn zoom_result_from_frame(
    screen: &ScreenInfo,
    frame: &VideoFrame,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<ScreenshotCapture> {
    let rgb = rgb_from_frame(frame)?;
    let cropped = crop_logical_region(&rgb, screen, x, y, w, h)?;

    encode_resized_jpeg_base64(
        &cropped,
        compute_target_dims(w as u32, h as u32, screen.scale.unwrap_or(1.0)),
    )
}

fn rgba_from_frame(frame: &VideoFrame) -> Result<Vec<u8>> {
    let bytes = match &frame.buffer {
        FrameBuffer::Memory(data) => data.as_ref(),
        FrameBuffer::DmaBuf(_) => bail!("cannot save PNG directly from DMA-BUF frame"),
    };

    let width = frame.width as usize;
    let height = frame.height as usize;
    let stride = frame.stride as usize;
    let mut rgba = vec![0u8; width * height * 4];

    for y in 0..height {
        let src_row = &bytes[y * stride..y * stride + width * 4];
        let dst_row = &mut rgba[y * width * 4..(y + 1) * width * 4];

        for x in 0..width {
            let s = x * 4;
            let d = x * 4;
            match frame.format {
                PixelFormat::BGRx => {
                    dst_row[d] = src_row[s + 2];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s];
                    dst_row[d + 3] = 255;
                }
                PixelFormat::BGRA => {
                    dst_row[d] = src_row[s + 2];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s];
                    dst_row[d + 3] = src_row[s + 3];
                }
                PixelFormat::RGBx => {
                    dst_row[d] = src_row[s];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s + 2];
                    dst_row[d + 3] = 255;
                }
                PixelFormat::RGBA => {
                    dst_row[d] = src_row[s];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s + 2];
                    dst_row[d + 3] = src_row[s + 3];
                }
                _ => bail!(
                    "unsupported pixel format for PNG export: {:?}",
                    frame.format
                ),
            }
        }
    }

    Ok(rgba)
}

fn rgb_from_frame(frame: &VideoFrame) -> Result<RgbImage> {
    let bytes = match &frame.buffer {
        FrameBuffer::Memory(data) => data.as_ref(),
        FrameBuffer::DmaBuf(_) => bail!("cannot convert DMA-BUF frame without import support"),
    };

    let width = frame.width as usize;
    let height = frame.height as usize;
    let stride = frame.stride as usize;
    let mut rgb = vec![0u8; width * height * 3];

    for y in 0..height {
        let src_row = &bytes[y * stride..y * stride + width * 4];
        let dst_row = &mut rgb[y * width * 3..(y + 1) * width * 3];

        for x in 0..width {
            let s = x * 4;
            let d = x * 3;
            match frame.format {
                PixelFormat::BGRx | PixelFormat::BGRA => {
                    dst_row[d] = src_row[s + 2];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s];
                }
                PixelFormat::RGBx | PixelFormat::RGBA => {
                    dst_row[d] = src_row[s];
                    dst_row[d + 1] = src_row[s + 1];
                    dst_row[d + 2] = src_row[s + 2];
                }
                _ => bail!(
                    "unsupported pixel format for screenshot export: {:?}",
                    frame.format
                ),
            }
        }
    }

    RgbImage::from_raw(frame.width, frame.height, rgb)
        .ok_or_else(|| anyhow::anyhow!("failed to construct RGB image buffer"))
}

fn crop_logical_region(
    rgb: &RgbImage,
    screen: &ScreenInfo,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<RgbImage> {
    if x < 0 || y < 0 {
        bail!("zoom region must start within the display");
    }

    if x + w > screen.geometry.width || y + h > screen.geometry.height {
        bail!(
            "zoom region {x},{y} {}x{} exceeds display `{}` logical bounds {}x{}",
            w,
            h,
            screen.id,
            screen.geometry.width,
            screen.geometry.height,
        );
    }

    let scale_x = f64::from(rgb.width()) / f64::from(screen.geometry.width.max(1));
    let scale_y = f64::from(rgb.height()) / f64::from(screen.geometry.height.max(1));

    let crop_x = ((x as f64) * scale_x).round() as u32;
    let crop_y = ((y as f64) * scale_y).round() as u32;
    let crop_w = ((w as f64) * scale_x).round().max(1.0) as u32;
    let crop_h = ((h as f64) * scale_y).round().max(1.0) as u32;

    let max_w = rgb.width().saturating_sub(crop_x);
    let max_h = rgb.height().saturating_sub(crop_y);
    let clamped_w = crop_w.min(max_w).max(1);
    let clamped_h = crop_h.min(max_h).max(1);

    Ok(image::imageops::crop_imm(rgb, crop_x, crop_y, clamped_w, clamped_h).to_image())
}

fn encode_resized_jpeg_base64(rgb: &RgbImage, target: (u32, u32)) -> Result<ScreenshotCapture> {
    let mut filter_type = FilterType::CatmullRom;
    if cfg!(debug_assertions) {
        filter_type = FilterType::Nearest;
    }
    let resized = image::imageops::resize(rgb, target.0, target.1, filter_type);

    let mut jpeg = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut jpeg, SCREENSHOT_JPEG_QUALITY);
    encoder
        .encode_image(&resized)
        .context("failed to JPEG-encode screenshot")?;
    // save jpeg to file
    // let file_path = format!("/tmp/screenshot_{}.jpg", Uuid::new_v4());
    // std::fs::write(&file_path, &jpeg).context("failed to save screenshot to file")?;

    Ok(ScreenshotCapture {
        base64: base64::engine::general_purpose::STANDARD.encode(jpeg),
        width: resized.width(),
        height: resized.height(),
    })
}

pub(crate) fn resolve_screen<'a>(
    screens: &'a [ScreenInfo],
    selector: Option<&str>,
) -> Result<&'a ScreenInfo> {
    if screens.is_empty() {
        bail!("no screens reported by KWin");
    }

    if let Some(selector) = selector {
        if let Some(screen) = screens
            .iter()
            .find(|screen| screen.id == selector || screen.name == selector)
        {
            return Ok(screen);
        }

        bail!("display `{selector}` not found");
    }

    screens
        .iter()
        .find(|screen| screen.is_active)
        .or_else(|| screens.iter().find(|screen| screen.is_primary))
        .or_else(|| screens.first())
        .ok_or_else(|| anyhow::anyhow!("no screen available"))
}

fn compute_target_dims(logical_w: u32, logical_h: u32, scale_factor: f64) -> (u32, u32) {
    let safe_scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };

    let phys_w = ((logical_w as f64) * safe_scale).round().max(1.0);
    let phys_h = ((logical_h as f64) * safe_scale).round().max(1.0);
    let long_edge_scale = (MAX_LONG_EDGE as f64) / phys_w.max(phys_h);
    let pixel_scale = ((MAX_PIXELS as f64) / (phys_w * phys_h)).sqrt();
    let scale = 1.0_f64.min(long_edge_scale).min(pixel_scale);

    (
        (phys_w * scale).round().max(1.0) as u32,
        (phys_h * scale).round().max(1.0) as u32,
    )
}
