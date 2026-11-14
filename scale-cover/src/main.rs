use std::fs::File;
use std::io::Read;
use std::path::Path;
use image::imageops::FilterType;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use zune_jpeg::{JpegDecoder, zune_core::colorspace::ColorSpace};

fn main() {
    let input_path = "folder.jpg";
    let output_path = "front.jpg";

    println!("Checking whether '{}' is a valid JPEG image...", input_path);
    if let Err(err) = process_cover(input_path, output_path) {
        eprintln!("Error: {}", err);
    }
}

fn process_cover(input_path: &str, output_path: &str) -> Result<(), String> {
    // 1. Validate input JPEG and rewrite it in-place as a clean, standardized JPEG.
    if let Err(err) = validate_and_reencode_jpeg(input_path, input_path) {
        return Err(format!(
            "'{}' is not a valid/re-encodable JPEG file: {}",
            input_path, err
        ));
    }
    println!("✓ File structure validated and re-encoded.");

    // 2. Load origin image from folder.jpg
    let origin_img = match image::open(input_path) {
        Ok(i) => i,
        Err(e) => {
            return Err(format!("loading image '{}': {}", input_path, e));
        }
    };

    // 3. Accept origin image only if both sides are >= 1000 pixels.
    let (width, height) = (origin_img.width(), origin_img.height());
    if width < 1000 || height < 1000 {
        return Err(format!(
            "Origin image '{}' must be at least 1000x1000 (got {}x{}).",
            input_path,
            width, height
        ));
    }

    // 4. Copy cleaned origin to output path before scaling.
    copy_file_atomically(input_path, output_path)?;
    println!(
        "✓ Copied cleaned origin '{}' to '{}'.",
        input_path, output_path
    );

    println!("Scaling image to 1000x1000 pixels...");

    // 5. Load copied cleaned image from output path for scaling.
    let img = match image::open(output_path) {
        Ok(i) => i,
        Err(e) => {
            return Err(format!("loading image '{}': {}", output_path, e));
        }
    };

    // 6. Crop to a square starting from top-left (0,0).
    let square_side = width.min(height);
    let square_img = img.crop_imm(0, 0, square_side, square_side);

    // 7. Scale square image exactly to 1000x1000.
    let scaled_img = square_img.resize_exact(1000, 1000, FilterType::Lanczos3);

    // 8. Save processed image atomically as clean JPEG with explicit 4:2:0 subsampling.
    let rgb = scaled_img.to_rgb8();
    let output_tmp_path = format!("{output_path}.tmp");
    let output_file = match File::create(&output_tmp_path) {
        Ok(file) => file,
        Err(e) => {
            return Err(format!(
                "creating temporary output image '{}': {}",
                output_tmp_path, e
            ));
        }
    };

    let mut encoder = Encoder::new(output_file, 100);
    encoder.set_sampling_factor(SamplingFactor::R_4_2_0);

    if let Err(e) = encoder.encode(&rgb, 1000, 1000, ColorType::Rgb) {
        remove_file_if_exists(&output_tmp_path, "temporary output file");
        return Err(format!("saving scaled image: {}", e));
    }

    if let Err(e) = std::fs::rename(&output_tmp_path, output_path) {
        remove_file_if_exists(&output_tmp_path, "temporary output file");
        return Err(format!(
            "replacing '{}' with '{}': {}",
            output_path, output_tmp_path, e
        ));
    }

    println!(
        "✓ Image successfully scaled from '{}' and saved as '{}'.",
        input_path, output_path
    );
    Ok(())
}

fn copy_file_atomically(source_path: &str, target_path: &str) -> Result<(), String> {
    if source_path == target_path {
        return Ok(());
    }

    let target_tmp_path = format!("{target_path}.tmp");
    if let Err(e) = std::fs::copy(source_path, &target_tmp_path) {
        return Err(format!(
            "copying '{}' to temporary '{}': {}",
            source_path, target_tmp_path, e
        ));
    }

    if let Err(e) = std::fs::rename(&target_tmp_path, target_path) {
        remove_file_if_exists(&target_tmp_path, "temporary copied file");
        return Err(format!(
            "replacing '{}' with '{}': {}",
            target_path, target_tmp_path, e
        ));
    }

    Ok(())
}

fn remove_file_if_exists(path: &str, label: &str) {
    if !Path::new(path).exists() {
        return;
    }

    match std::fs::remove_file(path) {
        Ok(_) => {
            println!("✓ Removed {} '{}'.", label, path);
        }
        Err(e) => {
            eprintln!("Error removing {} '{}': {}", label, path, e);
        }
    }
}

fn validate_and_reencode_jpeg(
    input_path: &str,
    output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open(input_path)?;
    let mut input_bytes = Vec::new();
    file.read_to_end(&mut input_bytes)?;

    let mut decoder = JpegDecoder::new(&input_bytes);
    let pixels = decoder.decode()?;

    let info = decoder
        .info()
        .ok_or("Failed to read JPEG metadata during validation")?;
    let width = info.width;
    let height = info.height;
    let colorspace = decoder
        .get_output_colorspace()
        .ok_or("Failed to detect JPEG colorspace during validation")?;

    if colorspace != ColorSpace::RGB {
        return Err("Only RGB JPEG images are supported in this workflow".into());
    }

    let output_tmp_path = format!("{output_path}.tmp");
    let output_file = File::create(&output_tmp_path)?;
    let mut encoder = Encoder::new(output_file, 90);
    encoder.set_sampling_factor(SamplingFactor::R_4_2_0);
    if let Err(e) = encoder.encode(&pixels, width, height, ColorType::Rgb) {
        remove_file_if_exists(&output_tmp_path, "temporary validation output file");
        return Err(e.into());
    }

    if let Err(e) = std::fs::rename(&output_tmp_path, output_path) {
        remove_file_if_exists(&output_tmp_path, "temporary validation output file");
        return Err(e.into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "scale_cover_tests_{}_{}_{}",
            name,
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).expect("failed to create test directory");
        dir
    }

    fn write_jpeg(path: &Path, width: u32, height: u32) {
        let mut img = RgbImage::new(width, height);
        for px in img.pixels_mut() {
            *px = Rgb([42, 84, 126]);
        }
        DynamicImage::ImageRgb8(img)
            .save_with_format(path, image::ImageFormat::Jpeg)
            .expect("failed to save jpeg test image");
    }

    #[test]
    fn validate_and_reencode_jpeg_creates_output_without_tmp_file() {
        let dir = test_dir("validate_success");
        let input = dir.join("in.jpg");
        let output = dir.join("out.jpg");
        let output_tmp = dir.join("out.jpg.tmp");
        write_jpeg(&input, 1200, 1200);

        validate_and_reencode_jpeg(
            input.to_str().expect("utf-8 path expected"),
            output.to_str().expect("utf-8 path expected"),
        )
        .expect("validation should succeed");

        assert!(output.exists(), "expected validation output to exist");
        assert!(
            !output_tmp.exists(),
            "temporary validation file should not remain"
        );

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }

    #[test]
    fn validate_and_reencode_jpeg_invalid_input_does_not_create_output_tmp() {
        let dir = test_dir("validate_invalid");
        let input = dir.join("in.jpg");
        let output = dir.join("out.jpg");
        let output_tmp = dir.join("out.jpg.tmp");
        fs::write(&input, b"not-a-jpeg").expect("failed to write invalid input");

        let result = validate_and_reencode_jpeg(
            input.to_str().expect("utf-8 path expected"),
            output.to_str().expect("utf-8 path expected"),
        );

        assert!(result.is_err(), "invalid jpeg should fail validation");
        assert!(!output.exists(), "output file should not exist");
        assert!(
            !output_tmp.exists(),
            "temporary validation file should not remain"
        );

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }

    #[test]
    fn process_cover_small_origin_rejected_without_output() {
        let dir = test_dir("small_image_cleanup");
        let input = dir.join("folder.jpg");
        let output = dir.join("front_out.jpg");
        write_jpeg(&input, 900, 1200);

        let result = process_cover(
            input.to_str().expect("utf-8 path expected"),
            output.to_str().expect("utf-8 path expected"),
        );

        assert!(result.is_err(), "small image should be rejected");
        assert!(input.exists(), "cleaned origin should remain available");
        assert!(!output.exists(), "output should not be written");

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }

    #[test]
    fn process_cover_rename_failure_cleans_temp_and_preserves_target() {
        let dir = test_dir("rename_failure_cleanup");
        let input = dir.join("folder.jpg");
        let output_target = dir.join("front_out.jpg");
        let output_tmp = dir.join("front_out.jpg.tmp");
        write_jpeg(&input, 1200, 1300);
        fs::create_dir(&output_target).expect("failed to create output target directory");

        let result = process_cover(
            input.to_str().expect("utf-8 path expected"),
            output_target.to_str().expect("utf-8 path expected"),
        );

        assert!(result.is_err(), "rename to directory should fail");
        assert!(output_target.is_dir(), "output target directory should remain");
        assert!(!output_tmp.exists(), "temporary output file should be removed");
        assert!(input.exists(), "folder.jpg should still exist");

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }

    #[test]
    fn process_cover_success_writes_1000_square_and_cleans_temporary_files() {
        let dir = test_dir("process_success");
        let input = dir.join("folder.jpg");
        let output = dir.join("front_out.jpg");
        let output_tmp = dir.join("front_out.jpg.tmp");
        write_jpeg(&input, 1600, 1200);

        process_cover(
            input.to_str().expect("utf-8 path expected"),
            output.to_str().expect("utf-8 path expected"),
        )
        .expect("processing should succeed");

        assert!(output.exists(), "expected output image to exist");
        assert!(input.exists(), "folder.jpg should exist");
        assert!(!output_tmp.exists(), "temporary output should be removed");

        let out_img = image::open(&output).expect("failed to open output image");
        assert_eq!(out_img.dimensions(), (1000, 1000), "unexpected output size");
        let folder_img = image::open(&input).expect("failed to open folder image");
        assert_eq!(
            folder_img.dimensions(),
            (1600, 1200),
            "folder.jpg should keep original dimensions"
        );

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }

    #[test]
    fn process_cover_reuses_existing_folder_as_origin() {
        let dir = test_dir("reuse_existing_folder");
        let input = dir.join("folder.jpg");
        let front = dir.join("front.jpg");
        let output_one = dir.join("front_out_1.jpg");
        let output_two = dir.join("front_out_2.jpg");
        write_jpeg(&input, 1600, 1200);

        process_cover(
            input.to_str().expect("utf-8 path expected"),
            output_one.to_str().expect("utf-8 path expected"),
        )
        .expect("first processing should succeed");

        fs::write(&front, b"not-a-jpeg").expect("failed to write unrelated front.jpg");

        process_cover(
            input.to_str().expect("utf-8 path expected"),
            output_two.to_str().expect("utf-8 path expected"),
        )
        .expect("second processing should reuse existing folder.jpg origin");

        let output_two_img = image::open(&output_two).expect("failed to open second output");
        assert_eq!(
            output_two_img.dimensions(),
            (1000, 1000),
            "unexpected second output size"
        );
        let folder_img = image::open(&input).expect("failed to open folder image");
        assert_eq!(
            folder_img.dimensions(),
            (1600, 1200),
            "folder.jpg should remain the unscaled origin"
        );

        fs::remove_dir_all(dir).expect("failed to clean test directory");
    }
}
