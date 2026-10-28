//! Getting a page ready to show: decode it and shrink it to the size it's displayed at.
//!
//! Pages are only ever made smaller here. Making them bigger is left to the terminal, which
//! scales images on the GPU; doing it on the CPU was most of the cost of showing a page.

use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, RgbImage};

use super::{LocalChapter, pages};

/// The largest image side the terminal accepts.
const MAX_SIDE: f64 = 10_000.0;

/// The largest size a page will be shown at, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub width: u32,
    /// `None` when the page can be any height, e.g. when it's scrolled in fit-width mode.
    pub height: Option<u32>,
}

/// A page as 8-bit RGB, which the kitty protocol accepts as is.
pub struct PreparedPage {
    /// Size of the original image.
    pub native: (u32, u32),
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl PreparedPage {
    pub fn into_image(self) -> DynamicImage {
        let image = RgbImage::from_raw(self.width, self.height, self.rgb)
            .expect("the buffer matches the dimensions");
        DynamicImage::ImageRgb8(image)
    }
}

/// Decodes `page` of `chapter` and shrinks it to fit `bounds`.
pub fn prepare(chapter: &LocalChapter, page: &str, bounds: Bounds) -> Result<PreparedPage, String> {
    Ok(shrink_to(pages::load(chapter, page)?, bounds))
}

/// Shrinks `image` to fit `bounds`, keeping its aspect ratio. Images that already fit are
/// only converted to RGB.
pub fn shrink_to(image: DynamicImage, bounds: Bounds) -> PreparedPage {
    let native = (image.width(), image.height());
    let (width, height) = fitted_size(native, bounds);
    PreparedPage {
        native,
        width,
        height,
        rgb: resize_rgb(image.into_rgb8(), (width, height)).into_raw(),
    }
}

/// Scales `image` to fill `size` pixels exactly (a strip page's cells), so neighbouring pages
/// meet without a gap. When that would make it wider, it keeps its width and takes the cells'
/// shape, rounded to be a little taller rather than shorter: the terminal then fits the
/// height exactly and leaves any sliver at the sides, not between pages.
pub fn fill_cells(image: DynamicImage, size: (u32, u32)) -> PreparedPage {
    let native = (image.width(), image.height());
    let (width, height) = if native.0 >= size.0 {
        size
    } else {
        let height = f64::from(native.0) * f64::from(size.1) / f64::from(size.0.max(1));
        (native.0, (height.ceil() as u32).max(1))
    };
    PreparedPage {
        native,
        width,
        height,
        rgb: resize_rgb(image.into_rgb8(), (width, height)).into_raw(),
    }
}

/// Resizes to exactly `size` with the fast SIMD resizer; returns `image` as is when it's
/// already that size.
pub fn resize_rgb(image: RgbImage, size: (u32, u32)) -> RgbImage {
    if image.dimensions() == size {
        return image;
    }
    let mut resized = RgbImage::new(size.0.max(1), size.1.max(1));
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom));
    Resizer::new()
        .resize(&image, &mut resized, &options)
        .expect("source and destination are both RGB8");
    resized
}

/// The size `native` is shown at within `bounds`, never larger than `native`.
pub fn fitted_size(native: (u32, u32), bounds: Bounds) -> (u32, u32) {
    let (width, height) = (f64::from(native.0.max(1)), f64::from(native.1.max(1)));
    let mut scale = (f64::from(bounds.width) / width).min(1.0);
    if let Some(max_height) = bounds.height {
        scale = scale.min(f64::from(max_height) / height);
    }
    // Ghostty rejects images over 10000 px on either side; it scales them up to the
    // placement anyway.
    scale = scale.min(MAX_SIDE / width).min(MAX_SIDE / height);
    let fitted = |side: f64| ((side * scale).round() as u32).max(1);
    (fitted(width), fitted(height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ever_shrinks() {
        let bounds = |width, height| Bounds { width, height };
        assert_eq!(
            fitted_size((848, 1216), bounds(1900, Some(1000))),
            (697, 1000)
        );
        assert_eq!(
            fitted_size((848, 1216), bounds(3800, Some(2000))),
            (848, 1216)
        );
        assert_eq!(fitted_size((848, 1216), bounds(424, None)), (424, 608));
        assert_eq!(fitted_size((1, 1), bounds(0, Some(0))), (1, 1));
        assert_eq!(fitted_size((800, 16000), bounds(800, None)), (500, 10000));
    }

    #[test]
    fn fills_strip_cells_without_widening() {
        let page = fill_cells(DynamicImage::new_rgb8(800, 2010), (400, 1000));
        assert_eq!((page.width, page.height), (400, 1000), "exactly the cells");
        let narrow = fill_cells(DynamicImage::new_rgb8(300, 760), (600, 1500));
        assert_eq!(
            (narrow.width, narrow.height),
            (300, 750),
            "the cells' shape, own width"
        );
    }

    #[test]
    fn shrinks_to_rgb_of_the_fitted_size() {
        let page = shrink_to(
            DynamicImage::new_rgba8(400, 600),
            Bounds {
                width: 100,
                height: None,
            },
        );
        assert_eq!(
            (page.native, page.width, page.height),
            ((400, 600), 100, 150)
        );
        assert_eq!(page.rgb.len(), 100 * 150 * 3);

        let small = shrink_to(
            DynamicImage::new_luma8(10, 20),
            Bounds {
                width: 100,
                height: None,
            },
        );
        assert_eq!((small.width, small.height, small.rgb.len()), (10, 20, 600));
    }
}
