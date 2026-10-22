//! Reading video files with the Media Foundation source reader: size, length, frame rate and
//! codec on import, and single frames for thumbnails and the still desktop picture.

use std::mem::ManuallyDrop;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
    MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFVideoFormat_AV1,
    MFVideoFormat_H264, MFVideoFormat_H265, MFVideoFormat_HEVC, MFVideoFormat_MJPG,
    MFVideoFormat_MP4V, MFVideoFormat_RGB32, MFVideoFormat_VP80, MFVideoFormat_VP90,
    MFVideoFormat_WMV3, MFVideoFormat_WVC1,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0, PropVariantClear,
    PropVariantToUInt64,
};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HSTRING};

use crate::mf;
use crate::wic::Bitmap;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub fps: f32,
    pub codec: String,
    pub has_audio: bool,
}

pub fn codec_name(subtype: &GUID) -> String {
    let known = [
        (MFVideoFormat_H264, "H.264"),
        (MFVideoFormat_HEVC, "HEVC"),
        (MFVideoFormat_H265, "HEVC"),
        (MFVideoFormat_AV1, "AV1"),
        (MFVideoFormat_VP90, "VP9"),
        (MFVideoFormat_VP80, "VP8"),
        (MFVideoFormat_WVC1, "VC-1"),
        (MFVideoFormat_WMV3, "WMV"),
        (MFVideoFormat_MP4V, "MPEG-4"),
        (MFVideoFormat_MJPG, "MJPEG"),
    ];
    known.iter().find(|(g, _)| g == subtype).map_or_else(
        || {
            // FourCC subtypes keep the code in the first four bytes.
            let b = subtype.data1.to_le_bytes();
            if b.iter().all(|c| c.is_ascii_alphanumeric()) {
                String::from_utf8_lossy(&b).trim().to_string()
            } else {
                "video".to_string()
            }
        },
        |(_, n)| n.to_string(),
    )
}

fn reader(path: &Path) -> Result<IMFSourceReader> {
    mf::init_thread()?;
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.context("attributes")?;
        // Lets the reader convert decoded frames to RGB32 for thumbnails.
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)
            .with_context(|| format!("Windows can't open {} as a video", path.display()))
    }
}

const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn rgb32_output(r: &IMFSourceReader) -> Result<()> {
    unsafe {
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        r.SetStreamSelection(VIDEO, true)?;
        let t: IMFMediaType = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        r.SetCurrentMediaType(VIDEO, None, &t).context(
            "Windows has no decoder for this video's format. HEVC, AV1 and VP9 need their free extensions from the Microsoft Store",
        )?;
    }
    Ok(())
}

/// Reads a video's properties and checks that Windows can decode it.
pub fn probe(path: &Path) -> Result<VideoInfo> {
    let r = reader(path)?;
    let mut info = VideoInfo::default();
    unsafe {
        let native = r
            .GetNativeMediaType(VIDEO, 0)
            .context("the file has no video track")?;
        info.codec = codec_name(&native.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default());
        let (w, h) = mf::unpack(
            native
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .context("unknown frame size")?,
        );
        (info.width, info.height) = (w, h);
        if let Ok(rate) = native.GetUINT64(&MF_MT_FRAME_RATE) {
            let (n, d) = mf::unpack(rate);
            if d > 0 {
                info.fps = n as f32 / d as f32;
            }
        }
        info.has_audio = r
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, 0)
            .is_ok();
        if let Ok(mut pv) =
            r.GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
        {
            info.duration_ms = PropVariantToUInt64(&pv).unwrap_or(0) / 10_000;
            let _ = PropVariantClear(&mut pv);
        }
    }
    rgb32_output(&r)?;
    if info.width == 0 || info.height == 0 {
        bail!("the video has no picture size");
    }
    Ok(info)
}

fn position(hns: i64) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: hns },
            }),
        },
    }
}

/// Decodes the frame shown at `at_ms` (the first frame for 0) at the video's own size.
pub fn frame_at(path: &Path, at_ms: u64) -> Result<Bitmap> {
    let r = reader(path)?;
    rgb32_output(&r)?;
    let target = at_ms as i64 * 10_000;
    unsafe {
        if target > 0 {
            let pv = position(target);
            r.SetCurrentPosition(&GUID::zeroed(), &pv)?;
        }
        let current = r.GetCurrentMediaType(VIDEO)?;
        let (w, h) = mf::unpack(current.GetUINT64(&MF_MT_FRAME_SIZE)?);
        let stride = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|s| s as i32)
            .unwrap_or(w as i32 * 4);
        let mut best: Option<IMFSample> = None;
        for _ in 0..600 {
            let (mut flags, mut ts) = (0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            r.ReadSample(
                VIDEO,
                0,
                None,
                Some(&mut flags),
                Some(&mut ts),
                Some(&mut sample),
            )?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(s) = sample {
                best = Some(s);
                // Seeking lands on the keyframe before the target; read up to it.
                if ts + 10_000 >= target {
                    break;
                }
            }
        }
        let sample = best.context("the video has no frames")?;
        let buf = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize);
        let mut out = Bitmap::black(w, h);
        let row = w as usize * 4;
        let pitch = stride.unsigned_abs() as usize;
        for y in 0..h as usize {
            // A negative stride means the rows are stored bottom-up.
            let src_y = if stride < 0 { h as usize - 1 - y } else { y };
            let start = src_y * pitch;
            if start + row > data.len() {
                break;
            }
            let dst = &mut out.pixels[y * row..(y + 1) * row];
            dst.copy_from_slice(&data[start..start + row]);
        }
        buf.Unlock()?;
        for px in out.pixels.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_codecs() {
        assert_eq!(codec_name(&MFVideoFormat_H264), "H.264");
        assert_eq!(codec_name(&MFVideoFormat_VP90), "VP9");
        assert_eq!(codec_name(&GUID::zeroed()), "video");
    }
}
