//! Screen capture with `grim` and the in-process image pipeline that replaces
//! `sharp`.
//!
//! `grim` is asked for raw PPM (`-t ppm`), not PNG: the frame is read straight
//! into memory with no PNG decode, and its size is known from the header. Each
//! frame is resampled (area average, never enlarged), encoded, and dropped.
//!
//! - Agent images: lossless WebP when it fits the byte budget, else JPEG at
//!   falling quality, else a smaller size.
//! - Operator previews: JPEG for a console that asks for it (its own previews
//!   directory holds raw PPM, so the shell never decodes JPEG), within 480×270
//!   / 256 KiB (`tile`) or 1280×720 / 1 MiB (`selected`). A console from before
//!   gets PNG: truecolour first, then a 256-colour palette, which always fits (at
//!   most one byte per pixel).
//! - Region hash: FNV-1a 32 over the raw RGB bytes of a region, the same pixel
//!   format on both sides of a comparison.

use super::hyprland::Rect;
use super::run::{Cancel, Cmd, clip, run};
use crate::error::{IbaraError, Result, internal, invalid};
use image::ExtendedColorType;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// Largest frame accepted before any processing (16 MP, §11.1).
pub const MAX_PIXELS: u64 = 16_000_000;
/// Output bound for one `grim` frame: a 16 MP RGB frame plus its header.
const MAX_FRAME_BYTES: usize = 50 * 1024 * 1024;
/// Images are not shrunk below this edge to meet a byte budget.
const MIN_EDGE: u32 = 64;

/// A raw RGB frame from `grim -t ppm`.
pub struct Frame {
    bytes: Vec<u8>,
    offset: usize,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    /// Tightly packed RGB8 rows.
    pub fn pixels(&self) -> &[u8] {
        &self.bytes[self.offset..self.offset + self.width as usize * self.height as usize * 3]
    }
}

fn not_ppm() -> IbaraError {
    IbaraError::new("CAPABILITY_UNAVAILABLE", "Capture was not a PPM frame.", true)
}

/// Parse a binary PPM (`P6`, maxval 255). The geometry is checked against
/// [`MAX_PIXELS`] before the pixels are used.
pub fn parse_ppm(bytes: Vec<u8>) -> Result<Frame> {
    if !bytes.starts_with(b"P6") {
        return Err(not_ppm());
    }
    let mut pos = 2;
    let mut fields = [0u64; 3];
    for field in &mut fields {
        loop {
            match bytes.get(pos) {
                Some(b) if b.is_ascii_whitespace() => pos += 1,
                Some(b'#') => {
                    while bytes.get(pos).is_some_and(|b| *b != b'\n') {
                        pos += 1;
                    }
                }
                _ => break,
            }
        }
        let start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == start || pos - start > 9 {
            return Err(not_ppm());
        }
        *field = std::str::from_utf8(&bytes[start..pos]).ok().and_then(|s| s.parse().ok()).ok_or_else(not_ppm)?;
    }
    if !bytes.get(pos).is_some_and(u8::is_ascii_whitespace) {
        return Err(not_ppm());
    }
    pos += 1;
    let [width, height, maxval] = fields;
    if maxval != 255 || width == 0 || height == 0 {
        return Err(not_ppm());
    }
    if width * height > MAX_PIXELS {
        return Err(IbaraError::new("BUDGET_EXCEEDED", "Capture exceeds the 16 MP geometry limit.", true));
    }
    if ((bytes.len() - pos) as u64) < width * height * 3 {
        return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Capture ended early.", true));
    }
    Ok(Frame { bytes, offset: pos, width: width as u32, height: height as u32 })
}

/// Decode a compositor's PNG with the same geometry bound as the grim path.
pub fn from_png(bytes: Vec<u8>) -> Result<Frame> {
    let reader = image::ImageReader::with_format(std::io::Cursor::new(&bytes), image::ImageFormat::Png);
    let (width, height) = reader.into_dimensions().map_err(|_| not_ppm())?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(IbaraError::new("BUDGET_EXCEEDED", "Capture exceeds the 16 MP geometry limit.", true));
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let rgb = reader.decode().map_err(|_| not_ppm())?.into_rgb8();
    Ok(Frame { bytes: rgb.into_raw(), offset: 0, width, height })
}

impl Frame {
    pub fn crop(self, x: u32, y: u32, width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || x.checked_add(width).is_none_or(|end| end > self.width)
            || y.checked_add(height).is_none_or(|end| end > self.height) {
            return Err(invalid("Capture region is outside the output."));
        }
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 3);
        for row in y..y + height {
            let start = (row as usize * self.width as usize + x as usize) * 3;
            bytes.extend_from_slice(&self.pixels()[start..start + width as usize * 3]);
        }
        Ok(Frame { bytes, offset: 0, width, height })
    }
}

/// What to capture.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// A whole output, at its native pixel size.
    Output(&'a str),
    /// A rectangle in logical layout coordinates (at the greatest output scale).
    Region(Rect),
}

/// `grim -t ppm (-o <output> | -g "x,y wxh") -`.
pub async fn grab(
    grim: &Path,
    env: &[(OsString, OsString)],
    source: Source<'_>,
    expected_pixels: u64,
    timeout: Duration,
    cancel: Option<&Cancel>,
) -> Result<Frame> {
    let mut cmd = Cmd::new(grim).args(["-t", "ppm"]);
    match source {
        Source::Output(name) => {
            if name.is_empty() || name.chars().count() > 128 || name.starts_with('-') {
                return Err(invalid("Invalid output name.").with("field", "display"));
            }
            cmd = cmd.arg("-o").arg(name);
        }
        Source::Region(r) => {
            if r.width < 1 || r.height < 1 {
                return Err(invalid("Capture region is empty."));
            }
            cmd = cmd.arg("-g").arg(format!("{},{} {}x{}", r.x, r.y, r.width, r.height));
        }
    }
    let expect = (expected_pixels.min(MAX_PIXELS) * 3 + 64) as usize;
    cmd = cmd.arg("-").envs(env).timeout(timeout).max_output(MAX_FRAME_BYTES).expect_output(expect);
    if let Some(cancel) = cancel {
        cmd = cmd.cancel(cancel);
    }
    let out = run(cmd).await?;
    if !out.success() || out.stdout.len() < 8 {
        return Err(IbaraError::new("CAPABILITY_UNAVAILABLE", "Screen capture failed.", true)
            .with("detail", clip(out.stderr_text().trim(), 240)));
    }
    parse_ppm(out.stdout)
}

/// The largest size inside `max_w`×`max_h` with the source aspect ratio,
/// never larger than the source ("fit inside, without enlargement").
pub fn fit_inside(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if width <= max_w && height <= max_h {
        return (width, height);
    }
    let scale = (max_w as f64 / width as f64).min(max_h as f64 / height as f64);
    let w = ((width as f64 * scale).round() as u32).clamp(1, max_w.max(1));
    let h = ((height as f64 * scale).round() as u32).clamp(1, max_h.max(1));
    (w, h)
}

/// Area-average weights mapping `dst` samples onto `src` samples (`dst <= src`).
struct Taps {
    first: Vec<u32>,
    offsets: Vec<u32>,
    weights: Vec<f32>,
}

impl Taps {
    fn new(src: u32, dst: u32) -> Taps {
        let scale = src as f64 / dst as f64;
        let mut taps = Taps { first: Vec::with_capacity(dst as usize), offsets: Vec::with_capacity(dst as usize + 1), weights: Vec::new() };
        taps.offsets.push(0);
        for i in 0..dst {
            let a = i as f64 * scale;
            let b = (a + scale).min(src as f64);
            let s0 = a.floor() as u32;
            let s1 = (b.ceil() as u32).min(src).max(s0 + 1);
            taps.first.push(s0);
            for s in s0..s1 {
                let covered = b.min(s as f64 + 1.0) - a.max(s as f64);
                taps.weights.push((covered.max(0.0) / scale) as f32);
            }
            taps.offsets.push(taps.weights.len() as u32);
        }
        taps
    }
    fn of(&self, i: u32) -> (u32, &[f32]) {
        let (lo, hi) = (self.offsets[i as usize] as usize, self.offsets[i as usize + 1] as usize);
        (self.first[i as usize], &self.weights[lo..hi])
    }
}

/// Downscale packed RGB8 by area averaging. Only a row accumulator is kept.
pub fn resample(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }
    let (dw, dh) = (dw.min(sw).max(1), dh.min(sh).max(1));
    let xt = Taps::new(sw, dw);
    let yt = Taps::new(sh, dh);
    let row_len = sw as usize * 3;
    let mut out = vec![0u8; dw as usize * dh as usize * 3];
    let mut acc = vec![0f32; dw as usize * 3];
    for dy in 0..dh {
        acc.fill(0.0);
        let (sy0, wys) = yt.of(dy);
        for (k, &wy) in wys.iter().enumerate() {
            let line = &src[(sy0 as usize + k) * row_len..][..row_len];
            for dx in 0..dw {
                let (sx0, wxs) = xt.of(dx);
                let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
                for (j, &wx) in wxs.iter().enumerate() {
                    let p = (sx0 as usize + j) * 3;
                    r += line[p] as f32 * wx;
                    g += line[p + 1] as f32 * wx;
                    b += line[p + 2] as f32 * wx;
                }
                let a = dx as usize * 3;
                acc[a] += r * wy;
                acc[a + 1] += g * wy;
                acc[a + 2] += b * wy;
            }
        }
        let dst = &mut out[dy as usize * dw as usize * 3..][..dw as usize * 3];
        for (d, a) in dst.iter_mut().zip(&acc) {
            *d = (a + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Webp,
    Jpeg,
    Png,
}

impl ImageFormat {
    pub fn mime_type(self) -> &'static str {
        match self {
            ImageFormat::Webp => "image/webp",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
}

/// Size limits for one agent image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudget {
    pub max_bytes: usize,
    pub max_width: u32,
    pub max_height: u32,
}

impl Default for ImageBudget {
    fn default() -> Self {
        ImageBudget { max_bytes: 256 * 1024, max_width: 1600, max_height: 1600 }
    }
}

/// Encoded pixels.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

fn encode_failed(e: impl std::fmt::Display) -> IbaraError {
    internal(format!("Image encoding failed: {e}"))
}

fn encode_webp(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(rgb, w, h, ExtendedColorType::Rgb8)
        .map_err(encode_failed)?;
    Ok(out)
}

fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode(rgb, w, h, ExtendedColorType::Rgb8)
        .map_err(encode_failed)?;
    Ok(out)
}

fn encode_png(data: &[u8], w: u32, h: u32, palette: Option<&[u8]>, compression: png::Compression) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(compression);
        match palette {
            Some(palette) => {
                encoder.set_color(png::ColorType::Indexed);
                encoder.set_palette(palette);
            }
            None => encoder.set_color(png::ColorType::Rgb),
        }
        let mut writer = encoder.write_header().map_err(encode_failed)?;
        writer.write_image_data(data).map_err(encode_failed)?;
        writer.finish().map_err(encode_failed)?;
    }
    Ok(out)
}

/// Encode RGB8 for an agent within `budget`: lossless WebP if it fits, else
/// JPEG at quality 80, 65, 50, else three-quarter size and again, down to a
/// 64-pixel edge (`BUDGET_EXCEEDED` after that).
pub fn encode_within(src: &[u8], sw: u32, sh: u32, budget: &ImageBudget) -> Result<Encoded> {
    if budget.max_bytes < 1024 || budget.max_width < 1 || budget.max_height < 1 {
        return Err(invalid("Image budget is too small."));
    }
    let (mut w, mut h) = fit_inside(sw, sh, budget.max_width, budget.max_height);
    let mut try_webp = true;
    loop {
        let scaled;
        let rgb: &[u8] = if (w, h) == (sw, sh) {
            src
        } else {
            scaled = resample(src, sw, sh, w, h);
            &scaled
        };
        if try_webp {
            let webp = encode_webp(rgb, w, h)?;
            if webp.len() <= budget.max_bytes {
                return Ok(Encoded { bytes: webp, format: ImageFormat::Webp, width: w, height: h });
            }
            // Lossless size falls roughly with area; far over budget, stop trying it.
            try_webp = webp.len() <= budget.max_bytes * 2;
        }
        for quality in [80, 65, 50] {
            let jpeg = encode_jpeg(rgb, w, h, quality)?;
            if jpeg.len() <= budget.max_bytes {
                return Ok(Encoded { bytes: jpeg, format: ImageFormat::Jpeg, width: w, height: h });
            }
        }
        if w <= MIN_EDGE && h <= MIN_EDGE {
            return Err(IbaraError::new("BUDGET_EXCEEDED", "Encoded image exceeds its byte budget.", true)
                .with("max_bytes", budget.max_bytes));
        }
        (w, h) = ((w * 3 / 4).max(1), (h * 3 / 4).max(1));
    }
}

/// Operator preview sizes (§11.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewQuality {
    Tile,
    Selected,
}

impl PreviewQuality {
    pub fn parse(value: &str) -> Result<PreviewQuality> {
        match value {
            "tile" => Ok(PreviewQuality::Tile),
            "selected" => Ok(PreviewQuality::Selected),
            _ => Err(invalid("quality must be tile or selected.").with("field", "quality")),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            PreviewQuality::Tile => "tile",
            PreviewQuality::Selected => "selected",
        }
    }
    /// `(max width, max height, max bytes)`.
    pub fn bounds(self) -> (u32, u32, usize) {
        match self {
            PreviewQuality::Tile => (480, 270, 256 * 1024),
            PreviewQuality::Selected => (1280, 720, 1024 * 1024),
        }
    }
}

/// How a preview picture crosses the network. A photo wallpaper at 1280×720 is
/// about 700 KB as PNG and a tenth of that as JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PreviewFormat {
    Png,
    Jpeg,
}

impl PreviewFormat {
    /// JPEG when the console asks for it; anything else gets PNG, which every console reads.
    pub fn parse(value: &str) -> PreviewFormat {
        if value == "jpeg" { PreviewFormat::Jpeg } else { PreviewFormat::Png }
    }
    /// The answer's field that carries the picture.
    pub fn as_str(self) -> &'static str {
        match self {
            PreviewFormat::Png => "png",
            PreviewFormat::Jpeg => "jpeg",
        }
    }
}

/// JPEG qualities of a preview: text stays readable at the first; a screen of
/// fine noise, which no desktop shows for long, needs the lower ones to fit.
const PREVIEW_JPEG_QUALITIES: [u8; 3] = [85, 65, 45];

/// Encode an operator preview within its quality's bounds.
pub fn encode_preview(src: &[u8], sw: u32, sh: u32, quality: PreviewQuality, format: PreviewFormat) -> Result<Encoded> {
    let (max_w, max_h, max_bytes) = quality.bounds();
    let (w, h) = fit_inside(sw, sh, max_w, max_h);
    let rgb = resample(src, sw, sh, w, h);
    if format == PreviewFormat::Jpeg {
        for jpeg_quality in PREVIEW_JPEG_QUALITIES {
            let jpeg = encode_jpeg(&rgb, w, h, jpeg_quality)?;
            if jpeg.len() <= max_bytes {
                return Ok(Encoded { bytes: jpeg, format: ImageFormat::Jpeg, width: w, height: h });
            }
        }
        return Err(IbaraError::new("BUDGET_EXCEEDED", "Encoded preview exceeds size limit.", true));
    }
    let png = encode_png(&rgb, w, h, None, png::Compression::Fast)?;
    if png.len() <= max_bytes {
        return Ok(Encoded { bytes: png, format: ImageFormat::Png, width: w, height: h });
    }
    drop(png);
    // A detailed wallpaper or photo does not fit the truecolour budget; a
    // 256-colour palette of the same frame always does.
    let (palette, indices) = quantize(&rgb);
    drop(rgb);
    let mut png = encode_png(&indices, w, h, Some(&palette), png::Compression::Fast)?;
    if png.len() > max_bytes {
        // Incompressible indices can grow under fast deflate; stored blocks cannot.
        png = encode_png(&indices, w, h, Some(&palette), png::Compression::NoCompression)?;
    }
    if png.len() > max_bytes {
        return Err(IbaraError::new("BUDGET_EXCEEDED", "Encoded preview exceeds size limit.", true));
    }
    Ok(Encoded { bytes: png, format: ImageFormat::Png, width: w, height: h })
}

/// Box of 5-bit-per-channel colour bins, inclusive bounds.
#[derive(Clone, Copy)]
struct ColorBox {
    lo: [usize; 3],
    hi: [usize; 3],
    count: u64,
}

fn bin(r: usize, g: usize, b: usize) -> usize {
    (r << 10) | (g << 5) | b
}

impl ColorBox {
    /// The tight box around every occupied bin within `lo..=hi`, if any.
    fn tight(hist: &[u32], lo: [usize; 3], hi: [usize; 3]) -> Option<ColorBox> {
        let mut min = [31usize; 3];
        let mut max = [0usize; 3];
        let mut count = 0u64;
        for r in lo[0]..=hi[0] {
            for g in lo[1]..=hi[1] {
                for b in lo[2]..=hi[2] {
                    let n = hist[bin(r, g, b)];
                    if n > 0 {
                        count += n as u64;
                        for (axis, v) in [r, g, b].into_iter().enumerate() {
                            min[axis] = min[axis].min(v);
                            max[axis] = max[axis].max(v);
                        }
                    }
                }
            }
        }
        (count > 0).then_some(ColorBox { lo: min, hi: max, count })
    }

    fn splittable(&self) -> bool {
        (0..3).any(|a| self.hi[a] > self.lo[a])
    }

    /// Split at the median along the longest axis.
    fn split(&self, hist: &[u32]) -> (ColorBox, ColorBox) {
        let axis = (0..3).max_by_key(|&a| self.hi[a] - self.lo[a]).unwrap_or(0);
        let mut slices = [0u64; 32];
        for r in self.lo[0]..=self.hi[0] {
            for g in self.lo[1]..=self.hi[1] {
                for b in self.lo[2]..=self.hi[2] {
                    slices[[r, g, b][axis]] += hist[bin(r, g, b)] as u64;
                }
            }
        }
        let mut cut = self.lo[axis];
        let mut seen = 0u64;
        for (v, n) in slices.iter().enumerate().take(self.hi[axis]).skip(self.lo[axis]) {
            seen += n;
            cut = v;
            if seen * 2 >= self.count {
                break;
            }
        }
        let mut left_hi = self.hi;
        left_hi[axis] = cut;
        let mut right_lo = self.lo;
        right_lo[axis] = cut + 1;
        // Both halves hold an occupied bin: the tight bounds are occupied and cut < hi.
        let left = ColorBox::tight(hist, self.lo, left_hi).unwrap_or(*self);
        let right = ColorBox::tight(hist, right_lo, self.hi).unwrap_or(*self);
        (left, right)
    }
}

/// Median-cut quantisation to at most 256 colours over a 15-bit histogram.
/// Returns the RGB palette and one index per pixel.
pub fn quantize(rgb: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key = |p: &[u8; 3]| bin((p[0] >> 3) as usize, (p[1] >> 3) as usize, (p[2] >> 3) as usize);
    let pixels = rgb.as_chunks::<3>().0;
    let mut hist = vec![0u32; 1 << 15];
    for p in pixels {
        hist[key(p)] += 1;
    }
    let mut boxes: Vec<ColorBox> = ColorBox::tight(&hist, [0; 3], [31; 3]).into_iter().collect();
    while boxes.len() < 256 {
        let Some(i) = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.splittable())
            .max_by_key(|(_, b)| b.count)
            .map(|(i, _)| i)
        else {
            break;
        };
        let chosen = boxes.swap_remove(i);
        let (left, right) = chosen.split(&hist);
        boxes.push(left);
        boxes.push(right);
    }
    let mut lut = vec![0u8; 1 << 15];
    let mut palette = Vec::with_capacity(boxes.len() * 3);
    for (index, cbox) in boxes.iter().enumerate() {
        let mut sum = [0u64; 3];
        for r in cbox.lo[0]..=cbox.hi[0] {
            for g in cbox.lo[1]..=cbox.hi[1] {
                for b in cbox.lo[2]..=cbox.hi[2] {
                    let n = hist[bin(r, g, b)] as u64;
                    if n > 0 {
                        lut[bin(r, g, b)] = index as u8;
                        sum[0] += n * ((r << 3) | 4) as u64;
                        sum[1] += n * ((g << 3) | 4) as u64;
                        sum[2] += n * ((b << 3) | 4) as u64;
                    }
                }
            }
        }
        palette.extend(sum.iter().map(|s| (s / cbox.count.max(1)) as u8));
    }
    if palette.is_empty() {
        palette.extend([0, 0, 0]);
    }
    let indices = pixels.iter().map(|p| lut[key(p)]).collect();
    (palette, indices)
}

/// FNV-1a 32-bit, as today's `regionHash`.
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2_166_136_261u32, |hash, b| (hash ^ *b as u32).wrapping_mul(16_777_619))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }

    fn ppm(w: u32, h: u32) -> Vec<u8> {
        let mut bytes = format!("P6\n{w} {h}\n255\n").into_bytes();
        bytes.extend(std::iter::repeat_n(7u8, (w * h * 3) as usize));
        bytes
    }

    #[test]
    fn ppm_header_is_validated_before_pixels_are_used() {
        let frame = parse_ppm(ppm(4, 3)).unwrap();
        assert_eq!((frame.width, frame.height, frame.pixels().len()), (4, 3, 36));
        let mut short = ppm(4, 3);
        short.pop();
        assert_eq!(parse_ppm(short).unwrap_err().code, "CAPABILITY_UNAVAILABLE");
        assert_eq!(parse_ppm(b"P6\n5000 4000\n255\n".to_vec()).unwrap_err().code, "BUDGET_EXCEEDED");
        assert!(parse_ppm(b"\x89PNG\r\n\x1a\n".to_vec()).is_err());
        assert!(parse_ppm(b"P6\n4 3\n65535\n".to_vec()).is_err());
    }

    #[test]
    fn fit_inside_never_enlarges_and_keeps_aspect() {
        assert_eq!(fit_inside(100, 50, 480, 270), (100, 50));
        assert_eq!(fit_inside(1920, 1080, 480, 270), (480, 270));
        assert_eq!(fit_inside(1080, 1920, 1280, 720), (405, 720));
        assert_eq!(fit_inside(5000, 1, 480, 270), (480, 1));
    }

    #[test]
    fn area_average_preserves_flat_colour_and_mean() {
        let flat: Vec<u8> = [10u8, 200, 90].repeat(7 * 5);
        assert_eq!(resample(&flat, 7, 5, 3, 2), [10u8, 200, 90].repeat(6));
        // Two columns black, two white: halving averages them.
        let stripes: Vec<u8> = [0u8, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255].to_vec();
        assert_eq!(resample(&stripes, 4, 1, 2, 1), vec![0, 0, 0, 255, 255, 255]);
        assert_eq!(resample(&stripes, 4, 1, 1, 1), vec![128, 128, 128]);
    }

    #[test]
    fn noisy_previews_fit_their_budget_as_a_palette_png_or_a_jpeg() {
        let (w, h) = (1280, 720);
        let src = noise((w * h * 3) as usize);
        let preview = encode_preview(&src, w, h, PreviewQuality::Selected, PreviewFormat::Png).unwrap();
        assert!(preview.bytes.len() <= 1024 * 1024, "{} bytes", preview.bytes.len());
        let decoded = image::load_from_memory_with_format(&preview.bytes, image::ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1280, 720));
        let tile = encode_preview(&src, w, h, PreviewQuality::Tile, PreviewFormat::Png).unwrap();
        assert!(tile.bytes.len() <= 256 * 1024);
        assert_eq!((tile.width, tile.height), (480, 270));
        // Noise, JPEG's worst case, still fits at a lower quality.
        for (quality, size, budget) in [(PreviewQuality::Selected, (1280, 720), 1024 * 1024), (PreviewQuality::Tile, (480, 270), 256 * 1024)] {
            let jpeg = encode_preview(&src, w, h, quality, PreviewFormat::Jpeg).unwrap();
            assert!(jpeg.bytes.len() <= budget, "{quality:?}: {} bytes", jpeg.bytes.len());
            let decoded = image::load_from_memory_with_format(&jpeg.bytes, image::ImageFormat::Jpeg).unwrap();
            assert_eq!((decoded.width(), decoded.height()), size);
        }
    }

    #[test]
    fn agent_images_meet_the_byte_budget_or_refuse() {
        let (w, h) = (800, 600);
        let src = noise((w * h * 3) as usize);
        let budget = ImageBudget { max_bytes: 40 * 1024, max_width: 1600, max_height: 1600 };
        let image = encode_within(&src, w, h, &budget).unwrap();
        assert!(image.bytes.len() <= budget.max_bytes);
        assert!(image.width < w, "noise cannot fit 40 KiB at full size");
        let flat = vec![128u8; (w * h * 3) as usize];
        let crisp = encode_within(&flat, w, h, &budget).unwrap();
        assert_eq!((crisp.format, crisp.width, crisp.height), (ImageFormat::Webp, w, h));
        let tiny = ImageBudget { max_bytes: 1024, max_width: 64, max_height: 64 };
        let err = encode_within(&noise(64 * 64 * 3), 64, 64, &tiny).unwrap_err();
        assert_eq!(err.code, "BUDGET_EXCEEDED");
    }

    #[test]
    fn quantize_keeps_few_colours_exact_enough() {
        let src: Vec<u8> = [[255u8, 0, 0], [0, 0, 255]].iter().flat_map(|c| c.repeat(50)).collect();
        let (palette, indices) = quantize(&src);
        assert_eq!(palette.len(), 6);
        assert_ne!(indices[0], indices[99]);
        let red = &palette[indices[0] as usize * 3..][..3];
        assert!(red[0] > 240 && red[2] < 16);
    }

    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        assert_eq!(format!("{:x}", fnv1a32(b"")), "811c9dc5");
        assert_eq!(format!("{:x}", fnv1a32(b"a")), "e40c292c");
    }
}
