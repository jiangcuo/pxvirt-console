//! Probe ratatui-image for the "VM graphical console over SSH" idea.
//!
//!   term-image-probe bench   encode cost + bytes per frame for every protocol (no terminal needed)
//!   term-image-probe live    detect the real terminal's protocol and animate a fake VM screen

use anyhow::Result;
use image::{DynamicImage, Rgb, RgbImage};
use ratatui::{backend::TestBackend, layout::Rect, layout::Size, Terminal};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    Image, Resize,
};
use std::time::{Duration, Instant};

/// Fake VM screen: dark background, rows of "text" glyph blocks like a BIOS / installer,
/// a gradient area, and a moving box so consecutive frames differ.
fn fake_frame(w: u32, h: u32, t: u32) -> DynamicImage {
    let mut img = RgbImage::from_pixel(w, h, Rgb([0, 0, 40]));
    // text rows: 8x16 cells, pseudo-random glyph pixels
    for row in 0..(h / 16) {
        let cols = (row * 37 % 80).max(20);
        for col in 0..cols.min(w / 8) {
            for y in 2..14 {
                for x in 1..7 {
                    if (x * 7 + y * 13 + col * 31 + row * 17) % 5 < 2 {
                        img.put_pixel(col * 8 + x, row * 16 + y, Rgb([200, 200, 200]));
                    }
                }
            }
        }
    }
    for y in h * 2 / 3..h {
        for x in w / 2..w {
            img.put_pixel(x, y, Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 128]));
        }
    }
    let bx = (t * 23) % (w - 100);
    for y in 100..180 {
        for x in bx..bx + 100 {
            img.put_pixel(x, y, Rgb([220, 60, 60]));
        }
    }
    DynamicImage::ImageRgb8(img)
}

fn buffer_bytes(term: &Terminal<TestBackend>) -> usize {
    term.backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol().len())
        .sum()
}

fn bench() -> Result<()> {
    let protocols = [
        ProtocolType::Halfblocks,
        ProtocolType::Sixel,
        ProtocolType::Kitty,
        ProtocolType::Iterm2,
    ];
    // (vm resolution, terminal cells, font px)
    let cases = [
        ((1024, 768), (200u16, 60u16), (10u16, 20u16)),
        ((800, 600), (120, 40), (9, 18)),
        ((1024, 768), (80, 24), (9, 18)),
    ];
    println!(
        "{:<11} {:>9} {:>9} {:>11} {:>12} {:>11}",
        "protocol", "vm res", "cells", "encode ms", "KiB/frame", "Mbit@10fps"
    );
    for ((vw, vh), (cw, ch), font) in cases {
        for p in protocols {
            let mut picker = Picker::from_fontsize(font.into());
            picker.set_protocol_type(p);
            let mut term = Terminal::new(TestBackend::new(cw, ch))?;
            let n = 10;
            let mut total = Duration::ZERO;
            let mut bytes = 0;
            for t in 0..n {
                let frame = fake_frame(vw, vh, t);
                let start = Instant::now();
                let proto = picker.new_protocol(frame, Size::new(cw, ch), Resize::Fit(None))?;
                term.draw(|f| f.render_widget(Image::new(&proto), f.area()))?;
                total += start.elapsed();
                bytes = buffer_bytes(&term);
            }
            let ms = total.as_secs_f64() * 1000.0 / n as f64;
            println!(
                "{:<11} {:>9} {:>9} {:>11.1} {:>12.0} {:>11.1}",
                format!("{p:?}"),
                format!("{vw}x{vh}"),
                format!("{cw}x{ch}"),
                ms,
                bytes as f64 / 1024.0,
                bytes as f64 * 8.0 * 10.0 / 1e6
            );
        }
        println!();
    }
    Ok(())
}

fn live() -> Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    let mut terminal = ratatui::init();
    let picker = Picker::from_query_stdio();
    let mut report = String::new();
    let result = (|| -> Result<()> {
        let mut picker = match picker {
            Ok(p) => p,
            Err(e) => {
                report.push_str(&format!("query failed: {e:?}, falling back to halfblocks\n"));
                Picker::halfblocks()
            }
        };
        report.push_str(&format!(
            "TERM={} TERM_PROGRAM={} SSH={}\n",
            std::env::var("TERM").unwrap_or_default(),
            std::env::var("TERM_PROGRAM").unwrap_or_default(),
            std::env::var("SSH_CONNECTION").is_ok()
        ));
        report.push_str(&format!(
            "detected protocol={:?} font={:?} tmux={} caps={:?}\n",
            picker.protocol_type(),
            (picker.font_size().width, picker.font_size().height),
            picker.tmux_detected(),
            picker.capabilities()
        ));
        let detected = picker.protocol_type();
        // Run the detected protocol, then halfblocks, 5 seconds each. 'q' skips.
        for p in [detected, ProtocolType::Halfblocks] {
            picker.set_protocol_type(p);
            let start = Instant::now();
            let mut frames = 0u32;
            while start.elapsed() < Duration::from_secs(5) {
                let area = terminal.size()?;
                let img_area = Rect::new(0, 1, area.width, area.height.saturating_sub(1));
                let proto = picker.new_protocol(
                    fake_frame(1024, 768, frames),
                    Size::new(img_area.width, img_area.height),
                    Resize::Fit(None),
                )?;
                terminal.draw(|f| {
                    f.render_widget(
                        ratatui::text::Line::from(format!(
                            "{p:?}  frame {frames}  {:.1} fps   (q = next)",
                            frames as f64 / start.elapsed().as_secs_f64().max(0.001)
                        )),
                        Rect::new(0, 0, area.width, 1),
                    );
                    f.render_widget(Image::new(&proto), img_area);
                })?;
                frames += 1;
                if event::poll(Duration::from_millis(1))? {
                    if let Event::Key(k) = event::read()? {
                        if k.code == KeyCode::Char('q') {
                            break;
                        }
                    }
                }
            }
            report.push_str(&format!(
                "{p:?}: {frames} frames, {:.1} fps\n",
                frames as f64 / start.elapsed().as_secs_f64()
            ));
        }
        Ok(())
    })();
    ratatui::restore();
    print!("{report}");
    std::fs::write("/tmp/term-image-probe.txt", &report)?;
    result
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("live") => live(),
        _ => bench(),
    }
}
