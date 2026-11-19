//! Just enough PNG to draw on a screenshot: non-interlaced RGB or RGBA at 8
//! or 16 bits in, 8-bit RGB out. The phone's screenshots are one of those;
//! anything else is refused with a reason rather than drawn wrong.

use std::io::{Read, Write};

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Copied to the output: how to read the colours and the pixel density.
/// Without the colour profile a wide-gamut screenshot looks washed out.
const KEPT: [&[u8; 4]; 5] = [b"iCCP", b"sRGB", b"cHRM", b"gAMA", b"pHYs"];

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// 8-bit RGB, row by row.
    pub rgb: Vec<u8>,
    kept: Vec<([u8; 4], Vec<u8>)>,
}

/// A chunk's type and data.
type Chunk<'a> = ([u8; 4], &'a [u8]);

fn chunks(png: &[u8]) -> Result<Vec<Chunk<'_>>, String> {
    if png.get(..8) != Some(SIGNATURE) {
        return Err("not a PNG".into());
    }
    let mut out = Vec::new();
    let mut off = 8;
    while off + 8 <= png.len() {
        let len = u32::from_be_bytes(png[off..off + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = png[off + 4..off + 8].try_into().unwrap();
        let data = png.get(off + 8..off + 8 + len).ok_or("the PNG is cut short")?;
        out.push((kind, data));
        if &kind == b"IEND" {
            break;
        }
        off += 12 + len;
    }
    Ok(out)
}

/// Width and height from the header, without decoding the pixels.
pub fn size(png: &[u8]) -> Option<(u32, u32)> {
    let (kind, d) = chunks(png).ok()?.into_iter().next()?;
    (&kind == b"IHDR" && d.len() >= 8).then(|| {
        (u32::from_be_bytes(d[0..4].try_into().unwrap()), u32::from_be_bytes(d[4..8].try_into().unwrap()))
    })
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = ((p - i16::from(a)).abs(), (p - i16::from(b)).abs(), (p - i16::from(c)).abs());
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

pub fn decode(png: &[u8]) -> Result<Image, String> {
    let all = chunks(png)?;
    let header = all.iter().find(|(k, _)| k == b"IHDR").map(|(_, d)| *d).filter(|d| d.len() == 13).ok_or("the PNG has no header")?;
    let width = u32::from_be_bytes(header[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
    let (depth, colour, interlace) = (header[8], header[9], header[12]);
    let channels = match colour {
        2 => 3,
        6 => 4,
        other => return Err(format!("PNG colour type {other} (only RGB and RGBA are supported)")),
    };
    if depth != 8 && depth != 16 {
        return Err(format!("PNG bit depth {depth} (only 8 and 16 are supported)"));
    }
    if interlace != 0 {
        return Err("interlaced PNG".into());
    }
    let step = usize::from(depth / 8);
    let bpp = channels * step;
    let (w, h) = (width as usize, height as usize);
    let stride = w.checked_mul(bpp).ok_or("the PNG is too large")?;
    let raw_len = (stride + 1).checked_mul(h).ok_or("the PNG is too large")?;
    let compressed: Vec<u8> = all.iter().filter(|(k, _)| k == b"IDAT").flat_map(|(_, d)| d.iter().copied()).collect();
    // Grown as the pixels come, never past the size the header gives.
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&compressed[..])
        .take(raw_len as u64)
        .read_to_end(&mut raw)
        .map_err(|e| format!("the PNG's pixels do not decompress: {e}"))?;
    if raw.len() < raw_len {
        return Err("the PNG has fewer pixels than its size says".into());
    }

    let mut rgb = Vec::with_capacity(w * h * 3);
    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    for line in raw.chunks_exact(stride + 1).take(h) {
        let (filter, data) = (line[0], &line[1..]);
        for x in 0..stride {
            let a = if x >= bpp { cur[x - bpp] } else { 0 };
            let b = prev[x];
            let c = if x >= bpp { prev[x - bpp] } else { 0 };
            let predicted = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => paeth(a, b, c),
                other => return Err(format!("PNG row filter {other}")),
            };
            cur[x] = data[x].wrapping_add(predicted);
        }
        // The high byte of a 16-bit sample is its 8-bit value.
        for px in cur.chunks_exact(bpp) {
            rgb.extend_from_slice(&[px[0], px[step], px[2 * step]]);
        }
        std::mem::swap(&mut cur, &mut prev);
    }
    let kept = all
        .iter()
        .filter(|(k, _)| KEPT.contains(&k))
        .map(|(k, d)| (*k, d.to_vec()))
        .collect();
    Ok(Image { width, height, rgb, kept })
}

fn crc(kind: &[u8], data: &[u8]) -> u32 {
    let mut c = flate2::Crc::new();
    c.update(kind);
    c.update(data);
    c.sum()
}

pub fn encode(image: &Image) -> Vec<u8> {
    let stride = image.width as usize * 3;
    // Each row as differences from the pixel to its left (filter 1).
    let mut raw = Vec::with_capacity((stride + 1) * image.height as usize);
    for row in image.rgb.chunks_exact(stride) {
        raw.push(1);
        raw.extend_from_slice(&row[..3.min(row.len())]);
        raw.extend(row.iter().skip(3).zip(row).map(|(x, left)| x.wrapping_sub(*left)));
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw).expect("writing to memory");
    let idat = z.finish().expect("writing to memory");

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&image.width.to_be_bytes());
    header.extend_from_slice(&image.height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut list: Vec<(&[u8], &[u8])> = vec![(b"IHDR", &header)];
    list.extend(image.kept.iter().map(|(k, d)| (&k[..], &d[..])));
    list.push((b"IDAT", &idat));
    list.push((b"IEND", &[]));
    let mut out = SIGNATURE.to_vec();
    for (kind, data) in list {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc(kind, data).to_be_bytes());
    }
    out
}

impl Image {
    #[cfg(test)]
    pub fn new(width: u32, height: u32, rgb: Vec<u8>) -> Self {
        Self { width, height, rgb, kept: Vec::new() }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }

    /// Blend `rgb` over the rectangle, clipped to the image. `alpha` 255 paints.
    pub fn fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, rgb: [u8; 3], alpha: u32) {
        let (w, h) = (i64::from(self.width), i64::from(self.height));
        for y in y0.max(0)..y1.min(h) {
            for x in x0.max(0)..x1.min(w) {
                let i = (y as usize * self.width as usize + x as usize) * 3;
                for k in 0..3 {
                    let old = u32::from(self.rgb[i + k]);
                    self.rgb[i + k] = ((old * (255 - alpha) + u32::from(rgb[k]) * alpha) / 255) as u8;
                }
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A PNG as the phone writes one, at either depth, with every row filter
    /// used in turn so the decoder's unfiltering is checked too.
    pub fn fixture(width: u32, height: u32, depth: u8, alpha: bool, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
        let channels = if alpha { 4 } else { 3 };
        let step = usize::from(depth / 8);
        let bpp = channels * step;
        let mut rows: Vec<Vec<u8>> = Vec::new();
        for y in 0..height {
            let mut row = Vec::new();
            for x in 0..width {
                let p = pixel(x, y);
                for v in p.iter().chain(alpha.then_some(&255)) {
                    row.push(*v);
                    if step == 2 {
                        row.push(v.wrapping_mul(7));
                    }
                }
            }
            rows.push(row);
        }
        let mut raw = Vec::new();
        let zero = vec![0u8; rows[0].len()];
        for (y, row) in rows.iter().enumerate() {
            let up = if y == 0 { &zero } else { &rows[y - 1] };
            let filter = (y % 5) as u8;
            raw.push(filter);
            for x in 0..row.len() {
                let a = if x >= bpp { row[x - bpp] } else { 0 };
                let b = up[x];
                let c = if x >= bpp { up[x - bpp] } else { 0 };
                let predicted = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                    _ => paeth(a, b, c),
                };
                raw.push(row[x].wrapping_sub(predicted));
            }
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw).unwrap();
        let mut header = width.to_be_bytes().to_vec();
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[depth, if alpha { 6 } else { 2 }, 0, 0, 0]);
        let image = Image {
            width,
            height,
            rgb: Vec::new(),
            kept: vec![(*b"iCCP", b"Display P3\0\0profile".to_vec()), (*b"pHYs", vec![0, 0, 0x16, 0x25, 0, 0, 0x16, 0x25, 1])],
        };
        let mut out = SIGNATURE.to_vec();
        let mut list: Vec<([u8; 4], Vec<u8>)> = vec![(*b"IHDR", header)];
        list.extend(image.kept.clone());
        list.push((*b"tEXt", b"Software\0iOS".to_vec()));
        list.push((*b"IDAT", z.finish().unwrap()));
        list.push((*b"IEND", vec![]));
        for (kind, data) in list {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(&kind);
            out.extend_from_slice(&data);
            out.extend_from_slice(&crc(&kind, &data).to_be_bytes());
        }
        out
    }

    fn colour(x: u32, y: u32) -> [u8; 3] {
        [(x * 37 + y) as u8, (y * 53) as u8, (x ^ y) as u8 * 3]
    }

    #[test]
    fn eight_and_sixteen_bit_screenshots_decode_and_survive_a_round_trip() {
        for (depth, alpha) in [(8, false), (8, true), (16, false), (16, true)] {
            let png = fixture(13, 11, depth, alpha, colour);
            assert_eq!(size(&png), Some((13, 11)));
            let image = decode(&png).unwrap_or_else(|e| panic!("{depth}-bit alpha={alpha}: {e}"));
            for (x, y) in [(0, 0), (12, 0), (5, 7), (12, 10)] {
                assert_eq!(image.pixel(x, y), colour(x, y), "{depth}-bit alpha={alpha} at {x},{y}");
            }
            let again = decode(&encode(&image)).unwrap();
            assert_eq!(again, image, "{depth}-bit alpha={alpha}: pixels and the colour profile come back");
            assert!(again.kept.iter().any(|(k, _)| k == b"iCCP"));
            assert!(!again.kept.iter().any(|(k, _)| k == b"tEXt"));
        }
    }

    #[test]
    fn other_pngs_are_refused_with_the_reason() {
        let mut grey = fixture(2, 2, 8, false, colour);
        grey[8 + 8 + 9] = 0;
        assert!(decode(&grey).unwrap_err().contains("colour type 0"));
        let mut interlaced = fixture(2, 2, 8, false, colour);
        interlaced[8 + 8 + 12] = 1;
        assert_eq!(decode(&interlaced).unwrap_err(), "interlaced PNG");
        assert_eq!(decode(b"GIF89a").unwrap_err(), "not a PNG");
        let png = fixture(4, 4, 8, false, colour);
        assert!(decode(&png[..png.len() / 2]).is_err());
        let mut huge = fixture(2, 2, 16, true, colour);
        huge[16..24].copy_from_slice(&[0x7f, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff]);
        assert!(decode(&huge).is_err(), "a size the pixels cannot fill is refused, not allocated");
    }
}
