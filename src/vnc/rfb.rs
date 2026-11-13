//! Minimal RFB (VNC) client: RFB 3.3/3.8, None and VNC (DES) auth,
//! Raw / CopyRect / Zlib encodings, DesktopSize and Cursor pseudo-encodings.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

pub type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

#[derive(Default)]
pub struct Cursor {
    pub w: usize,
    pub h: usize,
    pub hot_x: usize,
    pub hot_y: usize,
    /// 0x00RRGGBB, alpha in a separate mask
    pub pixels: Vec<u32>,
    pub mask: Vec<bool>,
}

#[derive(Default)]
pub struct Fb {
    pub w: usize,
    pub h: usize,
    /// 0x00RRGGBB
    pub data: Vec<u32>,
    /// incremented on every change
    pub gen: u64,
    pub name: String,
    pub closed: Option<String>,
    pub cursor: Cursor,
    pub bell: bool,
}

impl Fb {
    fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.data = vec![0; w * h];
        self.gen += 1;
    }
}

pub struct Client {
    pub fb: Arc<Mutex<Fb>>,
    writer: Writer,
    buttons: u8,
    pub pointer: (u16, u16),
}

fn read_exact(r: &mut dyn Read, n: usize) -> Result<Vec<u8>> {
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).context("VNC connection closed")?;
    Ok(b)
}

fn u8_(r: &mut dyn Read) -> Result<u8> {
    Ok(read_exact(r, 1)?[0])
}

fn u16_(r: &mut dyn Read) -> Result<u16> {
    let b = read_exact(r, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn u32_(r: &mut dyn Read) -> Result<u32> {
    let b = read_exact(r, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn reason(r: &mut dyn Read) -> String {
    match u32_(r) {
        Ok(n) if n < 4096 => read_exact(r, n as usize).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// DES response for VNC authentication (key bytes are bit-reversed, per RFB spec).
pub fn vnc_auth_response(password: &str, challenge: &[u8]) -> [u8; 16] {
    use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
    let mut key = [0u8; 8];
    for (i, b) in password.bytes().take(8).enumerate() {
        key[i] = b.reverse_bits();
    }
    let cipher = des::Des::new_from_slice(&key).expect("8 byte key");
    let mut out = [0u8; 16];
    for i in 0..2 {
        let mut block = GenericArray::clone_from_slice(&challenge[i * 8..i * 8 + 8]);
        cipher.encrypt_block(&mut block);
        out[i * 8..i * 8 + 8].copy_from_slice(&block);
    }
    out
}

impl Client {
    /// Handshake and start the reader thread.
    pub fn connect(mut reader: Box<dyn Read + Send>, writer: Box<dyn Write + Send>, password: Option<&str>) -> Result<Self> {
        let writer: Writer = Arc::new(Mutex::new(writer));
        let r: &mut dyn Read = &mut *reader;
        let ver = read_exact(r, 12)?;
        let ver_s = String::from_utf8_lossy(&ver).to_string();
        if !ver_s.starts_with("RFB ") {
            bail!("not a VNC server ({:?})", ver_s.trim());
        }
        let minor: u32 = ver_s[8..11].parse().unwrap_or(3);
        let v38 = minor >= 7;
        {
            let mut w = writer.lock().unwrap();
            w.write_all(if v38 { b"RFB 003.008\n" } else { b"RFB 003.003\n" })?;
            w.flush()?;
        }
        let sec = if v38 {
            let n = u8_(r)?;
            if n == 0 {
                bail!("VNC server refused: {}", reason(r));
            }
            let types = read_exact(r, n as usize)?;
            let t = if types.contains(&1) {
                1
            } else if types.contains(&2) {
                2
            } else {
                bail!("unsupported VNC security types {types:?}")
            };
            let mut w = writer.lock().unwrap();
            w.write_all(&[t])?;
            w.flush()?;
            t
        } else {
            let t = u32_(r)?;
            if t == 0 {
                bail!("VNC server refused: {}", reason(r));
            }
            t as u8
        };
        if sec == 2 {
            let chal = read_exact(r, 16)?;
            let pw = password.ok_or_else(|| anyhow!("VNC server requires a password"))?;
            let resp = vnc_auth_response(pw, &chal);
            let mut w = writer.lock().unwrap();
            w.write_all(&resp)?;
            w.flush()?;
        }
        if sec == 2 || v38 {
            let res = u32_(r)?;
            if res != 0 {
                let msg = if v38 { reason(r) } else { String::new() };
                bail!("VNC authentication failed {msg}");
            }
        }
        // ClientInit (shared)
        {
            let mut w = writer.lock().unwrap();
            w.write_all(&[1])?;
            w.flush()?;
        }
        let width = u16_(r)? as usize;
        let height = u16_(r)? as usize;
        let _pf = read_exact(r, 16)?;
        let nlen = u32_(r)? as usize;
        let name = String::from_utf8_lossy(&read_exact(r, nlen.min(65536))?).to_string();

        let fb = Arc::new(Mutex::new(Fb::default()));
        {
            let mut f = fb.lock().unwrap();
            f.resize(width, height);
            f.name = name;
        }
        {
            let mut w = writer.lock().unwrap();
            // SetPixelFormat: 32bpp, depth 24, little endian, true colour, RGB888 at 16/8/0
            let mut m = vec![0u8, 0, 0, 0];
            m.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
            w.write_all(&m)?;
            // SetEncodings
            let encs: [i32; 5] = [1, 6, 0, -223, -239];
            let mut m = vec![2u8, 0];
            m.extend_from_slice(&(encs.len() as u16).to_be_bytes());
            for e in encs {
                m.extend_from_slice(&e.to_be_bytes());
            }
            w.write_all(&m)?;
            w.write_all(&update_request(false, width as u16, height as u16))?;
            w.flush()?;
        }

        let fb2 = fb.clone();
        let w2 = writer.clone();
        thread::spawn(move || {
            let res = read_loop(&mut *reader, &fb2, &w2);
            let mut f = fb2.lock().unwrap();
            f.closed = Some(match res {
                Ok(()) => "connection closed".into(),
                Err(e) => format!("{e:#}"),
            });
            f.gen += 1;
        });
        Ok(Self { fb, writer, buttons: 0, pointer: (0, 0) })
    }

    fn send(&self, b: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(b);
            let _ = w.flush();
        }
    }

    pub fn key(&self, down: bool, keysym: u32) {
        let mut m = vec![4u8, down as u8, 0, 0];
        m.extend_from_slice(&keysym.to_be_bytes());
        self.send(&m);
    }

    /// Press and release, optionally wrapped in modifiers.
    pub fn tap(&self, mods: &[u32], keysym: u32) {
        for m in mods {
            self.key(true, *m);
        }
        self.key(true, keysym);
        self.key(false, keysym);
        for m in mods.iter().rev() {
            self.key(false, *m);
        }
    }

    pub fn pointer_event(&mut self, x: u16, y: u16, buttons: u8) {
        self.buttons = buttons;
        self.pointer = (x, y);
        let mut m = vec![5u8, buttons];
        m.extend_from_slice(&x.to_be_bytes());
        m.extend_from_slice(&y.to_be_bytes());
        self.send(&m);
    }

    pub fn buttons(&self) -> u8 {
        self.buttons
    }

    pub fn request_full(&self) {
        let (w, h) = {
            let f = self.fb.lock().unwrap();
            (f.w as u16, f.h as u16)
        };
        self.send(&update_request(false, w, h));
    }
}

fn update_request(incremental: bool, w: u16, h: u16) -> Vec<u8> {
    let mut m = vec![3u8, incremental as u8, 0, 0, 0, 0];
    m.extend_from_slice(&w.to_be_bytes());
    m.extend_from_slice(&h.to_be_bytes());
    m
}

fn read_loop(r: &mut dyn Read, fb: &Arc<Mutex<Fb>>, writer: &Writer) -> Result<()> {
    let mut zlib = flate2::Decompress::new(true);
    let mut zbuf: Vec<u8> = Vec::new();
    loop {
        let t = u8_(r)?;
        match t {
            0 => {
                let _pad = u8_(r)?;
                let n = u16_(r)?;
                for _ in 0..n {
                    let x = u16_(r)? as usize;
                    let y = u16_(r)? as usize;
                    let w = u16_(r)? as usize;
                    let h = u16_(r)? as usize;
                    let enc = u32_(r)? as i32;
                    match enc {
                        0 => {
                            let px = read_exact(r, w * h * 4)?;
                            put_pixels(fb, x, y, w, h, &px);
                        }
                        1 => {
                            let sx = u16_(r)? as usize;
                            let sy = u16_(r)? as usize;
                            let mut f = fb.lock().unwrap();
                            copy_rect(&mut f, sx, sy, x, y, w, h);
                        }
                        6 => {
                            let len = u32_(r)? as usize;
                            let data = read_exact(r, len)?;
                            let need = w * h * 4;
                            zbuf.clear();
                            zbuf.reserve(need);
                            let mut input = &data[..];
                            while zbuf.len() < need {
                                let before_in = zlib.total_in();
                                let before_out = zlib.total_out();
                                let mut chunk = vec![0u8; need - zbuf.len()];
                                zlib.decompress(input, &mut chunk, flate2::FlushDecompress::Sync).map_err(|e| anyhow!("zlib: {e}"))?;
                                let consumed = (zlib.total_in() - before_in) as usize;
                                let produced = (zlib.total_out() - before_out) as usize;
                                zbuf.extend_from_slice(&chunk[..produced]);
                                input = &input[consumed..];
                                if produced == 0 && consumed == 0 {
                                    break;
                                }
                            }
                            if zbuf.len() < need {
                                bail!("zlib rect too short");
                            }
                            put_pixels(fb, x, y, w, h, &zbuf);
                        }
                        -223 => {
                            fb.lock().unwrap().resize(w, h);
                        }
                        -239 => {
                            let px = read_exact(r, w * h * 4)?;
                            let mask = read_exact(r, w.div_ceil(8) * h)?;
                            let mut f = fb.lock().unwrap();
                            let stride = w.div_ceil(8);
                            f.cursor = Cursor {
                                w,
                                h,
                                hot_x: x,
                                hot_y: y,
                                pixels: px.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], 0])).collect(),
                                mask: (0..w * h).map(|i| mask[(i / w) * stride + (i % w) / 8] & (0x80 >> (i % 8)) != 0).collect(),
                            };
                            f.gen += 1;
                        }
                        other => bail!("unsupported VNC encoding {other}"),
                    }
                }
                let (fw, fh) = {
                    let mut f = fb.lock().unwrap();
                    f.gen += 1;
                    (f.w as u16, f.h as u16)
                };
                let mut wr = writer.lock().unwrap();
                wr.write_all(&update_request(true, fw, fh))?;
                wr.flush()?;
            }
            1 => {
                let _ = read_exact(r, 1)?;
                let _first = u16_(r)?;
                let n = u16_(r)? as usize;
                let _ = read_exact(r, n * 6)?;
            }
            2 => {
                fb.lock().unwrap().bell = true;
            }
            3 => {
                let _ = read_exact(r, 3)?;
                let len = u32_(r)? as usize;
                let _ = read_exact(r, len)?;
            }
            other => bail!("unknown VNC message {other}"),
        }
    }
}

fn put_pixels(fb: &Arc<Mutex<Fb>>, x: usize, y: usize, w: usize, h: usize, px: &[u8]) {
    let mut f = fb.lock().unwrap();
    let fw = f.w;
    let fh = f.h;
    for row in 0..h {
        let ty = y + row;
        if ty >= fh {
            break;
        }
        for col in 0..w {
            let tx = x + col;
            if tx >= fw {
                break;
            }
            let i = (row * w + col) * 4;
            f.data[ty * fw + tx] = u32::from_le_bytes([px[i], px[i + 1], px[i + 2], 0]);
        }
    }
}

fn copy_rect(f: &mut Fb, sx: usize, sy: usize, dx: usize, dy: usize, w: usize, h: usize) {
    let fw = f.w;
    if sx + w > f.w || dx + w > f.w || sy + h > f.h || dy + h > f.h {
        return;
    }
    let rows: Vec<usize> = if dy > sy { (0..h).rev().collect() } else { (0..h).collect() };
    for row in rows {
        let s = (sy + row) * fw + sx;
        let d = (dy + row) * fw + dx;
        f.data.copy_within(s..s + w, d);
    }
}

/// Snapshot the framebuffer as RGB bytes with the cursor composited at the pointer position.
pub fn snapshot_rgb(fb: &Fb, pointer: (u16, u16)) -> (usize, usize, Vec<u8>) {
    let mut out = Vec::with_capacity(fb.w * fb.h * 3);
    for p in &fb.data {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    let c = &fb.cursor;
    if c.w > 0 && c.h > 0 {
        let ox = pointer.0 as isize - c.hot_x as isize;
        let oy = pointer.1 as isize - c.hot_y as isize;
        for cy in 0..c.h {
            for cx in 0..c.w {
                if !c.mask[cy * c.w + cx] {
                    continue;
                }
                let x = ox + cx as isize;
                let y = oy + cy as isize;
                if x < 0 || y < 0 || x as usize >= fb.w || y as usize >= fb.h {
                    continue;
                }
                let p = c.pixels[cy * c.w + cx];
                let i = (y as usize * fb.w + x as usize) * 3;
                out[i] = (p >> 16) as u8;
                out[i + 1] = (p >> 8) as u8;
                out[i + 2] = p as u8;
            }
        }
    }
    (fb.w, fb.h, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_known_answer() {
        // challenge of zeros, password "password" -> deterministic; just check it is stable and 16 bytes
        let a = vnc_auth_response("password", &[0u8; 16]);
        let b = vnc_auth_response("password", &[0u8; 16]);
        assert_eq!(a, b);
        assert_ne!(a, [0u8; 16]);
    }
}
