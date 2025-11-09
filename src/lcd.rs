use crate::Mode;
use elgato_streamdeck::AsyncStreamDeck;
use elgato_streamdeck::images::convert_image_with_format;
use image::{DynamicImage, Rgb};

pub async fn update_lcd_mode(
    device: &AsyncStreamDeck,
    mode: Mode,
    img_playback: &DynamicImage,
    img_edit: &DynamicImage,
) {
    // ... (This function is unchanged)
    println!("Setting LCD mode to: {:?}", mode);
    let img_to_use = match mode {
        Mode::Playback => img_playback,
        Mode::Edit => img_edit,
    };
    if let Some(format) = device.kind().lcd_image_format() {
        let scaled_image = img_to_use.clone().resize_to_fill(
            format.size.0 as u32,
            format.size.1 as u32,
            image::imageops::FilterType::Nearest,
        );
        let converted_image = convert_image_with_format(format, scaled_image).unwrap();
        let _ = device.write_lcd_fill(&converted_image).await;
    } else {
        eprintln!("Failed to set LCD image (is this a Stream Deck Plus?)");
    }
}

pub fn create_fallback_image(color: Rgb<u8>) -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(72, 72, move |_, _| color))
}

pub fn create_fallback_lcd_image(color: Rgb<u8>) -> DynamicImage {
    DynamicImage::ImageRgb8(image::RgbImage::from_fn(800, 100, move |_, _| color))
}
