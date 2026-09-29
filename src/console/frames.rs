//! Preview frames as private files for the console.
//!
//! A frame used to reach the shell as base64 PNG inside the JSON line. The
//! shell's script engine then held the line, the parsed string and a
//! `data:` URL, each up to 2.8 MB of UTF-16, once a second while Screen showed
//! a computer. Its heap grew with that stream and kept the growth: at 2 GiB on
//! Tulip1 the picture stream was 14 of the console's 31 MiB still held after
//! closing, and 32 of its 80 MiB while open (`.research/console-memory/`).
//!
//! Now `ibarad --role operator` writes the frame to
//! `$XDG_RUNTIME_DIR/ibara/previews/<computer>-<quality>-<n>.ppm` (directory
//! 0700, files 0600) and the envelope carries `result.file` and
//! `result.bytes` instead of the picture; the shell's `Image` reads the file.
//!
//! The target sends JPEG, which this console asks for (PNG from a computer from
//! before). The file is raw PPM (P6) scaled to fit the size the plugin shows
//! that quality at, because the shell decodes it on its GUI thread: Qt's
//! asynchronous image reader kept about 15 MiB in the shell after the console
//! closed, and a 1280×720 PNG blocked the GUI thread for 51–83 ms on Tulip1
//! where a 1152×648 PPM takes 4–6 ms.
//!
//! A still screen is not sent again. Each picture's answer names its digest;
//! the next request for the same display and shown size carries it while the
//! file is still there, and a target whose screen has not changed answers
//! `unchanged` without the picture. The answer then names the same file, which
//! the shell already shows, so it reads nothing.
//!
//! One writer thread decodes, scales and writes one frame at a time; its
//! buffers are freed after each frame. The newest three frames of each computer
//! and quality are kept (an image cross-fades between two).
//! `preview-release <file name>…`, sent when the console closes, deletes every
//! frame except the ones named: those the shell's sessions still show, so a
//! reopened wall shows its last pictures at once. Startup clears the directory.

use crate::desktop::capture::{fit_inside, resample};
use crate::operator::pattern;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{LazyLock, Mutex};

/// Frames kept per computer and quality.
const KEEP: usize = 3;
/// Base64 lengths the console accepts (≈ 256 KiB and 1 MiB of PNG).
const TILE_CHARS: usize = 350_000;
const SELECTED_CHARS: usize = 1_400_000;
/// The largest picture each quality carries (the target's preview bounds).
const TILE_LARGEST: (u32, u32) = (480, 270);
const SELECTED_LARGEST: (u32, u32) = (1280, 720);
/// The shown size used until the plugin names one, in device pixels.
const TILE_SHOWN: (u32, u32) = (448, 256);
const SELECTED_SHOWN: (u32, u32) = (1152, 648);
/// Bounds of a shown size the plugin may name, in device pixels.
pub const SHOWN_SIDE: std::ops::RangeInclusive<u32> = 16..=4096;

const INVALID: &str = "The preview picture was not valid.";
const NO_PICTURE: &str = "The preview carried no picture.";
const TOO_LARGE: &str = "The preview picture was larger than its quality allows.";

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `$XDG_RUNTIME_DIR/ibara/previews`, beside the socket.
pub fn directory(socket: &Path) -> Option<PathBuf> {
    socket.parent().map(|dir| dir.join("previews"))
}

/// Create the private directory (ours, 0700) and remove every file in it.
pub fn reset(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new().mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?
        }
        Err(e) => return Err(format!("{}: {e}", dir.display())),
        Ok(meta) if !meta.is_dir() || meta.uid() != crate::operator::current_uid() || meta.mode() & 0o077 != 0 => {
            return Err(format!("{} is not a private directory owned by this user.", dir.display()));
        }
        Ok(_) => {}
    }
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
    Ok(())
}

/// A stored frame: `(computer, quality, sequence)` from its file name.
fn parse_name(name: &str) -> Option<(&str, &str, u64)> {
    let stem = name.strip_suffix(".ppm")?;
    let (rest, sequence) = stem.rsplit_once('-')?;
    let (computer, quality) = rest.rsplit_once('-')?;
    Some((computer, quality, sequence.parse().ok()?))
}

/// How the target encoded a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Png,
    Jpeg,
}

/// One frame for the writer thread.
struct Job {
    encoding: Encoding,
    picture: String,
    path: PathBuf,
    largest: (u32, u32),
    shown: (u32, u32),
    reply: tokio::sync::oneshot::Sender<Result<usize, String>>,
}

/// The writer thread's queue, started with the first frame. `None` if the
/// thread could not start.
static WRITER: LazyLock<Option<Sender<Job>>> = LazyLock::new(|| {
    let (jobs, queue) = channel::<Job>();
    let thread = std::thread::Builder::new().name("ibara-previews".into()).spawn(move || {
        for job in queue {
            let written = write_frame(job.encoding, &job.picture, &job.path, job.largest, job.shown);
            drop(job.picture);
            let _ = job.reply.send(written);
        }
    });
    thread.ok().map(|_| jobs)
});

/// `(base64 limit, largest picture, shown size until the plugin names one)` of a quality.
fn bounds(quality: &str) -> (usize, (u32, u32), (u32, u32)) {
    if quality == "tile" { (TILE_CHARS, TILE_LARGEST, TILE_SHOWN) } else { (SELECTED_CHARS, SELECTED_LARGEST, SELECTED_SHOWN) }
}

/// The last picture written of one computer at one quality.
#[derive(Debug, Clone)]
pub struct Shown {
    display: String,
    size: (u32, u32),
    digest: String,
    path: PathBuf,
    bytes: usize,
}

impl Shown {
    /// The target's digest of the picture.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// The previews directory, and the last picture of each computer and quality.
pub struct Frames {
    dir: PathBuf,
    last: Mutex<HashMap<(String, String), Shown>>,
}

impl Frames {
    pub fn new(dir: PathBuf) -> Frames {
        Frames { dir, last: Mutex::new(HashMap::new()) }
    }

    /// The last picture of this display at this shown size (the quality's
    /// default when `None`), while its file is still there.
    pub fn shown(&self, computer: &str, quality: &str, display: &str, shown: Option<(u32, u32)>) -> Option<Shown> {
        let size = shown.unwrap_or(bounds(quality).2);
        let last = self.last.lock().unwrap_or_else(|p| p.into_inner()).get(&(computer.to_string(), quality.to_string())).cloned()?;
        (last.display == display && last.size == size && last.path.exists()).then_some(last)
    }

    /// Replace the picture in an observe answer (`result.jpeg`, or `result.png`
    /// from a computer from before) with a written PPM file, scaled to fit
    /// `shown` (device pixels; the quality's default when `None`). An
    /// `unchanged` answer names `sent`'s file: the picture whose digest the
    /// request carried.
    pub async fn store(
        &self,
        data: Value,
        computer: &str,
        quality: &str,
        display: &str,
        shown: Option<(u32, u32)>,
        sent: Option<Shown>,
    ) -> Result<Value, String> {
        let (limit, largest, default) = bounds(quality);
        let size = shown.unwrap_or(default);
        let key = (computer.to_string(), quality.to_string());
        let mut data = data;
        let result = data.get_mut("result").and_then(Value::as_object_mut).ok_or(NO_PICTURE)?;
        let digest = result.get("digest").and_then(Value::as_str).filter(|d| pattern::lower_hex(d, 64)).map(str::to_string);
        if result.get("unchanged") == Some(&Value::Bool(true)) {
            // A picture released since the request went out is gone: the next one comes in full.
            let Some(sent) = sent.filter(|s| digest.as_deref() == Some(s.digest.as_str()) && s.path.exists()) else {
                self.last.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
                return Err(NO_PICTURE.into());
            };
            result.insert("file".into(), json!(sent.path.display().to_string()));
            result.insert("bytes".into(), json!(sent.bytes));
            self.last.lock().unwrap_or_else(|p| p.into_inner()).insert(key, sent);
            return Ok(data);
        }
        let (encoding, picture) = match (result.shift_remove("jpeg"), result.shift_remove("png")) {
            (Some(Value::String(jpeg)), None) => (Encoding::Jpeg, jpeg),
            (None, Some(Value::String(png))) => (Encoding::Png, png),
            _ => return Err(NO_PICTURE.into()),
        };
        if picture.len() > limit {
            return Err("The preview exceeded its frame budget.".into());
        }
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{computer}-{quality}-{sequence}.ppm"));
        let (reply, written) = tokio::sync::oneshot::channel();
        let job = Job { encoding, picture, path: path.clone(), largest, shown: size, reply };
        WRITER.as_ref().and_then(|jobs| jobs.send(job).ok()).ok_or("The preview writer is not running.")?;
        let bytes = written.await.map_err(|_| "The preview writer stopped.")??;
        prune(&self.dir, |c, q| c == computer && q == quality, KEEP);
        result.insert("file".into(), json!(path.display().to_string()));
        result.insert("bytes".into(), json!(bytes));
        let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
        match digest {
            Some(digest) => last.insert(key, Shown { display: display.to_string(), size, digest, path, bytes }),
            None => last.remove(&key),
        };
        Ok(data)
    }

    /// The console closed: delete every frame the shell no longer references.
    /// `keep` names the files its sessions still show (base names); anything
    /// else, including a picture that landed after the close, goes.
    pub fn release(&self, keep: &[String]) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if parse_name(name).is_some() && !keep.iter().any(|k| k == name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Decode a base64 PNG or JPEG no larger than `largest`, scale it to fit
/// `shown` keeping its aspect (never enlarging), and write it as a new 0600 P6
/// file. Returns the file's size.
fn write_frame(encoding: Encoding, picture: &str, path: &Path, largest: (u32, u32), shown: (u32, u32)) -> Result<usize, String> {
    let picture = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, picture).map_err(|_| INVALID)?;
    let (rgb, width, height) = match encoding {
        Encoding::Png => decode_png(&picture, largest)?,
        Encoding::Jpeg => decode_jpeg(&picture, largest)?,
    };
    drop(picture);
    let (w, h) = fit_inside(width, height, shown.0, shown.1);
    let pixels = if (w, h) == (width, height) { rgb } else { resample(&rgb, width, height, w, h) };
    let header = format!("P6\n{w} {h}\n255\n");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if let Err(e) = file.write_all(header.as_bytes()).and_then(|()| file.write_all(&pixels)) {
        let _ = std::fs::remove_file(path);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(header.len() + pixels.len())
}

/// A PNG decoder that reads only what a preview needs. The pixels are ours to
/// allocate after the size check. Colour profiles and text chunks are skipped
/// rather than inflated (an iCCP chunk of a few hundred KB can inflate to the
/// png crate's default 64 MiB limit, on every frame), and the decoder may
/// allocate at most 2 MiB of its own: more than any real frame's chunks and
/// row buffers need.
fn preview_decoder(png: &[u8]) -> png::Decoder<std::io::Cursor<&[u8]>> {
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(png), png::Limits { bytes: 2 << 20 });
    decoder.set_ignore_iccp_chunk(true);
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder
}

/// A PNG as tightly packed 8-bit RGB, refusing one larger than `largest`.
fn decode_png(png: &[u8], largest: (u32, u32)) -> Result<(Vec<u8>, u32, u32), String> {
    let mut reader = preview_decoder(png).read_info().map_err(|_| INVALID)?;
    let (width, height) = reader.info().size();
    if width == 0 || height == 0 || width > largest.0 || height > largest.1 {
        return Err(TOO_LARGE.into());
    }
    let mut buffer = vec![0u8; reader.output_buffer_size().ok_or(INVALID)?];
    let frame = reader.next_frame(&mut buffer).map_err(|_| INVALID)?;
    buffer.truncate(frame.buffer_size());
    let pixels = width as usize * height as usize;
    let rgb = match frame.color_type {
        png::ColorType::Rgb => buffer,
        png::ColorType::Rgba => {
            for i in 0..pixels {
                buffer.copy_within(i * 4..i * 4 + 3, i * 3);
            }
            buffer.truncate(pixels * 3);
            buffer
        }
        png::ColorType::Grayscale => buffer.iter().flat_map(|&v| [v; 3]).collect(),
        png::ColorType::GrayscaleAlpha => buffer.chunks_exact(2).flat_map(|p| [p[0]; 3]).collect(),
        png::ColorType::Indexed => return Err(INVALID.into()),
    };
    if rgb.len() != pixels * 3 {
        return Err(INVALID.into());
    }
    Ok((rgb, width, height))
}

/// A JPEG as tightly packed 8-bit RGB, refusing one larger than `largest`
/// from its header, before any pixels are decoded.
fn decode_jpeg(jpeg: &[u8], largest: (u32, u32)) -> Result<(Vec<u8>, u32, u32), String> {
    use image::ImageDecoder;
    let decoder = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg)).map_err(|_| INVALID)?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || width > largest.0 || height > largest.1 {
        return Err(TOO_LARGE.into());
    }
    let grey = match decoder.color_type() {
        image::ColorType::Rgb8 => false,
        image::ColorType::L8 => true,
        _ => return Err(INVALID.into()),
    };
    let mut buffer = vec![0u8; decoder.total_bytes() as usize];
    decoder.read_image(&mut buffer).map_err(|_| INVALID)?;
    let rgb = if grey { buffer.iter().flat_map(|&v| [v; 3]).collect() } else { buffer };
    Ok((rgb, width, height))
}

/// Keep the newest `keep` frames of every (computer, quality) that `matches`.
fn prune(dir: &Path, matches: impl Fn(&str, &str) -> bool, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut groups: HashMap<(String, String), Vec<(u64, PathBuf)>> = HashMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((computer, quality, sequence)) = name.to_str().and_then(parse_name) else { continue };
        if matches(computer, quality) {
            groups.entry((computer.to_string(), quality.to_string())).or_default().push((sequence, entry.path()));
        }
    }
    for mut frames in groups.into_values() {
        frames.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, path) in frames.into_iter().skip(keep) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, color: png::ColorType, data: &[u8], palette: Option<&[u8]>) -> String {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            if let Some(palette) = palette {
                encoder.set_palette(palette.to_vec());
            }
            encoder.write_header().unwrap().write_image_data(data).unwrap();
        }
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, out)
    }

    fn frame(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_palette_frame_is_written_as_rgb_ppm_at_its_own_size_when_it_fits() {
        let dir = std::env::temp_dir().join(format!("ibara-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = frame(&dir, "palette.ppm");
        let encoded = png(2, 1, png::ColorType::Indexed, &[1, 0], Some(&[10, 20, 30, 200, 100, 50]));
        let bytes = write_frame(Encoding::Png, &encoded, &path, (480, 270), (448, 256)).unwrap();
        let written = std::fs::read(&path).unwrap();
        assert_eq!(written, b"P6\n2 1\n255\n\xc8\x64\x32\x0a\x14\x1e");
        assert_eq!(bytes, written.len());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_frame_scales_to_fit_the_shown_size_and_keeps_its_aspect() {
        let dir = std::env::temp_dir().join(format!("ibara-frames-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 64×36, left half black and right half white.
        let rgb: Vec<u8> = (0..36).flat_map(|_| (0..64u32).flat_map(|x| [if x < 32 { 0 } else { 255 }; 3])).collect();
        let path = frame(&dir, "scaled.ppm");
        write_frame(Encoding::Png, &png(64, 36, png::ColorType::Rgb, &rgb, None), &path, (480, 270), (32, 32)).unwrap();
        let written = std::fs::read(&path).unwrap();
        let header = b"P6\n32 18\n255\n";
        assert_eq!(&written[..header.len()], header);
        let pixels = &written[header.len()..];
        assert_eq!(pixels.len(), 32 * 18 * 3);
        assert_eq!((pixels[0], pixels[31 * 3]), (0, 255), "area averaging keeps the two halves");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The zlib stream of a PNG's image data: here, 8 MiB of zeros in a few KB.
    fn zlib_bomb() -> Vec<u8> {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 4095, 2048);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.write_header().unwrap().write_image_data(&vec![0; 4095 * 2048]).unwrap();
        }
        let (mut at, mut stream) = (8, Vec::new());
        while at < encoded.len() {
            let length = u32::from_be_bytes(encoded[at..at + 4].try_into().unwrap()) as usize;
            if &encoded[at + 4..at + 8] == b"IDAT" {
                stream.extend_from_slice(&encoded[at + 8..at + 8 + length]);
            }
            at += 12 + length;
        }
        stream
    }

    #[test]
    fn a_colour_profile_that_inflates_to_megabytes_is_skipped_unread() {
        let bomb = zlib_bomb();
        assert!(bomb.len() < 64 * 1024, "{} bytes", bomb.len());
        let mut chunk = b"bomb\0\0".to_vec();
        chunk.extend_from_slice(&bomb);
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
            encoder.set_color(png::ColorType::Rgb);
            let mut writer = encoder.write_header().unwrap();
            writer.write_chunk(png::chunk::iCCP, &chunk).unwrap();
            writer.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        }
        // The png crate's own default inflates the whole profile.
        let reader = png::Decoder::new(std::io::Cursor::new(&encoded[..])).read_info().unwrap();
        assert_eq!(reader.info().icc_profile.as_ref().map(|p| p.len()), Some(4096 * 2048));
        // The preview decoder never inflates it and still reads the picture.
        let reader = preview_decoder(&encoded).read_info().unwrap();
        assert!(reader.info().icc_profile.is_none());
        assert_eq!(decode_png(&encoded, (480, 270)).unwrap(), (vec![1, 2, 3, 4, 5, 6], 2, 1));
        // The decoder's own 2 MiB covers the widest, deepest frame allowed.
        let (w, h) = SELECTED_LARGEST;
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, w, h);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Sixteen);
            encoder.write_header().unwrap().write_image_data(&vec![7; (w * h * 8) as usize]).unwrap();
        }
        let (rgb, ..) = decode_png(&encoded, SELECTED_LARGEST).unwrap();
        assert_eq!(rgb.len(), (w * h * 3) as usize);
    }

    fn jpeg(width: u32, height: u32) -> String {
        let mut out = Vec::new();
        let rgb = vec![128; (width * height * 3) as usize];
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode(&rgb, width, height, image::ExtendedColorType::Rgb8).unwrap();
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, out)
    }

    #[test]
    fn a_frame_larger_than_its_quality_or_not_a_picture_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("ibara-frames-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = frame(&dir, "refused.ppm");
        let wide = png(481, 1, png::ColorType::Grayscale, &[0; 481], None);
        assert_eq!(write_frame(Encoding::Png, &wide, &path, (480, 270), (448, 256)).unwrap_err(), TOO_LARGE);
        assert_eq!(write_frame(Encoding::Png, "iVBORw0KGgo=", &path, (480, 270), (448, 256)).unwrap_err(), INVALID);
        assert_eq!(write_frame(Encoding::Jpeg, &jpeg(480, 271), &path, (480, 270), (448, 256)).unwrap_err(), TOO_LARGE);
        assert_eq!(write_frame(Encoding::Jpeg, &wide, &path, (480, 270), (448, 256)).unwrap_err(), INVALID);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
