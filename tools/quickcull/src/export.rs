//! Copy or compress the picked photos into a destination folder.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

#[derive(Clone, Copy)]
pub struct Settings {
    pub compress: bool,
    pub quality: u8,
    /// Longest side in pixels; 0 keeps the original size.
    pub max_edge: u32,
}

pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

pub fn run(files: Vec<PathBuf>, dest: PathBuf, s: Settings, done: Arc<AtomicUsize>, errors: Sender<String>) {
    if let Err(e) = std::fs::create_dir_all(&dest) {
        let _ = errors.send(format!("can't create {}: {e}", dest.display()));
        done.store(files.len(), Ordering::SeqCst);
        return;
    }
    for src in files {
        if let Err(e) = one(&src, &dest, s) {
            let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let _ = errors.send(format!("{name}: {e}"));
        }
        done.fetch_add(1, Ordering::SeqCst);
    }
}

fn one(src: &Path, dest_dir: &Path, s: Settings) -> Result<(), String> {
    let name = src.file_name().ok_or("no file name")?;
    if !s.compress {
        return std::fs::copy(src, dest_dir.join(name)).map(|_| ()).map_err(|e| e.to_string());
    }
    let mut img = crate::loader::decode_oriented(src)?;
    if s.max_edge > 0 && img.width().max(img.height()) > s.max_edge {
        img = img.resize(s.max_edge, s.max_edge, FilterType::Lanczos3);
    }
    let stem = src.file_stem().ok_or("no file name")?;
    let out = dest_dir.join(Path::new(stem).with_extension("jpg"));
    let tmp = out.with_extension("jpg.part");
    let file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut w = std::io::BufWriter::new(file);
    let rgb = img.to_rgb8();
    JpegEncoder::new_with_quality(&mut w, s.quality)
        .encode_image(&rgb)
        .map_err(|e| e.to_string())?;
    std::io::Write::flush(&mut w).map_err(|e| e.to_string())?;
    drop(w);
    std::fs::rename(&tmp, &out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("quickcull-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_jpeg(path: &Path, w: u32, h: u32) {
        image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
            .save(path)
            .unwrap();
    }

    #[test]
    fn copies_bytes_exactly() {
        let src = scratch("cp-src");
        let dest = scratch("cp-dst").join("out");
        make_jpeg(&src.join("a.jpg"), 64, 48);
        let (tx, rx) = channel();
        let done = Arc::new(AtomicUsize::new(0));
        run(vec![src.join("a.jpg")], dest.clone(), Settings { compress: false, quality: 85, max_edge: 0 }, done.clone(), tx);
        assert!(rx.try_recv().is_err());
        assert_eq!(done.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(src.join("a.jpg")).unwrap(), std::fs::read(dest.join("a.jpg")).unwrap());
    }

    #[test]
    fn compress_resizes_and_shrinks() {
        let src = scratch("cz-src");
        let dest = scratch("cz-dst");
        make_jpeg(&src.join("big.jpg"), 400, 200);
        let (tx, rx) = channel();
        let done = Arc::new(AtomicUsize::new(0));
        run(vec![src.join("big.jpg")], dest.clone(), Settings { compress: true, quality: 60, max_edge: 100 }, done, tx);
        assert!(rx.try_recv().is_err());
        let out = image::open(dest.join("big.jpg")).unwrap();
        assert_eq!((out.width(), out.height()), (100, 50));
    }

    #[test]
    fn broken_file_is_reported_not_fatal() {
        let src = scratch("bad-src");
        let dest = scratch("bad-dst");
        std::fs::write(src.join("bad.jpg"), b"not a jpeg").unwrap();
        make_jpeg(&src.join("ok.jpg"), 32, 32);
        let (tx, rx) = channel();
        let done = Arc::new(AtomicUsize::new(0));
        run(vec![src.join("bad.jpg"), src.join("ok.jpg")], dest.clone(), Settings { compress: true, quality: 80, max_edge: 0 }, done.clone(), tx);
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(done.load(Ordering::SeqCst), 2);
        assert!(dest.join("ok.jpg").exists());
    }
}
