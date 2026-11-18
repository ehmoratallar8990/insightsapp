//! Asynchronous thumbnail cache. Most-recently-requested first (LIFO), so the
//! thumbnails currently on screen win over ones scrolled past.

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, TextureHandle, TextureOptions};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub const SIDE: u32 = 256;
const MAX_ENTRIES: usize = 900;

enum State {
    Pending,
    Ready { tex: TextureHandle, born: f64 },
    Failed,
}

struct Slot {
    state: State,
    last_used: u64,
}

struct Queue {
    stack: Mutex<Vec<String>>,
    cv: Condvar,
}

pub struct Thumbs {
    map: HashMap<String, Slot>,
    queue: Arc<Queue>,
    rx: Receiver<(String, Option<ColorImage>)>,
    frame: u64,
}

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Self {
        let queue = Arc::new(Queue { stack: Mutex::new(Vec::new()), cv: Condvar::new() });
        let (tx, rx): (Sender<(String, Option<ColorImage>)>, _) = unbounded();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 6);
        for i in 0..workers {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            std::thread::Builder::new()
                .name(format!("thumb-{i}"))
                .spawn(move || loop {
                    let path = {
                        let mut s = queue.stack.lock();
                        while s.is_empty() {
                            queue.cv.wait(&mut s);
                        }
                        s.pop().unwrap()
                    };
                    let img = crate::media::decode::thumbnail(Path::new(&path), SIDE).map(|rgba| {
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
                    });
                    if tx.send((path, img)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                })
                .expect("spawn thumbnail worker");
        }
        Thumbs { map: HashMap::new(), queue, rx, frame: 0 }
    }

    /// Call once per frame: uploads finished thumbnails (bounded per frame) and evicts old ones.
    pub fn pump(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        let now = ctx.input(|i| i.time);
        for _ in 0..24 {
            let Ok((path, img)) = self.rx.try_recv() else { break };
            let state = match img {
                Some(img) => State::Ready { tex: ctx.load_texture(&path, img, TextureOptions::LINEAR), born: now },
                None => State::Failed,
            };
            if let Some(slot) = self.map.get_mut(&path) {
                slot.state = state;
            }
        }
        if self.map.len() > MAX_ENTRIES {
            let mut v: Vec<(u64, String)> = self.map.iter().map(|(k, s)| (s.last_used, k.clone())).collect();
            v.sort();
            for (_, k) in v.into_iter().take(self.map.len() - MAX_ENTRIES * 3 / 4) {
                self.map.remove(&k);
            }
        }
        // Drop queued requests that nobody looked at for a while.
        let mut s = self.queue.stack.lock();
        if s.len() > 400 {
            let excess = s.len() - 400;
            let dropped: Vec<String> = s.drain(..excess).collect();
            drop(s);
            for d in dropped {
                self.map.remove(&d);
            }
        }
    }

    /// Returns the texture (if ready) and its fade-in factor 0..1.
    pub fn get(&mut self, ctx: &egui::Context, path: &str) -> Option<(egui::TextureId, egui::Vec2, f32)> {
        let frame = self.frame;
        let now = ctx.input(|i| i.time);
        match self.map.get_mut(path) {
            Some(slot) => {
                slot.last_used = frame;
                match &slot.state {
                    State::Ready { tex, born } => {
                        let t = ((now - born) / 0.35).clamp(0.0, 1.0) as f32;
                        if t < 1.0 {
                            ctx.request_repaint();
                        }
                        Some((tex.id(), tex.size_vec2(), super::theme::ease_out_cubic(t)))
                    }
                    _ => None,
                }
            }
            None => {
                self.map.insert(path.to_string(), Slot { state: State::Pending, last_used: frame });
                self.queue.stack.lock().push(path.to_string());
                self.queue.cv.notify_one();
                None
            }
        }
    }

    pub fn failed(&self, path: &str) -> bool {
        matches!(self.map.get(path), Some(Slot { state: State::Failed, .. }))
    }
}
