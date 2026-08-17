//! Best-effort intrinsic metadata extraction.
//!
//! Per ADR 0003 this is a pass that runs *after* hashing and never blocks
//! import: anything not recognized as a supported image yields an empty
//! [`FileMetadata`]. Phase 1 covers image dimensions + MIME type; video
//! duration comes later.

use std::fs;
use std::io::Read;
use std::path::Path;

use imagesize::{ImageType, image_type, size};
use naiad_core::FileMetadata;

/// Extract image dimensions and MIME type from the file at `path`.
///
/// Best-effort and infallible: unreadable files and non-image content yield a
/// `FileMetadata` with all fields `None`.
#[must_use]
pub fn extract_metadata(path: &Path) -> FileMetadata {
    let dims = size(path).ok();
    let mime = read_header(path)
        .and_then(|header| image_type(&header).ok())
        .and_then(mime_for);
    let meta = FileMetadata {
        mime,
        width: dims.as_ref().map(|d| d.width as u32),
        height: dims.as_ref().map(|d| d.height as u32),
    };
    tracing::trace!(target: "scan", path = %path.display(), "extracted metadata");
    if meta == FileMetadata::default() && crate::is_supported_image(path) {
        tracing::debug!(target: "scan", path = %path.display(), "supported image yielded empty metadata");
    }
    meta
}

/// Read up to 1 KiB from the front of the file — enough for any image's magic
/// bytes, without loading large media into memory.
fn read_header(path: &Path) -> Option<Vec<u8>> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 1024];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Map a detected image type to its MIME string, or `None` for kinds we don't
/// surface in Phase 1.
fn mime_for(kind: ImageType) -> Option<String> {
    let mime = match kind {
        ImageType::Bmp => "image/bmp",
        ImageType::Gif => "image/gif",
        ImageType::Jpeg => "image/jpeg",
        ImageType::Png => "image/png",
        ImageType::Tiff => "image/tiff",
        ImageType::Webp => "image/webp",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use naiad_test_support::fixture_dir;

    // A minimal valid GIF89a: signature + logical screen descriptor declaring a
    // 4x7 canvas (width/height are little-endian at offsets 6 and 8), then the
    // trailer byte. Enough for a header reader to report dimensions + type.
    const GIF_4X7: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x39, 0x61, // "GIF89a"
        0x04, 0x00, // width = 4
        0x07, 0x00, // height = 7
        0x00, 0x00, 0x00, // packed fields, bg color, aspect ratio
        0x3B, // trailer
    ];

    #[test]
    fn reads_gif_dimensions_and_mime() {
        let dir = fixture_dir(&[("pic.gif", GIF_4X7)]);
        let meta = extract_metadata(&dir.path().join("pic.gif"));
        assert_eq!(meta.width, Some(4));
        assert_eq!(meta.height, Some(7));
        assert_eq!(meta.mime.as_deref(), Some("image/gif"));
    }

    #[test]
    fn non_image_yields_empty_metadata() {
        let dir = fixture_dir(&[("notes.txt", b"hello world")]);
        let meta = extract_metadata(&dir.path().join("notes.txt"));
        assert_eq!(meta, FileMetadata::default());
    }
}
