//! Pictures through Windows Imaging Component: decoding (with camera rotation applied),
//! high-quality scaling onto a display-sized canvas, and PNG/JPEG output.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_ContainerFormatPng,
    GUID_WICPixelFormat32bppPBGRA, IWICBitmapFrameDecode, IWICBitmapSource, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant,
    WICBitmapInterpolationModeHighQualityCubic, WICBitmapPaletteTypeCustom,
    WICBitmapTransformFlipHorizontal, WICBitmapTransformFlipVertical, WICBitmapTransformOptions,
    WICBitmapTransformRotate90, WICBitmapTransformRotate180, WICBitmapTransformRotate270,
    WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantClear, PropVariantToUInt16,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::core::{GUID, HSTRING, w};

use crate::layout::{self, Rect};
use crate::model::Fit;

/// Premultiplied BGRA pixels, tightly packed.
#[derive(Clone, Debug)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Bitmap {
    pub fn black(width: u32, height: u32) -> Self {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        for px in pixels.as_chunks_mut::<4>().0.iter_mut() {
            px[3] = 255;
        }
        Self {
            width,
            height,
            pixels,
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.pixels[i],
            self.pixels[i + 1],
            self.pixels[i + 2],
            self.pixels[i + 3],
        ]
    }

    /// Straight RGBA, as egui wants it.
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pixels.len());
        for px in self.pixels.as_chunks::<4>().0.iter() {
            out.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        out
    }
}

pub fn factory() -> Result<IWICImagingFactory> {
    unsafe {
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .context("WIC factory")
    }
}

/// Size of a picture file without decoding its pixels.
pub fn dimensions(path: &Path) -> Result<(u32, u32)> {
    let f = factory()?;
    let src = open(&f, path)?;
    size_of_source(&src)
}

fn size_of_source(src: &IWICBitmapSource) -> Result<(u32, u32)> {
    let (mut w, mut h) = (0u32, 0u32);
    unsafe { src.GetSize(&mut w, &mut h)? };
    if w == 0 || h == 0 {
        bail!("empty picture");
    }
    Ok((w, h))
}

/// Opens a picture's first frame, turned the way the camera recorded it.
pub fn open(f: &IWICImagingFactory, path: &Path) -> Result<IWICBitmapSource> {
    unsafe {
        let decoder = f
            .CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                windows::Win32::Foundation::GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .with_context(|| format!("Windows can't read {} as a picture", path.display()))?;
        let frame = decoder.GetFrame(0)?;
        let transform = orientation(&frame);
        let src: IWICBitmapSource = frame.into();
        if transform.0 == 0 {
            return Ok(src);
        }
        let rot = f.CreateBitmapFlipRotator()?;
        rot.Initialize(&src, transform)?;
        Ok(rot.into())
    }
}

/// EXIF orientation (1-8) as a WIC transform.
fn orientation(frame: &IWICBitmapFrameDecode) -> WICBitmapTransformOptions {
    let value = unsafe {
        let Ok(reader) = frame.GetMetadataQueryReader() else {
            return WICBitmapTransformOptions(0);
        };
        let mut pv = PROPVARIANT::default();
        if reader
            .GetMetadataByName(w!("System.Photo.Orientation"), &mut pv)
            .is_err()
        {
            return WICBitmapTransformOptions(0);
        }
        let v = PropVariantToUInt16(&pv).unwrap_or(1);
        let _ = PropVariantClear(&mut pv);
        v
    };
    orientation_transform(value)
}

pub fn orientation_transform(exif: u16) -> WICBitmapTransformOptions {
    let flip_h = WICBitmapTransformFlipHorizontal.0;
    let flip_v = WICBitmapTransformFlipVertical.0;
    WICBitmapTransformOptions(match exif {
        2 => flip_h,
        3 => WICBitmapTransformRotate180.0,
        4 => flip_v,
        5 => WICBitmapTransformRotate90.0 | flip_h,
        6 => WICBitmapTransformRotate90.0,
        7 => WICBitmapTransformRotate270.0 | flip_h,
        8 => WICBitmapTransformRotate270.0,
        _ => 0,
    })
}

fn to_pbgra(f: &IWICImagingFactory, src: &IWICBitmapSource) -> Result<IWICBitmapSource> {
    unsafe {
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(
            src,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        Ok(conv.into())
    }
}

fn scaled(
    f: &IWICImagingFactory,
    src: &IWICBitmapSource,
    w: u32,
    h: u32,
) -> Result<IWICBitmapSource> {
    let (sw, sh) = size_of_source(src)?;
    if (sw, sh) == (w, h) {
        return Ok(src.clone());
    }
    unsafe {
        let scaler = f.CreateBitmapScaler()?;
        if scaler
            .Initialize(src, w, h, WICBitmapInterpolationModeHighQualityCubic)
            .is_err()
        {
            scaler.Initialize(src, w, h, WICBitmapInterpolationModeFant)?;
        }
        Ok(scaler.into())
    }
}

/// Draws `src` onto a black `width` x `height` canvas the way `fit` says.
pub fn render_source(
    f: &IWICImagingFactory,
    src: &IWICBitmapSource,
    fit: Fit,
    width: u32,
    height: u32,
) -> Result<Bitmap> {
    let (sw, sh) = size_of_source(src)?;
    let placed = layout::place(fit, sw, sh, width, height);
    let mut out = Bitmap::black(width, height);
    let visible = placed.intersect(&Rect::new(0, 0, width as i32, height as i32));
    if visible.is_empty() {
        return Ok(out);
    }
    let pbgra = to_pbgra(f, src)?;
    let source = scaled(f, &pbgra, placed.width() as u32, placed.height() as u32)?;
    let rect = WICRect {
        X: visible.left - placed.left,
        Y: visible.top - placed.top,
        Width: visible.width(),
        Height: visible.height(),
    };
    let stride = width as usize * 4;
    let offset = visible.top as usize * stride + visible.left as usize * 4;
    let len = (visible.height() as usize - 1) * stride + visible.width() as usize * 4;
    unsafe {
        source.CopyPixels(&rect, stride as u32, &mut out.pixels[offset..offset + len])?;
    }
    // Transparent areas become black, not see-through.
    for px in out.pixels.as_chunks_mut::<4>().0.iter_mut() {
        px[3] = 255;
    }
    Ok(out)
}

/// Decodes a picture file and draws it onto a display-sized canvas.
pub fn render_file(path: &Path, fit: Fit, width: u32, height: u32) -> Result<Bitmap> {
    let f = factory()?;
    let src = open(&f, path)?;
    render_source(&f, &src, fit, width, height)
}

/// Wraps premultiplied BGRA rows (any stride) as a WIC source.
pub fn source_from_bgra(
    f: &IWICImagingFactory,
    width: u32,
    height: u32,
    stride: u32,
    pixels: &[u8],
) -> Result<IWICBitmapSource> {
    unsafe {
        let bmp = f.CreateBitmapFromMemory(
            width,
            height,
            &GUID_WICPixelFormat32bppPBGRA,
            stride,
            pixels,
        )?;
        Ok(bmp.into())
    }
}

pub fn save_png(bmp: &Bitmap, path: &Path) -> Result<()> {
    save(bmp, path, &GUID_ContainerFormatPng)
}

pub fn save_jpeg(bmp: &Bitmap, path: &Path) -> Result<()> {
    save(bmp, path, &GUID_ContainerFormatJpeg)
}

fn save(bmp: &Bitmap, path: &Path, container: &GUID) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("part");
    let f = factory()?;
    unsafe {
        let stream = f.CreateStream()?;
        stream.InitializeFromFilename(&HSTRING::from(tmp.as_os_str()), GENERIC_WRITE.0)?;
        let encoder = f.CreateEncoder(container, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.context("encoder frame")?;
        frame.Initialize(None)?;
        frame.SetSize(bmp.width, bmp.height)?;
        let mut format = GUID_WICPixelFormat32bppPBGRA;
        frame.SetPixelFormat(&mut format)?;
        if format == GUID_WICPixelFormat32bppPBGRA {
            frame.WritePixels(bmp.height, bmp.width * 4, &bmp.pixels)?;
        } else {
            // The encoder wants another layout (JPEG takes 24-bit BGR): convert through WIC.
            let src = source_from_bgra(&f, bmp.width, bmp.height, bmp.width * 4, &bmp.pixels)?;
            let conv = f.CreateFormatConverter()?;
            conv.Initialize(
                &src,
                &format,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )?;
            frame.WriteSource(&conv, std::ptr::null())?;
        }
        frame.Commit()?;
        encoder.Commit()?;
    }
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Decodes any picture into straight RGBA (for showing thumbnails).
pub fn load_rgba(path: &Path, max_w: u32, max_h: u32) -> Result<(u32, u32, Vec<u8>)> {
    let f = factory()?;
    let src = open(&f, path)?;
    let (sw, sh) = size_of_source(&src)?;
    let s = (max_w as f64 / sw as f64)
        .min(max_h as f64 / sh as f64)
        .min(1.0);
    let (w, h) = (
        ((sw as f64 * s).round() as u32).max(1),
        ((sh as f64 * s).round() as u32).max(1),
    );
    let bmp = render_source(&f, &src, Fit::Stretch, w, h)?;
    Ok((w, h, bmp.to_rgba()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32) -> Bitmap {
        let mut b = Bitmap::black(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                // Left half red, right half blue.
                if x < w / 2 {
                    b.pixels[i + 2] = 255;
                } else {
                    b.pixels[i] = 255;
                }
            }
        }
        b
    }

    #[test]
    fn exif_orientations() {
        assert_eq!(orientation_transform(1).0, 0);
        assert_eq!(orientation_transform(6), WICBitmapTransformRotate90);
        assert_eq!(orientation_transform(3), WICBitmapTransformRotate180);
        assert_eq!(orientation_transform(8), WICBitmapTransformRotate270);
    }

    #[test]
    fn renders_and_round_trips_png() {
        crate::mf::init_com_mta();
        let dir = std::env::temp_dir().join(format!("driftwall-wic-{}", std::process::id()));
        let png = dir.join("in.png");
        save_png(&checker(400, 300), &png).unwrap();
        assert_eq!(dimensions(&png).unwrap(), (400, 300));

        // Fit onto a wide canvas: black bars left and right, red then blue in the middle.
        let out = render_file(&png, Fit::Fit, 800, 300).unwrap();
        assert_eq!(out.pixel(10, 150), [0, 0, 0, 255]);
        assert_eq!(out.pixel(250, 150)[2], 255);
        assert_eq!(out.pixel(550, 150)[0], 255);
        assert_eq!(out.pixel(790, 150), [0, 0, 0, 255]);

        // Fill crops instead: no black anywhere.
        let out = render_file(&png, Fit::Fill, 800, 300).unwrap();
        assert_eq!(out.pixel(5, 150)[2], 255);
        assert_eq!(out.pixel(795, 150)[0], 255);

        let jpg = dir.join("out.jpg");
        save_jpeg(&out, &jpg).unwrap();
        let (w, h, rgba) = load_rgba(&jpg, 200, 200).unwrap();
        assert_eq!((w, h), (200, 75));
        assert!(rgba[0] > 200 && rgba[2] < 60, "{:?}", &rgba[..4]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
