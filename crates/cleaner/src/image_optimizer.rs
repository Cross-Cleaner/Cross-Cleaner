use image::AnimationDecoder;
use image::DynamicImage;
use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::{GifDecoder, GifEncoder};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::codecs::webp::WebPEncoder;
use std::io::{self, BufReader, Cursor, Write};
use std::path::Path;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];
const JPEG_QUALITY: u8 = 85;
/// Hard cap: images larger than this are skipped to bound peak RAM.
const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

fn is_image_extension(ext: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&ext)
}

pub fn optimize_single(path: &Path) -> Result<crate::custom_cleaners::GlobCleanStats, io::Error> {
    optimize_single_with(path, false)
}

pub fn optimize_single_with(
    path: &Path,
    dry_run: bool,
) -> Result<crate::custom_cleaners::GlobCleanStats, io::Error> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    database::diag::info(format!(
        "[image_optimize] path={} ext={ext:?} is_image={}",
        path.display(),
        is_image_extension(&ext)
    ));

    if !is_image_extension(&ext) {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // The handle, not the path, is what the replacement lands in: resolving
    // the parent once here means a link swapped in for one of its components
    // afterwards cannot redirect the write outside the tree that was checked.
    let parent_dir = crate::open_dir_without_links(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path names no file"))?;
    let original = parent_dir.symlink_metadata(name)?;
    if original.is_symlink() || !original.is_file() {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    }
    let original_size = original.len();
    if original_size > MAX_IMAGE_BYTES {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    }
    database::diag::info(format!("[image_optimize] original_size={original_size}"));

    if ext == "gif" {
        let decoder =
            GifDecoder::new(BufReader::new(parent_dir.open(name)?)).map_err(io::Error::other)?;
        let mut frames = decoder.into_frames();
        frames.next().transpose().map_err(io::Error::other)?;
        if frames
            .next()
            .transpose()
            .map_err(io::Error::other)?
            .is_some()
        {
            return Ok(crate::custom_cleaners::GlobCleanStats::default());
        }
    }

    let img = image::ImageReader::new(BufReader::new(parent_dir.open(name)?))
        .with_guessed_format()
        .map_err(|e| {
            database::diag::warn(format!("[image_optimize] open failed: {e}"));
            io::Error::new(io::ErrorKind::InvalidData, e)
        })?
        .decode()
        .map_err(|e| {
            database::diag::warn(format!("[image_optimize] open failed: {e}"));
            io::Error::new(io::ErrorKind::InvalidData, e)
        })?;
    // Bound peak: reject huge decoded pixel buffers (e.g. 5000x5000 RGBA ~100MB).
    let (w, h) = (img.width() as u64, img.height() as u64);
    if w.checked_mul(h).unwrap_or(u64::MAX).saturating_mul(4) > MAX_IMAGE_BYTES {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    }

    let compressed = {
        let mut buf = Cursor::new(Vec::new());
        match ext.as_str() {
            "png" => encode_png(&mut buf, &img)?,
            "jpg" | "jpeg" => encode_jpeg(&mut buf, &img)?,
            "webp" => encode_webp(&mut buf, &img)?,
            "gif" => encode_gif(&mut buf, &img)?,
            "bmp" => encode_bmp(&mut buf, &img)?,
            _ => return Ok(crate::custom_cleaners::GlobCleanStats::default()),
        }
        buf.into_inner()
    };
    // Release decoded image before allocating replacement file buffer.
    drop(img);
    let new_size = compressed.len() as u64;

    database::diag::info(format!(
        "[image_optimize] new_size={new_size} saved={}",
        original_size.saturating_sub(new_size)
    ));

    if new_size >= original_size {
        return Ok(crate::custom_cleaners::GlobCleanStats::default());
    }

    let saved = original_size - new_size;
    if dry_run {
        database::diag::info(format!("[image_optimize] DRY-RUN saved={saved}"));
        return Ok(crate::custom_cleaners::GlobCleanStats::file(saved));
    }

    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement.write_all(&compressed)?;
    replacement.as_file().sync_all()?;
    let permissions = original.permissions().into_std(replacement.as_file())?;
    replacement.as_file().set_permissions(permissions)?;

    let replacement_path = replacement.into_temp_path();
    let replacement_name = replacement_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "replacement names no file"))?;

    // Renamed through the pinned parent handle, so the destination is the
    // directory that was checked rather than whatever the path names now.
    parent_dir.rename(replacement_name, &parent_dir, name)?;
    // The temp name is gone, so dropping the guard finds nothing to clean up.
    drop(replacement_path);

    database::diag::info(format!("[image_optimize] WRITTEN saved={saved}"));
    Ok(crate::custom_cleaners::GlobCleanStats::file(saved))
}

fn encode_png(w: &mut impl Write, img: &DynamicImage) -> io::Result<()> {
    let encoder = PngEncoder::new_with_quality(w, CompressionType::Best, FilterType::Adaptive);
    img.write_with_encoder(encoder).map_err(io::Error::other)
}

fn encode_jpeg(w: &mut impl Write, img: &DynamicImage) -> io::Result<()> {
    let encoder = JpegEncoder::new_with_quality(w, JPEG_QUALITY);
    img.write_with_encoder(encoder).map_err(io::Error::other)
}

fn encode_webp(w: &mut impl Write, img: &DynamicImage) -> io::Result<()> {
    let encoder = WebPEncoder::new_lossless(w);
    img.write_with_encoder(encoder).map_err(io::Error::other)
}

fn encode_gif(w: &mut impl Write, img: &DynamicImage) -> io::Result<()> {
    let encoder = GifEncoder::new(w);
    img.write_with_encoder(encoder).map_err(io::Error::other)
}

fn encode_bmp(w: &mut impl Write, img: &DynamicImage) -> io::Result<()> {
    let encoder = BmpEncoder::new(w);
    img.write_with_encoder(encoder).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Frame, ImageEncoder, Rgba, RgbaImage};
    use std::fs::File;

    #[test]
    fn optimized_png_replaces_original_with_same_pixels() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("image.png");
        let image = RgbaImage::from_fn(256, 256, |x, y| {
            Rgba([x as u8, y as u8, (x + y) as u8, 255])
        });
        let file = File::create(&path).unwrap();
        PngEncoder::new_with_quality(file, CompressionType::Fast, FilterType::NoFilter)
            .write_image(image.as_raw(), 256, 256, image::ExtendedColorType::Rgba8)
            .unwrap();
        let original_size = std::fs::metadata(&path).unwrap().len();

        let result = optimize_single(&path).unwrap();
        assert_eq!(result.files, 1);
        assert_eq!(
            result.bytes,
            original_size - std::fs::metadata(&path).unwrap().len()
        );
        assert_eq!(image::open(&path).unwrap().to_rgba8(), image);
    }
    #[test]
    fn dry_run_measures_png_savings_without_rewriting_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("image.png");
        let image = RgbaImage::from_fn(256, 256, |x, y| {
            Rgba([x as u8, y as u8, (x + y) as u8, 255])
        });
        let file = File::create(&path).unwrap();
        PngEncoder::new_with_quality(file, CompressionType::Fast, FilterType::NoFilter)
            .write_image(image.as_raw(), 256, 256, image::ExtendedColorType::Rgba8)
            .unwrap();
        let before = std::fs::read(&path).unwrap();

        let dry = optimize_single_with(&path, true).unwrap();
        assert_eq!(dry.files, 1);
        assert!(dry.bytes > 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let real = optimize_single_with(&path, false).unwrap();
        assert_eq!(dry, real);
    }

    #[test]
    fn animated_gif_is_left_unchanged() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("animated.gif");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = GifEncoder::new(file);
        let frames = [Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])]
            .into_iter()
            .map(|color| Frame::new(RgbaImage::from_pixel(64, 64, color)));
        encoder.encode_frames(frames).unwrap();
        drop(encoder);

        let original = std::fs::read(&path).unwrap();
        assert_eq!(optimize_single(&path).unwrap().files, 0);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn linked_image_parent_is_refused() {
        use crate::{dir_symlink, skip_without_symlink};
        let temp_dir = tempfile::tempdir().unwrap();
        let outside = temp_dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let image = outside.join("image.png");
        RgbaImage::new(2, 2).save(&image).unwrap();
        let original = std::fs::read(&image).unwrap();
        let link = temp_dir.path().join("link");
        if skip_without_symlink(dir_symlink(&outside, &link)) {
            return;
        }

        assert!(optimize_single(&link.join("image.png")).is_err());
        assert_eq!(std::fs::read(&image).unwrap(), original);
    }
}
