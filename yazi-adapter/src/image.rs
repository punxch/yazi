use std::io::Cursor;
use std::path::Path;

use anyhow::Result;
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageError, ImageReader, ImageResult, Limits, codecs::{jpeg::JpegEncoder, png::PngEncoder}, imageops::FilterType, metadata::Orientation, ImageFormat, AnimationDecoder, Frame};
use ratatui::layout::Rect;
use yazi_config::YAZI;
use yazi_fs::provider::{Provider, local::Local};

use crate::Dimension;

pub struct Image;

impl Image {
	pub async fn precache(src: &Path, cache: &Path) -> Result<()> {
		let frames = Self::decode_from(src).await?;
		let (mut img, orientation, icc) = frames
			.into_iter()
			.next()
			.ok_or_else(|| anyhow::anyhow!("no image frames"))?;
        let (w, h) = Self::flip_size(orientation, (YAZI.preview.max_width, YAZI.preview.max_height));
 
        let buf = tokio::task::spawn_blocking(move || {
			if img.width() > w || img.height() > h {
				img = img.resize(w, h, Self::filter());
			}
			if orientation != Orientation::NoTransforms {
				img.apply_orientation(orientation);
			}

			let mut buf = Vec::new();
			if img.color().has_alpha() {
				let rgba = img.into_rgba8();
				let mut encoder = PngEncoder::new(&mut buf);
				icc.map(|b| encoder.set_icc_profile(b));
				encoder.write_image(&rgba, rgba.width(), rgba.height(), ExtendedColorType::Rgba8)?;
			} else {
				let mut encoder = JpegEncoder::new_with_quality(&mut buf, YAZI.preview.image_quality);
				icc.map(|b| encoder.set_icc_profile(b));
				encoder.encode_image(&img.into_rgb8())?;
			}

			Ok::<_, ImageError>(buf)
		})
		.await??;

		Ok(Local.write(cache, buf).await?)
	}

	pub(super) async fn downscale(path: &Path, rect: Rect) -> Result<DynamicImage> {
		let frames = Self::decode_from(path).await?;
		let (mut img, orientation, _) = frames
			.into_iter()
			.next()
			.ok_or_else(|| anyhow::anyhow!("no image frames"))?;
         let (w, h) = Self::flip_size(orientation, Self::max_pixel(rect));

		// Fast path.
		if img.width() <= w && img.height() <= h && orientation == Orientation::NoTransforms {
			return Ok(img);
		}

		let img = tokio::task::spawn_blocking(move || {
			if img.width() > w || img.height() > h {
				img = img.resize(w, h, Self::filter())
			}
			if orientation != Orientation::NoTransforms {
				img.apply_orientation(orientation);
			}
			img
		})
		.await?;

		Ok(img)
	}

	pub(super) fn max_pixel(rect: Rect) -> (u32, u32) {
		Dimension::cell_size()
			.map(|(cw, ch)| {
				let (w, h) = ((rect.width as f64 * cw) as u32, (rect.height as f64 * ch) as u32);
				(w.min(YAZI.preview.max_width), h.min(YAZI.preview.max_height))
			})
			.unwrap_or((YAZI.preview.max_width, YAZI.preview.max_height))
	}

	pub(super) fn pixel_area(size: (u32, u32), rect: Rect) -> Rect {
		Dimension::cell_size()
			.map(|(cw, ch)| Rect {
				x:      rect.x,
				y:      rect.y,
				width:  (size.0 as f64 / cw).ceil() as u16,
				height: (size.1 as f64 / ch).ceil() as u16,
			})
			.unwrap_or(rect)
	}

	fn filter() -> FilterType {
		match YAZI.preview.image_filter.as_str() {
			"nearest" => FilterType::Nearest,
			"triangle" => FilterType::Triangle,
			"catmull-rom" => FilterType::CatmullRom,
			"gaussian" => FilterType::Gaussian,
			"lanczos3" => FilterType::Lanczos3,
			_ => FilterType::Triangle,
		}
	}

	async fn decode_from(path: &Path) -> ImageResult<Vec<(DynamicImage, Orientation, Option<Vec<u8>>)>> {
        let mut limits = Limits::no_limits();
        if YAZI.tasks.image_alloc > 0 {
            limits.max_alloc = Some(YAZI.tasks.image_alloc as u64);
        }
        if YAZI.tasks.image_bound[0] > 0 {
            limits.max_image_width = Some(YAZI.tasks.image_bound[0] as u32);
        }
        if YAZI.tasks.image_bound[1] > 0 {
            limits.max_image_height = Some(YAZI.tasks.image_bound[1] as u32);
        }
    
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let data = std::fs::read(&path)?;
            let guessed = image::guess_format(&data).ok();
    
            let is_avif = |buf: &[u8]| -> bool {
                if buf.len() >= 12 && &buf[4..8] == b"ftyp" {
                    let brand = &buf[8..12];
                    brand == b"avif" || brand == b"avis" || brand == b"mif1" || brand == b"msf1"
                } else {
                    false
                }
            };
    
            let fmt_used = guessed.or_else(|| if is_avif(&data) { Some(ImageFormat::Avif) } else { None });
    
            use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
    
            if matches!(fmt_used, Some(ImageFormat::Gif)) {
                let mut dec = GifDecoder::new(Cursor::new(&data))?;
                dec.set_limits(limits.clone());
                let orientation = Orientation::NoTransforms; // GIF doesn't support EXIF orientation
                let icc = dec.icc_profile().ok().flatten();
                let mut out = Vec::new();
                for f in dec.into_frames() {
                    let frame = f?;
                    out.push((
                        DynamicImage::ImageRgba8(frame.buffer().to_owned()),
                        orientation,
                        icc.clone(),
                    ));
                }
                return Ok(out);
            }
    
            if matches!(fmt_used, Some(ImageFormat::Png)) {
                if let Ok(mut dec) = PngDecoder::new(Cursor::new(&data)) {
                    // Get ICC profile before moving dec into APNG decoder
                    let icc = dec.icc_profile().ok().flatten();
                    dec.set_limits(limits.clone());
                    if let Ok(apng) = dec.apng() {
                        // apng.set_limits(limits);
                        let orientation = Orientation::NoTransforms; // APNG doesn't support EXIF orientation
                        let mut out = Vec::new();
                        for f in apng.into_frames() {
                            let frame = f?;
                            out.push((
                                DynamicImage::ImageRgba8(frame.buffer().to_owned()),
                                orientation,
                                icc.clone(),
                            ));
                        }
                        return Ok(out);
                    }
                }
            }
    
            if matches!(fmt_used, Some(ImageFormat::WebP)) {
                if let Ok(mut dec) = WebPDecoder::new(Cursor::new(&data)) {
                    dec.set_limits(limits.clone());
                    let orientation = Orientation::NoTransforms; // WebP animation doesn't support EXIF orientation
                    let icc = dec.icc_profile().ok().flatten();
                    let mut out = Vec::new();
                    for f in dec.into_frames() {
                        let frame = f?;
                        out.push((
                            DynamicImage::ImageRgba8(frame.buffer().to_owned()),
                            orientation,
                            icc.clone(),
                        ));
                    }
                    return Ok(out);
                }
            }
    
            // Fallback: use ImageReader for non-animated formats
            let mut reader = if let Some(fmt) = fmt_used {
                ImageReader::with_format(Cursor::new(&data), fmt)
            } else {
                ImageReader::new(Cursor::new(&data)).with_guessed_format()?
            };
            reader.limits(limits);
    
            let mut decoder = reader.into_decoder()?;
            let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
            let icc = decoder.icc_profile().ok().flatten();
    
            let img = DynamicImage::from_decoder(decoder)?;
            Ok(vec![(img, orientation, icc)])
        })
        .await
        .map_err(|e| ImageError::IoError(e.into()))?
    }

	fn flip_size(orientation: Orientation, (w, h): (u32, u32)) -> (u32, u32) {
		use image::metadata::Orientation::{Rotate90, Rotate90FlipH, Rotate270, Rotate270FlipH};
		match orientation {
			Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH => (h, w),
			_ => (w, h),
		}
	}
}
