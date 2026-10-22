//! `driftwall --self-test`: checks the pieces that don't need a desktop session, without
//! opening any window. Exit code 0 when everything required works.

use std::time::Duration;

use crate::model::Fit;
use crate::{desktop, gpu, mf, player, wic};

pub fn run() -> i32 {
    let mut failed = 0;
    let mut check = |name: &str, r: anyhow::Result<String>| match r {
        Ok(detail) => println!("ok    {name}: {detail}"),
        Err(e) => {
            println!("FAIL  {name}: {e:#}");
            failed += 1;
        }
    };
    mf::init_com_mta();

    check(
        "pictures",
        (|| {
            let dir =
                std::env::temp_dir().join(format!("driftwall-selftest-{}", std::process::id()));
            let mut bmp = wic::Bitmap::black(64, 36);
            for px in bmp.pixels.chunks_exact_mut(4) {
                px[1] = 200;
            }
            let png = dir.join("t.png");
            wic::save_png(&bmp, &png)?;
            let out = wic::render_file(&png, Fit::Fit, 128, 128)?;
            let _ = std::fs::remove_dir_all(&dir);
            anyhow::ensure!(
                out.pixel(64, 64)[1] == 200 && out.pixel(64, 4)[1] == 0,
                "unexpected pixels"
            );
            Ok("decode, fit and PNG round trip".into())
        })(),
    );

    let device = gpu::default_device();
    check(
        "graphics",
        device
            .as_ref()
            .map(|g| g.adapter_name.clone())
            .map_err(|e| anyhow::anyhow!("{e:#}")),
    );

    // Pictures work without Media Foundation (Windows N, some servers); report, don't fail.
    match mf::init_thread() {
        Ok(()) => println!("ok    media foundation: available"),
        Err(e) => println!("info  media foundation: {e:#}"),
    }

    // Software-only adapters (CI machines) have no video decoder.
    if let Ok(g) = &device
        && mf::available()
        && !g.adapter_name.contains("Basic Render")
        && let Some(sample) = std::env::args().skip_while(|a| a != "--video").nth(1)
    {
        check(
            "video decode",
            (|| {
                let px = player::decode_offscreen(
                    g,
                    std::path::Path::new(&sample),
                    320,
                    180,
                    Duration::from_millis(1200),
                )?;
                anyhow::ensure!(!px.is_empty(), "no frames");
                Ok(format!("{} frames sampled", px.len()))
            })(),
        );
    }

    // Informational: CI machines have no interactive desktop.
    match desktop::displays() {
        Ok(d) => println!(
            "info  displays: {}",
            d.iter()
                .map(|d| format!("{} {}", d.name, d.size_label()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(e) => println!("info  displays: {e:#}"),
    }
    match desktop::find_host() {
        Ok(h) => println!("info  desktop window: {:?}", h.kind),
        Err(e) => println!("info  desktop window: {e:#}"),
    }
    if failed == 0 {
        println!("self-test passed");
        0
    } else {
        println!("self-test failed ({failed})");
        1
    }
}
