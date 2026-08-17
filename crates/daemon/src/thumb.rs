//! Pure thumbnailing: decode an image and produce an aspect-preserving JPEG
//! bounded by the requested size. No HTTP, no DB — unit-tested in isolation.

/// Decode `bytes`, produce an aspect-preserving thumbnail bounded by
/// `size`x`size` (the longer edge becomes `size`, the shorter edge scales
/// proportionally), and encode it as JPEG (quality 85).
///
/// The thumbnail keeps the source aspect ratio rather than cover-cropping to a
/// square so the client can choose its own fit: `object-fit: contain` letterboxes
/// the whole image (Frame) and `object-fit: cover` crops it (Fill). Cropping
/// server-side would bake that choice in and make the client toggle a no-op.
///
/// Triangle (bilinear) rather than Lanczos3: at thumbnail scale the two are
/// visually indistinguishable, and Triangle resizes several times faster —
/// generation latency is what gates gallery scrolling (#51).
///
/// # Errors
/// Returns an error if `bytes` is not a decodable image or encoding fails.
pub(crate) fn make_thumbnail(bytes: &[u8], size: u32) -> anyhow::Result<Vec<u8>> {
    let img = match decode_jpeg_scaled(bytes, size) {
        Some(img) => img,
        None => image::load_from_memory(bytes)?,
    };
    // resize (not resize_to_fill) preserves aspect ratio, fitting the image
    // within a size x size box; the client applies contain/cover.
    let fitted = img.resize(size, size, image::imageops::FilterType::Triangle);
    let rgb = fitted.to_rgb8();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    enc.encode_image(&rgb)?;
    Ok(out)
}

/// Decode a JPEG at reduced resolution via IDCT scaling: the decoder produces
/// the smallest DCT scale whose dimensions still cover the requested size, so a
/// multi-megapixel photo is decoded at roughly thumbnail scale instead of full
/// size — the dominant per-thumbnail cost for large sources (#51). Returns
/// `None` for non-JPEG bytes or exotic pixel formats (CMYK); callers fall back
/// to the generic full decode.
fn decode_jpeg_scaled(bytes: &[u8], size: u32) -> Option<image::DynamicImage> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    dec.read_info().ok()?;
    let header = dec.info()?;
    let (fw, fh) = (u32::from(header.width), u32::from(header.height));
    // Pick the smallest DCT scale (s/8ths of full size) whose SHORTER side
    // still covers the thumbnail edge, so the fit resize never upscales (both
    // edges stay >= size). The decoder's own scale() picks by the longer side,
    // which can undershoot.
    let min_side = fw.min(fh).max(1);
    let s = (8 * size).div_ceil(min_side).clamp(1, 8);
    let (tw, th) = (
        u16::try_from(fw * s / 8).ok()?,
        u16::try_from(fh * s / 8).ok()?,
    );
    dec.scale(tw, th).ok()?;
    let pixels = dec.decode().ok()?;
    let info = dec.info()?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => {
            image::RgbImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageRgb8)
        }
        jpeg_decoder::PixelFormat::L8 => {
            image::GrayImage::from_raw(w, h, pixels).map(image::DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, decodable PNG of arbitrary dimensions, generated via the `image`
    /// crate so the thumbnailer has true pixel data to work with.
    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn make_thumbnail_preserves_aspect_ratio() {
        let src = png_bytes(40, 20); // deliberately non-square (2:1)
        let out = make_thumbnail(&src, 32).unwrap();
        // JPEG SOI marker.
        assert_eq!(&out[..2], &[0xFF, 0xD8]);
        // Aspect-preserving fit: the longer edge is the requested size and the
        // shorter edge scales proportionally (2:1 -> 32x16), NOT cover-cropped
        // to a square. This is what lets the client's Frame/Fill toggle work.
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 16));
    }

    #[test]
    fn make_thumbnail_rejects_non_image() {
        assert!(make_thumbnail(b"not an image", 32).is_err());
    }

    /// A real JPEG source exercises the scaled-decode fast path end to end.
    fn jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut buf),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        buf
    }

    #[test]
    fn make_thumbnail_handles_jpeg_via_scaled_decode() {
        // Large enough that the decoder actually picks a reduced DCT scale.
        let src = jpeg_bytes(1024, 640); // 16:10
        let out = make_thumbnail(&src, 32).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();
        // Aspect-preserving: longer edge == size, shorter scales (16:10 -> 32x20).
        assert_eq!((decoded.width(), decoded.height()), (32, 20));
    }

    #[test]
    fn make_thumbnail_handles_grayscale_jpeg() {
        let img = image::GrayImage::from_fn(400, 300, |x, _| image::Luma([(x % 256) as u8]));
        let mut src = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut src),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let out = make_thumbnail(&src, 32).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();
        // Aspect-preserving: 4:3 -> 32x24.
        assert_eq!((decoded.width(), decoded.height()), (32, 24));
    }

    #[test]
    fn scaled_decode_covers_the_requested_size() {
        // The scaled decode must never hand back dimensions smaller than the
        // thumbnail edge, or resize_to_fill would upscale.
        let src = jpeg_bytes(1600, 1200);
        let img = decode_jpeg_scaled(&src, 360).unwrap();
        assert!(img.width() >= 360 && img.height() >= 360);
        // And it genuinely decoded below full resolution.
        assert!(img.width() < 1600);
    }

    #[test]
    fn scaled_decode_rejects_non_jpeg() {
        assert!(decode_jpeg_scaled(&png_bytes(40, 20), 32).is_none());
        assert!(decode_jpeg_scaled(b"not an image", 32).is_none());
    }

    /// Encode a small RGB image to the requested format. Used to build test
    /// fixtures for every format in `SUPPORTED_IMAGE_EXTENSIONS` that the
    /// `image` crate can decode, so a future feature-flag regression is caught
    /// at test time rather than silently at runtime.
    fn format_sample_bytes(fmt: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 32) as u8, (y * 32) as u8, 128])
        });
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), fmt)
            .expect("test fixture encode failed");
        buf
    }

    /// Each format kept in the workspace image feature set must round-trip
    /// through `make_thumbnail`. A failure here means the feature was removed
    /// and thumbnailing would silently break for real files of that type.
    ///
    /// JPEG and PNG are already covered by `make_thumbnail_preserves_aspect_ratio`
    /// and `make_thumbnail_handles_jpeg_via_scaled_decode`. This test covers the
    /// remaining indexed formats.
    #[test]
    fn make_thumbnail_decodes_all_kept_formats() {
        for (label, fmt) in [
            ("BMP", image::ImageFormat::Bmp),
            ("GIF", image::ImageFormat::Gif),
            ("WebP", image::ImageFormat::WebP),
            ("TIFF", image::ImageFormat::Tiff),
        ] {
            let src = format_sample_bytes(fmt);
            assert!(
                make_thumbnail(&src, 16).is_ok(),
                "{label} decode failed — was the image feature for this format dropped?"
            );
        }
    }

    #[test]
    fn make_thumbnail_does_not_depend_on_the_global_rayon_pool() {
        use std::sync::{Arc, Barrier};
        use std::time::Duration;

        // Two-phase barrier: `ready` lets us confirm all workers are actually
        // parked before the decode starts (a single barrier races: the decode
        // could finish before any worker blocks, so broken global-pool code
        // would also pass).
        //
        // Park every global rayon worker; a decode that offloads to the global
        // pool would wedge here, as progressive JPEGs did behind a slow
        // startup scan (#65).
        let n = rayon::current_num_threads();
        let ready = Arc::new(Barrier::new(n + 1)); // "I am parked"
        let gate = Arc::new(Barrier::new(n + 1)); // "you may leave"
        for _ in 0..n {
            let (ready, gate) = (Arc::clone(&ready), Arc::clone(&gate));
            rayon::spawn(move || {
                ready.wait();
                gate.wait();
            });
        }
        ready.wait(); // all workers parked before the decode starts

        // A real progressive JPEG (SOF2) — the only coding mode where jpeg-decoder
        // ever offloaded to the global rayon pool. Generated with PIL:
        // Image.new('RGB',(64,40),(200,30,90)).save(p,'JPEG',progressive=True,quality=85)
        let src = include_bytes!("testdata/progressive.jpg").to_vec();
        assert!(
            src.windows(2).any(|w| w == [0xFF, 0xC2]),
            "fixture must be progressive (SOF2)"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(make_thumbnail(&src, 32));
        });
        let out = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("make_thumbnail wedged behind the saturated global rayon pool");
        assert!(out.is_ok());

        gate.wait(); // release the global pool for the rest of the suite
        worker.join().unwrap(); // decode already finished; join is cleanup
    }
}
