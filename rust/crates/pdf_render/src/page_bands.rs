//! Drawing a page on several threads: the page is cut into horizontal bands,
//! each drawn by its own renderer as a region of the page, and the bands
//! share the page's parsed content, fonts and decoded images.
//!
//! A region is the same pixels as that part of a whole-page render (see
//! `crisp_tests`), so the bands join without seams. Each band still reads all
//! of the page's content, but what lies outside it is skipped before any
//! drawing, so a page of dense drawing comes out several times faster on a
//! phone's cores while a light page loses nothing.
use super::*;
use crate::image::DecodedImage;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

/// Bands smaller than this are not worth a thread: every band reads all of
/// the page's content.
const MIN_BAND_PIXELS: u64 = 64_000;

/// Bands larger than this are cut smaller, so the bands being drawn at once
/// add little to the memory of the page itself.
const MAX_BAND_PIXELS: u64 = 1 << 20;

/// Threads to draw with when the caller leaves it to the renderer. Phones
/// pair a few fast cores with slower ones and share one allocator between
/// threads; past four, measured on two Android phones, more threads made a
/// dense drawing slower, not faster.
pub(super) fn worker_count(threads: usize) -> usize {
    match threads {
        0 => std::thread::available_parallelism()
            .map_or(1, NonZeroUsize::get)
            .min(4),
        n => n,
    }
}

/// `region` cut into full-width bands: two for each worker, so a band of
/// dense drawing does not hold the others up, within the size limits above.
pub(super) fn split_bands(region: PixelRegion, workers: usize) -> Vec<PixelRegion> {
    let pixels = u64::from(region.width) * u64::from(region.height);
    let count = if workers <= 1 {
        1
    } else {
        (2 * workers as u64)
            .min(pixels / MIN_BAND_PIXELS)
            .max(pixels.div_ceil(MAX_BAND_PIXELS))
            .clamp(1, u64::from(region.height.max(1)))
    } as u32;
    (0..count)
        .map(|index| {
            let top = (u64::from(region.height) * u64::from(index) / u64::from(count)) as u32;
            let bottom =
                (u64::from(region.height) * u64::from(index + 1) / u64::from(count)) as u32;
            PixelRegion {
                x: region.x,
                y: region.y + top,
                width: region.width,
                height: bottom - top,
            }
        })
        .collect()
}

/// Fonts, decoded images and parsed content shared by the bands of a render
/// — and, for a document the app keeps open, by every render of it (see
/// `PdfDocument::derived`), so zooming in again does not decode the same
/// scan or parse the same drawing again. Keyed by the object each comes
/// from; whichever band asks first loads it, and the others wait for it.
pub(super) struct Shared {
    fonts: Mutex<HashMap<ObjectId, Arc<OnceLock<Arc<RenderFont>>>>>,
    images: Budgeted<DecodedImage>,
    pages: Budgeted<ParsedPage>,
    forms: Budgeted<Content>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            fonts: Mutex::default(),
            // An image larger than the budget is still kept until the next
            // one arrives: the bands of a render must share it.
            images: Budgeted::new(64 << 20, true),
            // Parsed content larger than the budget is used for its render
            // and let go: a giant drawing is not held while it is not drawn.
            pages: Budgeted::new(32 << 20, false),
            forms: Budgeted::new(16 << 20, false),
        }
    }
}

impl Shared {
    pub(super) fn font(&self, id: ObjectId, load: impl FnOnce() -> RenderFont) -> Arc<RenderFont> {
        let slot = {
            let mut fonts = self.fonts.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(fonts.entry(id).or_default())
        };
        Arc::clone(slot.get_or_init(|| Arc::new(load())))
    }

    pub(super) fn image(
        &self,
        id: ObjectId,
        decode: impl FnOnce() -> Option<DecodedImage>,
    ) -> Option<Arc<DecodedImage>> {
        self.images.get(id, decode, DecodedImage::byte_size)
    }

    pub(super) fn page(
        &self,
        id: ObjectId,
        load: impl FnOnce() -> Option<ParsedPage>,
    ) -> Option<Arc<ParsedPage>> {
        self.pages.get(id, load, |page| page.content.byte_size())
    }

    pub(super) fn form(
        &self,
        id: ObjectId,
        load: impl FnOnce() -> Option<Content>,
    ) -> Option<Arc<Content>> {
        self.forms.get(id, load, Content::byte_size)
    }
}

/// The images a render has used, held for its bands whatever the shared
/// cache lets go of. Bands are drawn a few at a time, so a later band reaches
/// a page's first image after its last one is in: when that pushed the first
/// out of the cache, every later band decoded it again, and four threads came
/// out slower than one.
#[derive(Default)]
pub(super) struct Recent {
    images: Mutex<VecDeque<(ObjectId, Arc<DecodedImage>)>>,
}

/// What [`Recent`] holds at most, besides the newest image: all of a page's
/// images, unless it is a collage of photographs.
const RECENT_IMAGE_BYTES: usize = 128 << 20;

impl Recent {
    pub(super) fn image(
        &self,
        id: ObjectId,
        load: impl FnOnce() -> Option<Arc<DecodedImage>>,
    ) -> Option<Arc<DecodedImage>> {
        let lock = || self.images.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(image) = lock()
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, image)| Arc::clone(image))
        {
            return Some(image);
        }
        let image = load()?;
        let mut images = lock();
        if !images.iter().any(|(key, _)| *key == id) {
            images.push_back((id, Arc::clone(&image)));
            let mut bytes: usize = images.iter().map(|(_, image)| image.byte_size()).sum();
            while bytes > RECENT_IMAGE_BYTES && images.len() > 1 {
                if let Some((_, old)) = images.pop_front() {
                    bytes -= old.byte_size();
                }
            }
        }
        Some(image)
    }
}

/// Values kept up to a memory budget, the least recently used let go first.
/// One in use stays alive through its `Arc`; letting it go only means the
/// next to ask loads it again.
struct Budgeted<T> {
    budget: usize,
    keep_oversized: bool,
    state: Mutex<BudgetState<T>>,
}

struct BudgetState<T> {
    entries: HashMap<ObjectId, Arc<OnceLock<Option<Arc<T>>>>>,
    /// Loaded entries and their sizes, least recently used first.
    order: VecDeque<(ObjectId, usize)>,
    bytes: usize,
}

impl<T> Budgeted<T> {
    fn new(budget: usize, keep_oversized: bool) -> Self {
        Self {
            budget,
            keep_oversized,
            state: Mutex::new(BudgetState {
                entries: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BudgetState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(
        &self,
        id: ObjectId,
        load: impl FnOnce() -> Option<T>,
        size: impl FnOnce(&T) -> usize,
    ) -> Option<Arc<T>> {
        let slot = {
            let mut state = self.lock();
            if let Some(position) = state.order.iter().position(|(key, _)| *key == id) {
                let used = state.order.remove(position);
                state.order.extend(used);
            }
            Arc::clone(state.entries.entry(id).or_default())
        };
        let mut loaded_here = false;
        let value = slot
            .get_or_init(|| {
                loaded_here = true;
                load().map(Arc::new)
            })
            .clone();
        if loaded_here {
            let bytes = value.as_deref().map_or(0, size);
            let mut state = self.lock();
            if bytes > self.budget && !self.keep_oversized {
                state.entries.remove(&id);
                return value;
            }
            state.order.push_back((id, bytes));
            state.bytes += bytes;
            while state.bytes > self.budget && state.order.len() > 1 {
                let Some((old, bytes)) = state.order.pop_front() else {
                    break;
                };
                state.entries.remove(&old);
                state.bytes -= bytes;
            }
        }
        value
    }
}

/// What every band of a page draws from.
pub(super) struct Frame<'a> {
    pub doc: &'a PdfDocument,
    pub page: &'a Dictionary,
    pub resources: &'a Dictionary,
    pub content: &'a Content,
    pub recent: Recent,
    /// Page space to the pixels of the whole page as rendered.
    pub page_ctm: Matrix,
    pub background: Rgb,
    pub shared: &'a Shared,
}

impl Frame<'_> {
    /// Draw `band` of `region`: its pixels and what could not be drawn.
    fn draw(
        &self,
        band: PixelRegion,
        region: PixelRegion,
        temporary_limit: usize,
    ) -> (Vec<u8>, Vec<String>) {
        let mut canvas = Canvas::new(band.width as usize, band.height as usize);
        canvas.fill_background(self.background);
        let base_ctm = self
            .page_ctm
            .then(&Matrix::translate(-f64::from(band.x), -f64::from(band.y)));
        let mut renderer = Renderer {
            doc: self.doc,
            canvas: &mut canvas,
            shared: self.shared,
            recent: &self.recent,
            fonts: HashMap::new(),
            tint_transforms: HashMap::new(),
            depth: 0,
            pattern_pixels: 0.0,
            origin: (f64::from(band.x), f64::from(band.y)),
            region: (
                f64::from(region.x),
                f64::from(region.y),
                f64::from(region.x) + f64::from(region.width),
                f64::from(region.y) + f64::from(region.height),
            ),
            temporary_bytes: 0,
            mask_allocations: Vec::new(),
            temporary_limit,
            render_stopped: false,
            warnings: self.doc.recovery_warnings.clone(),
            visible: true,
            type3_depth: 0,
        };
        let mut state = GraphicsState::new(base_ctm);
        // Damage is recovered from while parsing, so this only fails where
        // drawing genuinely cannot go on; the page so far still stands.
        let _ = renderer.run_operations(self.content, self.resources, &mut state);
        renderer.configure_canvas(&GraphicsState::new(base_ctm));
        renderer.draw_annotations(self.page, self.resources, base_ctm);
        let warnings = renderer.warnings;
        (canvas.pixels, warnings)
    }

    /// Draw `region` of the page on up to `workers` threads, the calling
    /// thread among them.
    pub(super) fn draw_region(
        &self,
        region: PixelRegion,
        workers: usize,
    ) -> (Vec<u8>, Vec<String>) {
        let bands = split_bands(region, workers);
        let workers = workers.clamp(1, bands.len());
        // Bands drawn at once share the scratch budget one render had.
        let temporary_limit = limits::MAX_TEMPORARY_BYTES / workers;
        if bands.len() == 1 {
            return self.draw(region, region, temporary_limit);
        }

        let row_bytes = region.width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * region.height as usize];
        let mut slots = Vec::with_capacity(bands.len());
        let mut rest = pixels.as_mut_slice();
        for band in &bands {
            let (slot, tail) = rest.split_at_mut(row_bytes * band.height as usize);
            slots.push(Some(slot));
            rest = tail;
        }
        let slots = Mutex::new(slots);
        let warnings = Mutex::new(Vec::new());
        let next = AtomicUsize::new(0);
        let work = || loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(&band) = bands.get(index) else {
                break;
            };
            let (band_pixels, band_warnings) = self.draw(band, region, temporary_limit);
            let slot = slots.lock().unwrap_or_else(PoisonError::into_inner)[index].take();
            if let Some(slot) = slot {
                slot.copy_from_slice(&band_pixels);
            }
            warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(band_warnings);
        };
        std::thread::scope(|scope| {
            // A thread that cannot be started leaves its bands to the others.
            let helpers: Vec<_> = (1..workers)
                .filter_map(|_| {
                    std::thread::Builder::new()
                        .stack_size(8 << 20)
                        .spawn_scoped(scope, &work)
                        .ok()
                })
                .collect();
            work();
            for helper in helpers {
                if let Err(panic) = helper.join() {
                    std::panic::resume_unwind(panic);
                }
            }
        });
        let warnings = warnings
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        (pixels, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(width: u32, height: u32) -> PixelRegion {
        PixelRegion {
            x: 5,
            y: 7,
            width,
            height,
        }
    }

    #[test]
    fn bands_tile_the_region_exactly() {
        for (width, height, workers) in
            [(675, 955, 8), (912, 1492, 4), (100, 3, 8), (4000, 6000, 8)]
        {
            let bands = split_bands(region(width, height), workers);
            let mut y = 7;
            for band in &bands {
                assert_eq!((band.x, band.y, band.width), (5, y, width));
                assert!(band.height > 0);
                y += band.height;
            }
            assert_eq!(y, 7 + height);
        }
    }

    #[test]
    fn small_renders_and_single_workers_are_not_cut() {
        assert_eq!(split_bands(region(100, 100), 8).len(), 1);
        assert_eq!(split_bands(region(4000, 6000), 1).len(), 1);
        // Large ones are cut finer than two per worker, to bound memory.
        assert!(split_bands(region(4000, 6000), 2).len() >= 23);
    }

    #[test]
    fn bands_draw_the_same_pixels_as_one_thread() {
        // Fills, strokes, a clip, translucency and text crossing the bands,
        // with edges on half pixels, where rounding could tell them apart.
        let mut doc = super::super::tests::doc_with_content(
            "0.9 0 0 rg 10 10.25 180 230.5 re f \
             0 0 1 RG 0.3 w 5 5 m 195 245 l S 2 w 1 J 20 240 m 180 15 l S \
             q 30 30.5 140 160 re W n 0 0.6 0 rg 0 0 200 250 re f Q \
             /T gs 0.2 g 60 20 m 140 230 l 40 200 l f \
             BT /F 30 Tf 15 120 Td (Bands) Tj ET",
            [0, 0, 200, 250],
        );
        let id = doc.collect_page_ids().unwrap()[0];
        let mut page = doc.resolve(id).unwrap().as_dict().unwrap().clone();
        page.insert(
            "Resources".into(),
            pdf_core::parser::Parser::new(
                b"<< /Font << /F << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> \
                  /ExtGState << /T << /ca 0.5 >> >> >>",
            )
            .parse_object()
            .unwrap(),
        );
        doc.set_object(id, PdfObject::Dictionary(page));
        let render = |threads| {
            render_page(
                &doc,
                0,
                RenderOptions {
                    size: RenderSize::FitBox {
                        width: 640,
                        height: 800,
                    },
                    threads,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let (one, four) = (render(1), render(4));
        let whole = PixelRegion {
            x: 0,
            y: 0,
            width: one.width,
            height: one.height,
        };
        assert!(split_bands(whole, 4).len() > 4);
        assert_eq!(one.pixels, four.pixels);
    }

    #[test]
    fn an_image_is_decoded_once_for_every_band() {
        let shared = Shared::default();
        let decodes = AtomicUsize::new(0);
        let id = ObjectId {
            number: 4,
            generation: 0,
        };
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let image = shared.image(id, || {
                        decodes.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        Some(DecodedImage {
                            width: 1,
                            height: 1,
                            rgb: vec![1, 2, 3],
                            gray: false,
                            alpha: Vec::new(),
                            is_stencil: false,
                            mask: None,
                        })
                    });
                    assert_eq!(image.unwrap().rgb, [1, 2, 3]);
                });
            }
        });
        assert_eq!(decodes.load(Ordering::Relaxed), 1);
    }
}
