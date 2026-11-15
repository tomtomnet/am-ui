//! Artwork: fetched and decoded off the user interface thread, kept as textures.
//!
//! A widget asks for an artwork at the size it draws it (in device pixels). Worker threads take
//! the newest request first (what just scrolled into view), get the image bytes from the music
//! service (which keeps them on disk), decode and scale them, and hand RGBA pixels back; the
//! user interface thread wraps them in a texture and tells the widgets that wait. Decoded
//! textures stay in memory up to a byte budget, least recently used first out.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};

use amui::types::Artwork;
use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::data::Music;

/// Decoded textures kept in memory.
const MEMORY_BYTES: usize = 160 * 1024 * 1024;
const THREADS: usize = 6;
/// Sizes Apple's image service is asked for: a request uses the smallest one that is large
/// enough, so that the same file serves many drawn sizes and stays in the disk cache.
const FETCH_SIZES: [u32; 11] = [64, 100, 160, 240, 320, 480, 640, 800, 1200, 2000, 3000];

/// One artwork at one drawn size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub template: String,
    pub px: u32,
}

impl Key {
    pub fn new(artwork: &Artwork, px: u32) -> Key {
        // sizes in steps of 8 pixels: a window being resized does not decode every width
        Key { template: artwork.url_template.clone(), px: px.max(8).div_ceil(8) * 8 }
    }

    fn url(&self) -> String {
        if self.template.starts_with("file://") {
            return self.template.clone();
        }
        let size = FETCH_SIZES.iter().copied().find(|size| *size >= self.px).unwrap_or(3000);
        Artwork::new(self.template.clone()).url(size as i64)
    }
}

/// Called on the user interface thread when a requested artwork is ready.
pub type Waiter = Box<dyn Fn(&Key, &gdk::Texture)>;

struct Jobs {
    queue: Mutex<Vec<Key>>,
    ready: Condvar,
}

struct Decoded {
    key: Key,
    side: u32,
    pixels: Option<Vec<u8>>,
}

struct Inner {
    textures: RefCell<lru::LruCache<Key, gdk::Texture>>,
    bytes: Cell<usize>,
    pending: RefCell<HashSet<Key>>,
    failed: RefCell<HashSet<Key>>,
    waiters: RefCell<HashMap<Key, Vec<(usize, Waiter)>>>,
    jobs: Arc<Jobs>,
}

#[derive(Clone)]
pub struct ArtLoader {
    inner: Rc<Inner>,
}

thread_local! {
    static LOADER: RefCell<Option<ArtLoader>> = const { RefCell::new(None) };
}

/// The loader of this process (set once at start-up).
pub fn loader() -> ArtLoader {
    LOADER.with(|slot| slot.borrow().clone().expect("the artwork loader is created at start-up"))
}

impl ArtLoader {
    /// Create the loader and make it the one `loader()` returns.
    pub fn install(music: Arc<dyn Music>) -> ArtLoader {
        let jobs = Arc::new(Jobs { queue: Mutex::new(Vec::new()), ready: Condvar::new() });
        let (sender, receiver) = async_channel::unbounded::<Decoded>();
        for n in 0..THREADS {
            let (jobs, sender, music) = (jobs.clone(), sender.clone(), music.clone());
            let _ = std::thread::Builder::new().name(format!("am-ui-art-{n}")).spawn(move || {
                loop {
                    let key = {
                        let mut queue = jobs.queue.lock().unwrap();
                        loop {
                            if let Some(key) = queue.pop() {
                                break key;
                            }
                            queue = jobs.ready.wait(queue).unwrap();
                        }
                    };
                    let pixels = music.artwork(&key.url()).ok().and_then(|data| decode(&data, key.px));
                    if sender.send_blocking(Decoded { side: key.px, key, pixels }).is_err() {
                        return;
                    }
                }
            });
        }
        let inner = Rc::new(Inner {
            textures: RefCell::new(lru::LruCache::unbounded()),
            bytes: Cell::new(0),
            pending: RefCell::default(),
            failed: RefCell::default(),
            waiters: RefCell::default(),
            jobs,
        });
        let weak = Rc::downgrade(&inner);
        glib::spawn_future_local(async move {
            while let Ok(decoded) = receiver.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.pending.borrow_mut().remove(&decoded.key);
                let waiters = inner.waiters.borrow_mut().remove(&decoded.key);
                let Some(pixels) = decoded.pixels else {
                    inner.failed.borrow_mut().insert(decoded.key);
                    continue;
                };
                let size = pixels.len();
                let texture: gdk::Texture = gdk::MemoryTexture::new(
                    decoded.side as i32,
                    decoded.side as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(pixels),
                    decoded.side as usize * 4,
                )
                .upcast();
                inner.remember(decoded.key.clone(), texture.clone(), size);
                for (_, waiter) in waiters.into_iter().flatten() {
                    waiter(&decoded.key, &texture);
                }
            }
        });
        let loader = ArtLoader { inner };
        LOADER.with(|slot| *slot.borrow_mut() = Some(loader.clone()));
        loader
    }

    /// The texture when it is in memory; otherwise it is fetched and `waiter` is called once it
    /// is there (`owner` identifies who asked, for `cancel`).
    pub fn request(&self, key: &Key, owner: usize, waiter: Waiter) -> Option<gdk::Texture> {
        if let Some(texture) = self.inner.textures.borrow_mut().get(key) {
            return Some(texture.clone());
        }
        if self.inner.failed.borrow().contains(key) {
            return None;
        }
        self.inner.waiters.borrow_mut().entry(key.clone()).or_default().push((owner, waiter));
        if self.inner.pending.borrow_mut().insert(key.clone()) {
            self.inner.jobs.queue.lock().unwrap().push(key.clone());
            self.inner.jobs.ready.notify_one();
        }
        None
    }

    /// The texture when it is in memory, without asking for it.
    pub fn cached(&self, key: &Key) -> Option<gdk::Texture> {
        self.inner.textures.borrow_mut().get(key).cloned()
    }

    /// `owner` no longer shows `key`: a request that has not started yet is dropped when nobody
    /// else waits for it.
    pub fn cancel(&self, key: &Key, owner: usize) {
        let mut waiters = self.inner.waiters.borrow_mut();
        let Some(list) = waiters.get_mut(key) else { return };
        list.retain(|(who, _)| *who != owner);
        if list.is_empty() {
            waiters.remove(key);
            let mut queue = self.inner.jobs.queue.lock().unwrap();
            if let Some(position) = queue.iter().position(|queued| queued == key) {
                queue.remove(position);
                self.inner.pending.borrow_mut().remove(key);
            }
        }
    }
}

impl Inner {
    fn remember(&self, key: Key, texture: gdk::Texture, size: usize) {
        let mut textures = self.textures.borrow_mut();
        textures.put(key, texture);
        let mut bytes = self.bytes.get() + size;
        while bytes > MEMORY_BYTES {
            let Some((old, _)) = textures.pop_lru() else { break };
            bytes = bytes.saturating_sub(old.px as usize * old.px as usize * 4);
        }
        self.bytes.set(bytes);
    }
}

/// The pixels (RGBA) of an artwork as a small square of about `side` pixels, for what looks at an
/// artwork's colours instead of drawing it (the glow of Now Playing). Fetches and decodes: to be
/// called on a worker thread.
pub fn small_rgba(music: &dyn Music, artwork: &Artwork, side: u32) -> Option<Vec<u8>> {
    let key = Key::new(artwork, side);
    decode(&music.artwork(&key.url()).ok()?, key.px)
}

/// RGBA pixels of a square of `side` pixels: the image scaled to cover it, centred.
fn decode(data: &[u8], side: u32) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?;
    let filter = if side < image.width().min(image.height()) / 2 {
        image::imageops::FilterType::Triangle
    } else {
        image::imageops::FilterType::CatmullRom
    };
    Some(image.resize_to_fill(side, side, filter).to_rgba8().into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_few_sizes() {
        let art = Artwork::new("https://is1-ssl.mzstatic.com/image/thumb/x/{w}x{h}{c}.{f}");
        assert_eq!(Key::new(&art, 357).px, 360);
        assert_eq!(Key::new(&art, 360).url(), "https://is1-ssl.mzstatic.com/image/thumb/x/480x480bb.jpg");
        assert_eq!(Key::new(&art, 80).url(), "https://is1-ssl.mzstatic.com/image/thumb/x/100x100bb.jpg");
        assert_eq!(Key::new(&art, 5000).url(), "https://is1-ssl.mzstatic.com/image/thumb/x/3000x3000bb.jpg");
        let file = Artwork::new("file:///covers/0001.jpg");
        assert_eq!(Key::new(&file, 96).url(), "file:///covers/0001.jpg");
    }
}
