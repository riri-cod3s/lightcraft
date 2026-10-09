//! Background decoding: a small pool of threads decodes photos (EXIF-rotated, downscaled to screen
//! size) into `ColorImage`s; the UI thread turns them into textures.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::ColorImage;
use image::{DynamicImage, ImageDecoder, ImageReader};

/// Longest side of the on-screen copy. Plenty for a full-screen view while keeping decode fast.
const SCREEN_EDGE: u32 = 2600;

pub enum Slot {
    Ready(ColorImage),
    Failed(String),
    Skipped,
}

struct Job {
    gen_id: usize,
    idx: usize,
    path: PathBuf,
}

pub struct Loader {
    tx: Sender<Job>,
    rx: Receiver<(usize, usize, Slot)>,
    current: Arc<AtomicUsize>,
    requested: HashSet<usize>,
    failed: HashMap<usize, String>,
    generation: usize,
    pending: usize,
    workers_tx: Sender<(usize, usize, Slot)>,
    job_rx: Arc<Mutex<Receiver<Job>>>,
}

impl Loader {
    pub fn new() -> Self {
        let (tx, job_rx) = channel::<Job>();
        let (res_tx, rx) = channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let current = Arc::new(AtomicUsize::new(0));
        let loader = Self {
            tx,
            rx,
            current,
            requested: HashSet::new(),
            failed: HashMap::new(),
            generation: 0,
            pending: 0,
            workers_tx: res_tx,
            job_rx,
        };
        loader.spawn_workers();
        loader
    }

    fn spawn_workers(&self) {
        let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8);
        for _ in 0..workers {
            let job_rx = self.job_rx.clone();
            let res = self.workers_tx.clone();
            let current = self.current.clone();
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let guard = job_rx.lock().unwrap_or_else(PoisonError::into_inner);
                        match guard.recv() {
                            Ok(j) => j,
                            Err(_) => return,
                        }
                    };
                    // The user moved on while this waited in the queue: don't spend time on it.
                    let stale = job.idx.abs_diff(current.load(Ordering::Relaxed)) > 8;
                    let slot = if stale {
                        Slot::Skipped
                    } else {
                        match decode(&job.path) {
                            Ok(img) => Slot::Ready(img),
                            Err(e) => Slot::Failed(e),
                        }
                    };
                    if res.send((job.gen_id, job.idx, slot)).is_err() {
                        return;
                    }
                }
            });
        }
    }

    pub fn set_current(&self, idx: usize) {
        self.current.store(idx, Ordering::Relaxed);
    }

    pub fn request(&mut self, idx: usize, path: PathBuf) {
        if self.requested.insert(idx)
            && self.tx.send(Job { gen_id: self.generation, idx, path }).is_ok()
        {
            self.pending += 1;
        }
    }

    pub fn poll(&mut self) -> Vec<(usize, Slot)> {
        let mut out = Vec::new();
        while let Ok((gen_id, i, slot)) = self.rx.try_recv() {
            if gen_id != self.generation {
                continue;
            }
            self.pending = self.pending.saturating_sub(1);
            match &slot {
                Slot::Failed(e) => {
                    self.failed.insert(i, e.clone());
                }
                Slot::Skipped => {
                    self.requested.remove(&i);
                }
                Slot::Ready(_) => {}
            }
            out.push((i, slot));
        }
        out
    }

    pub fn failure(&self, idx: usize) -> Option<&str> {
        self.failed.get(&idx).map(String::as_str)
    }

    pub fn busy(&self) -> bool {
        self.pending > 0
    }

    /// Forget requests for photos the UI dropped, so scrolling back reloads them.
    pub fn forget_if(&mut self, far: impl Fn(usize) -> bool) {
        self.requested.retain(|&i| !far(i));
    }

    /// New folder: drop everything and ignore results still in flight.
    pub fn reset(&mut self) {
        self.generation += 1;
        self.pending = 0;
        self.requested.clear();
        self.failed.clear();
        while self.rx.try_recv().is_ok() {}
    }
}

pub fn decode(path: &Path) -> Result<ColorImage, String> {
    let img = decode_oriented(path)?;
    let img = if img.width().max(img.height()) > SCREEN_EDGE {
        img.thumbnail(SCREEN_EDGE, SCREEN_EDGE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Full-size decode with the EXIF orientation applied.
pub fn decode_oriented(path: &Path) -> Result<DynamicImage, String> {
    let reader = ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "timing probe: cargo test --release -- --ignored --nocapture"]
    fn decode_24mp_timing() {
        let p = std::env::temp_dir().join("qc-24mp.jpg");
        image::RgbImage::from_fn(6000, 4000, |x, y| image::Rgb([(x / 24) as u8, (y / 16) as u8, ((x ^ y) % 256) as u8]))
            .save(&p)
            .unwrap();
        let t = std::time::Instant::now();
        let img = super::decode(&p).unwrap();
        println!("24MP decode+fit: {:?} -> {:?}", t.elapsed(), img.size);
        let _ = std::fs::remove_file(p);
    }
}
