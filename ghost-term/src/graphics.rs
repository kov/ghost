//! kitty graphics protocol — receiving, decoding and storing images.
//!
//! The VT parser recognises the protocol's APC carrier (`ESC _ G … ST`, see
//! [`crate::parser`]) and hands the payload to [`GraphicsState::handle`]. This
//! module parses the control data, reassembles chunked transfers, decodes the
//! pixels to RGBA8, stores them keyed by image id, and queues the protocol's
//! acknowledgement responses (which the host or frontend writes back to the
//! child's input — the same detached-host / attached-frontend split the cursor
//! and device-attribute queries use).
//!
//! This is the transmission + query half of the protocol. Placement (display),
//! deletion, animation and the long tail layer on top of this store later.
//!
//! Only **direct** transmission (`t=d`, base64 in-band) is accepted; the file,
//! temp-file and shared-memory mediums are refused with `ENOTSUPPORTED` — they
//! reference the session host's filesystem, which is an arbitrary-read hazard and
//! is meaningless to a display attached from another machine.

use std::collections::HashMap;

use base64::Engine;

/// Largest single decoded image we accept, in bytes (RGBA8). Bounds every
/// decode path against a single-command memory blow-up: the chunked-transfer
/// accumulator, the zlib inflate output, the raw pixel buffer, and (via
/// [`MAX_IMAGE_PIXELS`]) the PNG output allocation. A transfer that would exceed
/// it is refused, not buffered. 128 MiB ≈ a 32-megapixel RGBA image. Lowered
/// under `cfg(test)` so the bound tests exercise the same logic without feeding
/// (and allocating) hundreds of MiB.
#[cfg(not(test))]
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
#[cfg(test)]
const MAX_IMAGE_BYTES: usize = 512 * 1024;

/// Pixel-count cap implied by [`MAX_IMAGE_BYTES`] at 4 bytes/pixel — lets the PNG
/// path reject an enormous *declared* size before allocating its output buffer
/// (png's own byte limit covers row buffers, not the output buffer).
const MAX_IMAGE_PIXELS: u64 = (MAX_IMAGE_BYTES / 4) as u64;

/// Backstop on the image store so an endless stream of distinct transmits cannot
/// grow memory without bound (a child has no deletion path yet, and nothing is
/// evicted at scrollback). A proper kitty-style LRU quota replaces this refusal
/// in a later phase; for now we refuse once either budget is reached.
const MAX_STORED_BYTES: usize = 320 * 1024 * 1024;
const MAX_STORED_IMAGES: usize = 1024;

/// Backstop on the number of active placements per screen, so a child cannot
/// exhaust memory by creating endless distinct placements (`a=p`/`a=T` with
/// distinct `p=`) of even a single stored image. A real screen shows a handful
/// of images, so this is generous.
const MAX_PLACEMENTS: usize = 4096;

/// A decoded image: RGBA8 pixels in row-major order, `width` × `height`.
pub struct Image {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// `4 * width * height` bytes, RGBA, sRGB, top-left origin.
    pub pixels: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

/// A request to display an image at a screen location — what the renderer draws.
///
/// `row` is an **absolute** line index (`lines_scrolled_off + cursor row` at
/// placement time) so the image tracks its content under vertical scrolling; the
/// renderer maps it back to a viewport row. The anchor is only valid for the
/// buffer and width in effect at placement time — a column resize re-wraps
/// logical lines and shifts the absolute index, and placements are kept per
/// screen; full reflow stability lands with the Unicode-placeholder phase.
///
/// `cols`/`rows` are the explicit cell footprint (`c`/`r`) or 0 when the client
/// left sizing to the terminal (the renderer then derives it from the image and
/// cell pixel size, which this headless core does not know). `no_cursor_move` is
/// the `C=1` policy, recorded for the renderer phase that advances the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub image_id: u32,
    pub placement_id: u32,
    pub row: usize,
    pub col: usize,
    pub cols: u32,
    pub rows: u32,
    pub z: i32,
    pub no_cursor_move: bool,
}

/// The terminal's kitty-graphics state: the stored images, their placements, an
/// in-progress chunked transfer, and the queued responses awaiting transmission.
#[derive(Default)]
pub struct GraphicsState {
    images: HashMap<u32, Image>,
    /// Placements on the active screen. Images are global, but placements are
    /// per-screen (kitty semantics), so the alternate screen's are parked in
    /// `alternate_placements` and swapped in on the alt-buffer transition — a
    /// transient alt-screen excursion must not lose or misplace primary images.
    placements: Vec<Placement>,
    alternate_placements: Vec<Placement>,
    chunk: Option<Chunk>,
    /// Next candidate id when the terminal must allocate one (image-number `I=`
    /// transfers, or a transfer with neither `i` nor `I`).
    next_id: u32,
    /// Running sum of stored `pixels.len()`, kept in step with `images` so the
    /// storage budget (`MAX_STORED_BYTES`) is an O(1) check.
    stored_bytes: usize,
    /// Acknowledgement bytes queued for the child's input stream.
    responses: Vec<u8>,
}

impl std::fmt::Debug for GraphicsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsState")
            .field("images", &self.images.len())
            .field("placements", &self.placements.len())
            .field("chunking", &self.chunk.is_some())
            .field("pending_response_bytes", &self.responses.len())
            .finish()
    }
}

/// An in-progress chunked transfer: the first chunk's control data, the cursor
/// anchor captured when it began, and the raw (base64-decoded) bytes so far.
struct Chunk {
    control: Control,
    anchor: (usize, usize),
    data: Vec<u8>,
}

impl GraphicsState {
    /// Handle one graphics command (the APC payload after the leading `G`).
    ///
    /// `anchor` is the cursor as `(absolute_row, col)` — `absolute_row` being
    /// `lines_scrolled_off + cursor row` — recorded on any placement the command
    /// creates so the image tracks its content under scrolling. For a chunked
    /// transfer the anchor captured on the first chunk is the one used.
    pub fn handle(&mut self, payload: &str, anchor: (usize, usize)) {
        let (control_str, data) = payload.split_once(';').unwrap_or((payload, ""));

        // A continuation chunk carries only `m` (and optionally `q`); the real
        // control data lives on the first chunk we already stashed. An explicit
        // `q` on a later chunk still governs the final response.
        if self.chunk.is_some() {
            let cont = Control::parse(control_str);
            if cont.has_quiet {
                if let Some(chunk) = self.chunk.as_mut() {
                    chunk.control.quiet = cont.quiet;
                }
            }
            self.append_chunk(data, cont.more);
            return;
        }

        let control = Control::parse(control_str);

        // Actions that carry no image data dispatch before the decode path.
        match control.action {
            // Display an already-stored image.
            b'p' => {
                self.place(&control, anchor);
                return;
            }
            // Delete images/placements.
            b'd' => {
                self.delete(&control);
                return;
            }
            _ => {}
        }

        if control.more {
            // First chunk of a chunked transfer.
            match decode_base64(data) {
                Ok(bytes) if bytes.len() > MAX_IMAGE_BYTES => {
                    self.respond_error(&control, "EINVAL", "image transfer exceeds size limit");
                }
                Ok(bytes) => {
                    self.chunk = Some(Chunk {
                        control,
                        anchor,
                        data: bytes,
                    })
                }
                Err(()) => self.respond_error(&control, "EINVAL", "invalid base64 payload"),
            }
            return;
        }

        // Single-shot transmit/query command.
        match decode_base64(data) {
            Ok(bytes) => self.process(control, bytes, anchor),
            Err(()) => self.respond_error(&control, "EINVAL", "invalid base64 payload"),
        }
    }

    /// Drain the queued acknowledgement bytes for writing to the child's input.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    /// Look up a stored image by id.
    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    /// The number of stored images.
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// The placements the renderer should draw, in insertion order.
    pub fn placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.iter()
    }

    /// Serialize the stored images as kitty transmit escapes (`a=t`, RGBA), so
    /// feeding the result to a fresh terminal restores them. Used so images
    /// survive a reattach (the resync dump) and a replay from a recording
    /// checkpoint; placeholder cells, which carry only the id, can then resolve to
    /// pixels again. Images are emitted in id order for a deterministic dump, and
    /// the payload is chunked at the kitty 4096-byte limit with the control data
    /// on the first chunk only.
    pub fn dump(&self) -> String {
        use std::fmt::Write;

        let mut ids: Vec<u32> = self.images.keys().copied().collect();
        ids.sort_unstable();
        let mut out = String::new();
        for id in ids {
            let img = &self.images[&id];
            let b64 = base64::engine::general_purpose::STANDARD.encode(&img.pixels);
            let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let last = i == chunks.len() - 1;
                out.push_str("\u{1b}_G");
                if i == 0 {
                    let _ = write!(out, "i={id},a=t,f=32,s={},v={}", img.width, img.height);
                    if !last {
                        out.push_str(",m=1");
                    }
                } else {
                    out.push_str(if last { "m=0" } else { "m=1" });
                }
                out.push(';');
                // base64 is ASCII, so the chunk is valid UTF-8.
                out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
                out.push_str("\u{1b}\\");
            }
        }
        out
    }

    /// The number of active placements.
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    /// Park the active screen's placements and bring in the other screen's, on an
    /// alternate-buffer transition. Images are global; placements are per screen.
    pub fn swap_screen_placements(&mut self) {
        std::mem::swap(&mut self.placements, &mut self.alternate_placements);
    }

    /// Clear all graphics state (on RIS / hard reset).
    pub fn reset(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.alternate_placements.clear();
        self.stored_bytes = 0;
        self.chunk = None;
        // Pending responses are transient output; leave them to be drained.
    }

    fn append_chunk(&mut self, data: &str, more: bool) {
        let Some(chunk) = self.chunk.as_mut() else {
            return;
        };
        match decode_base64(data) {
            Ok(mut bytes) => chunk.data.append(&mut bytes),
            Err(()) => {
                let control = self.chunk.take().expect("chunk present").control;
                self.respond_error(&control, "EINVAL", "invalid base64 payload");
                return;
            }
        }
        // Bound the accumulator: the parser caps each APC, but a chunked transfer
        // is many APCs, so without this an endless `m=1` stream (or a huge image)
        // grows memory unbounded. Abort and free the buffer once over budget.
        if chunk.data.len() > MAX_IMAGE_BYTES {
            let control = self.chunk.take().expect("chunk present").control;
            self.respond_error(&control, "EINVAL", "image transfer exceeds size limit");
            return;
        }
        if !more {
            // Anchor the placement to where the transfer *began*, not the final
            // chunk's cursor.
            let chunk = self.chunk.take().expect("chunk present");
            self.process(chunk.control, chunk.data, chunk.anchor);
        }
    }

    /// Decode and store (or, for a query, just validate) a fully-received image,
    /// creating a placement at `anchor` when the action also displays it (a=T).
    fn process(&mut self, control: Control, mut raw: Vec<u8>, anchor: (usize, usize)) {
        if control.has_i && control.has_number {
            self.respond_error(&control, "EINVAL", "i and I are mutually exclusive");
            return;
        }
        if control.medium != b'd' {
            self.respond_error(
                &control,
                "ENOTSUPPORTED",
                "only direct transmission is supported",
            );
            return;
        }
        if control.compressed {
            // Bounded inflate: a few KiB of zlib can expand to gigabytes, so cap
            // the output rather than letting a decompression bomb OOM the host.
            match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&raw, MAX_IMAGE_BYTES) {
                Ok(inflated) => raw = inflated,
                Err(_) => {
                    self.respond_error(&control, "EINVAL", "invalid or oversized zlib data");
                    return;
                }
            }
        }

        let image = match decode_pixels(&control, &raw) {
            Ok(image) => image,
            Err(msg) => {
                self.respond_error(&control, "EINVAL", msg);
                return;
            }
        };

        // A query (a=q) validates the transfer without storing anything.
        if control.action == b'q' {
            self.respond_ok(&control, control.id);
            return;
        }

        let id = self.assign_id(&control);
        let new_bytes = image.2.len();
        let freed = self.images.get(&id).map_or(0, |old| old.pixels.len());
        let projected = self.stored_bytes.saturating_sub(freed) + new_bytes;
        // Storage backstop: refuse rather than grow the store without bound. A
        // re-transmit of an existing id replaces (it frees the old bytes first),
        // so it never trips the count limit. Replaced by an LRU quota later.
        if projected > MAX_STORED_BYTES
            || (!self.images.contains_key(&id) && self.images.len() >= MAX_STORED_IMAGES)
        {
            self.respond_error(&control, "ENOSPC", "image storage limit exceeded");
            return;
        }
        self.stored_bytes = projected;
        self.images.insert(
            id,
            Image {
                id,
                width: image.0,
                height: image.1,
                pixels: image.2,
            },
        );
        // a=T transmits *and* displays; a=t (and a=q, handled above) only stores.
        // If the placement budget is full the image is still stored — a=T's
        // success is the transmit, like kitty's lenient display behaviour.
        if control.action == b'T' {
            self.add_placement(id, &control, anchor);
        }
        self.respond_ok(&control, id);
    }

    /// Display an already-stored image (a=p). The image is looked up by `i=`; a
    /// missing image is `ENOENT`, and a full placement budget is `ENOSPC`.
    fn place(&mut self, control: &Control, anchor: (usize, usize)) {
        let id = control.id;
        if id == 0 || !self.images.contains_key(&id) {
            self.respond_error(control, "ENOENT", "image not found");
            return;
        }
        if !self.add_placement(id, control, anchor) {
            self.respond_error(control, "ENOSPC", "too many placements");
            return;
        }
        self.respond_ok(control, id);
    }

    /// Record (or replace) a placement of `image_id` at the cursor anchor. A
    /// placement is keyed by `(image_id, placement_id)`, so re-placing the same
    /// pair updates it rather than stacking duplicates. Returns `false` (without
    /// recording) when a *new* placement would exceed [`MAX_PLACEMENTS`].
    fn add_placement(&mut self, image_id: u32, control: &Control, anchor: (usize, usize)) -> bool {
        let placement_id = control.placement;
        // Remove any existing placement for this pair first, so a re-place is
        // always allowed and never blocked by the budget.
        self.placements
            .retain(|p| !(p.image_id == image_id && p.placement_id == placement_id));
        if self.placements.len() >= MAX_PLACEMENTS {
            return false;
        }
        let (row, col) = anchor;
        self.placements.push(Placement {
            image_id,
            placement_id,
            row,
            col,
            cols: control.cols,
            rows: control.rows,
            z: control.z,
            // C=1 means "do not move the cursor after placing"; recorded for the
            // renderer phase, which advances the cursor (it knows the cell size).
            no_cursor_move: control.cursor_move == 1,
        });
        true
    }

    /// Delete images and/or placements (a=d). This phase handles the id-scoped
    /// selectors the plan calls for — `d=a/A` (all) and `d=i/I` (by image id) —
    /// where an uppercase selector additionally frees the image's pixel data
    /// (lowercase keeps it for later re-display). Location/number selectors are
    /// left for a later phase.
    fn delete(&mut self, control: &Control) {
        let free_data = control.delete.is_ascii_uppercase();
        match control.delete.to_ascii_lowercase() {
            b'a' => {
                self.placements.clear();
                if free_data {
                    self.images.clear();
                    self.stored_bytes = 0;
                }
            }
            b'i' => {
                let id = control.id;
                if id == 0 {
                    return; // a by-id delete needs i=
                }
                if control.placement != 0 {
                    self.placements
                        .retain(|p| !(p.image_id == id && p.placement_id == control.placement));
                } else {
                    self.placements.retain(|p| p.image_id != id);
                }
                // Uppercase frees the image data — but only once the image has no
                // remaining placements (a placement-scoped delete may leave some).
                if free_data && !self.placements.iter().any(|p| p.image_id == id) {
                    self.remove_image(id);
                }
            }
            // Number- and location-scoped deletes are not handled yet.
            _ => {}
        }
        // Deletes carry no acknowledgement.
    }

    /// Remove a stored image and keep `stored_bytes` in step.
    fn remove_image(&mut self, id: u32) {
        if let Some(image) = self.images.remove(&id) {
            self.stored_bytes = self.stored_bytes.saturating_sub(image.pixels.len());
        }
    }

    /// Resolve the id to store under: the client-specified `i`, or a freshly
    /// allocated id (for an image-number transfer or an unkeyed one).
    fn assign_id(&mut self, control: &Control) -> u32 {
        if control.id != 0 {
            return control.id;
        }
        let mut id = self.next_id.max(1);
        while self.images.contains_key(&id) {
            id = id.wrapping_add(1).max(1);
        }
        self.next_id = id.wrapping_add(1).max(1);
        id
    }

    fn respond_ok(&mut self, control: &Control, id: u32) {
        // q=1 suppresses OK responses. A response can only be matched if the
        // command was keyed by an image id or number.
        if control.quiet == 1 || (!control.has_i && !control.has_number) {
            return;
        }
        self.push_response(control, id, "OK");
    }

    fn respond_error(&mut self, control: &Control, code: &str, msg: &str) {
        // q=2 suppresses error responses; an unkeyed command (no id/number) has
        // nothing to match a reply against, so — like an unkeyed success — it
        // stays silent rather than emitting an unmatchable `i=0` reply.
        if control.quiet == 2 || (!control.has_i && !control.has_number) {
            return;
        }
        self.push_response(control, control.id, &format!("{code}:{msg}"));
    }

    /// Queue `ESC _ G i=<id>[,I=<number>][,p=<placement>] ; <body> ST`.
    fn push_response(&mut self, control: &Control, id: u32, body: &str) {
        let mut keys = format!("i={id}");
        if let Some(number) = control.number {
            keys.push_str(&format!(",I={number}"));
        }
        if control.placement != 0 {
            keys.push_str(&format!(",p={}", control.placement));
        }
        self.responses
            .extend_from_slice(format!("\x1b_G{keys};{body}\x1b\\").as_bytes());
    }
}

/// Parsed control data (key=value, comma-separated), with kitty's defaults.
struct Control {
    action: u8,
    format: u32,
    medium: u8,
    compressed: bool,
    more: bool,
    id: u32,
    has_i: bool,
    number: Option<u32>,
    has_number: bool,
    placement: u32,
    width: u32,
    height: u32,
    quiet: u8,
    has_quiet: bool,
    /// Display size in cells (`c`/`r`); 0 = unspecified (the renderer derives it
    /// from the image and cell pixel size, which the headless core does not know).
    cols: u32,
    rows: u32,
    /// Z-index (`z`) and cursor-movement policy (`C`: 1 = leave the cursor put).
    z: i32,
    cursor_move: u8,
    /// Delete selector (`d`); uppercase additionally frees the image data.
    delete: u8,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            action: b't', // transmit
            format: 32,   // RGBA
            medium: b'd', // direct
            compressed: false,
            more: false,
            id: 0,
            has_i: false,
            number: None,
            has_number: false,
            placement: 0,
            width: 0,
            height: 0,
            quiet: 0,
            has_quiet: false,
            cols: 0,
            rows: 0,
            z: 0,
            cursor_move: 0,
            delete: b'a', // a=d with no d= deletes all placements
        }
    }
}

impl Control {
    fn parse(s: &str) -> Control {
        let mut c = Control::default();
        for pair in s.split(',') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "a" => c.action = value.bytes().next().unwrap_or(b't'),
                "f" => c.format = value.parse().unwrap_or(32),
                "t" => c.medium = value.bytes().next().unwrap_or(b'd'),
                "o" => c.compressed = value == "z",
                "m" => c.more = value == "1",
                "i" => {
                    c.id = value.parse().unwrap_or(0);
                    // i=0 means "unset" per the spec, so it does not key a reply.
                    c.has_i = c.id != 0;
                }
                "I" => {
                    c.number = value.parse().ok();
                    c.has_number = true;
                }
                "p" => c.placement = value.parse().unwrap_or(0),
                "s" => c.width = value.parse().unwrap_or(0),
                "v" => c.height = value.parse().unwrap_or(0),
                "q" => {
                    c.quiet = value.parse().unwrap_or(0);
                    c.has_quiet = true;
                }
                "c" => c.cols = value.parse().unwrap_or(0),
                "r" => c.rows = value.parse().unwrap_or(0),
                "z" => c.z = value.parse().unwrap_or(0),
                "C" => c.cursor_move = value.parse().unwrap_or(0),
                "d" => c.delete = value.bytes().next().unwrap_or(b'a'),
                // Keys this phase does not act on (source crop x/y/w/h, in-cell
                // offsets X/Y, relative placement, animation): defaults apply.
                _ => {}
            }
        }
        c
    }
}

fn decode_base64(s: &str) -> Result<Vec<u8>, ()> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|_| ())
}

/// Decode the raw (post-base64, post-decompression) bytes into RGBA8 pixels.
/// Returns `(width, height, rgba)`.
fn decode_pixels(control: &Control, raw: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    match control.format {
        100 => decode_png(raw),
        24 => decode_raw(control, raw, 3),
        32 => decode_raw(control, raw, 4),
        _ => Err("unsupported pixel format"),
    }
}

/// Decode raw RGB (3 bpp) or RGBA (4 bpp) pixel data into RGBA8.
fn decode_raw(
    control: &Control,
    raw: &[u8],
    channels: usize,
) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let (w, h) = (control.width, control.height);
    if w == 0 || h == 0 {
        return Err("missing image dimensions");
    }
    let pixels = (w as usize)
        .checked_mul(h as usize)
        .ok_or("image dimensions overflow")?;
    let expected = pixels
        .checked_mul(channels)
        .ok_or("image dimensions overflow")?;
    if expected > MAX_IMAGE_BYTES {
        return Err("image dimensions exceed size limit");
    }
    if raw.len() != expected {
        return Err("pixel data size does not match dimensions");
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    for px in raw.chunks_exact(channels) {
        rgba.extend_from_slice(&px[..3]);
        rgba.push(if channels == 4 { px[3] } else { 255 });
    }
    Ok((w, h, rgba))
}

/// Decode a PNG (any common colour type / bit depth) into RGBA8.
fn decode_png(raw: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(raw));
    // Expand palette / sub-byte greyscale to 8-bit channels and collapse 16-bit
    // samples to 8-bit, so we only have to fan four colour types out to RGBA.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|_| "invalid PNG")?;
    // Reject a decompression-free allocation bomb: a tiny PNG can declare enormous
    // dimensions, and `output_buffer_size()` is bounded only by `isize::MAX` (png's
    // byte limit covers intermediate row buffers, not the output buffer). Cap the
    // declared pixel count before allocating.
    let (w, h) = reader.info().size();
    if (w as u64)
        .checked_mul(h as u64)
        .is_none_or(|px| px > MAX_IMAGE_PIXELS)
    {
        return Err("PNG dimensions exceed size limit");
    }
    let buf_size = reader
        .output_buffer_size()
        .ok_or("PNG dimensions exceed size limit")?;
    let mut buf = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buf).map_err(|_| "invalid PNG")?;
    buf.truncate(info.buffer_size());

    let (w, h) = (info.width, info.height);
    let count = (w as usize) * (h as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(count * 4);
            for px in buf.chunks_exact(3) {
                out.extend_from_slice(px);
                out.push(255);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(count * 4);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(count * 4);
            for ga in buf.chunks_exact(2) {
                out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            out
        }
        // EXPAND turns Indexed into Rgb/Rgba, so it should not reach here.
        png::ColorType::Indexed => return Err("unsupported PNG colour type"),
    };
    Ok((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `t=d, f=24` (RGB) transmit command's APC payload (as `handle` receives
    /// it — i.e. without the leading `G` the parser strips) for a `w`×`h` image.
    fn transmit_rgb(id: u32, w: u32, h: u32, raw: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        format!("i={id},a=t,f=24,s={w},v={h};{b64}")
    }

    /// Feed a command at the origin anchor (these tests don't exercise placement).
    fn feed(g: &mut GraphicsState, payload: &str) {
        g.handle(payload, (0, 0));
    }

    #[test]
    fn stored_bytes_tracks_the_store_and_does_not_double_count_replaces() {
        let mut g = GraphicsState::default();

        // Two distinct 1×1 RGB images -> 4 RGBA bytes each stored.
        feed(&mut g, &transmit_rgb(1, 1, 1, &[1, 2, 3]));
        feed(&mut g, &transmit_rgb(2, 1, 1, &[4, 5, 6]));
        assert_eq!(g.image_count(), 2);
        assert_eq!(g.stored_bytes, 8);

        // Re-transmitting id 1 with a bigger 2×1 image replaces it: the old 4
        // bytes are freed first, so the count holds and bytes reflect the new size.
        feed(&mut g, &transmit_rgb(1, 2, 1, &[1, 2, 3, 4, 5, 6]));
        assert_eq!(g.image_count(), 2);
        assert_eq!(g.stored_bytes, 8 + 4); // image 1 now 8 bytes, image 2 still 4

        g.reset();
        assert_eq!(g.image_count(), 0);
        assert_eq!(g.stored_bytes, 0);
    }

    #[test]
    fn zlib_bomb_is_refused_by_the_bounded_inflate() {
        let mut g = GraphicsState::default();

        // A buffer that decompresses to just over the per-image cap: a few hundred
        // bytes of zlib that the bounded inflate must reject rather than expand.
        let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; MAX_IMAGE_BYTES + 1], 6);
        let payload = base64::engine::general_purpose::STANDARD.encode(&bomb);
        feed(&mut g, &format!("i=2,a=t,o=z,f=32,s=1,v=1;{payload}"));

        let response = String::from_utf8(g.take_responses()).unwrap();
        assert!(response.contains("EINVAL"), "got {response:?}");
        assert_eq!(g.image_count(), 0);
    }

    #[test]
    fn unterminated_chunk_stream_aborts_at_the_cap_and_frees_the_buffer() {
        let mut g = GraphicsState::default();

        // 64 KiB of raw zeros per chunk (base64 of all-'A'), via m=1 forever. Once
        // the accumulator passes the cap it must abort with EINVAL and drop the
        // buffer instead of growing without bound.
        let raw_per_chunk = 64 * 1024;
        let chunk_b64 = "A".repeat(raw_per_chunk / 3 * 4);
        feed(
            &mut g,
            &format!("i=1,a=t,f=32,s=4096,v=4096,m=1;{chunk_b64}"),
        );

        let mut aborted = false;
        for _ in 0..(MAX_IMAGE_BYTES / raw_per_chunk + 4) {
            feed(&mut g, &format!("m=1;{chunk_b64}"));
            if String::from_utf8(g.take_responses())
                .unwrap()
                .contains("EINVAL")
            {
                aborted = true;
                break;
            }
        }
        assert!(aborted, "over-cap chunk stream was not refused");
        assert!(
            g.chunk.is_none(),
            "the aborted transfer's buffer was not freed"
        );

        // A fresh single-shot transfer still works after the abort.
        feed(&mut g, &transmit_rgb(3, 2, 1, &[255, 0, 0, 0, 255, 0]));
        assert!(g.image(3).is_some());
    }

    #[test]
    fn placements_are_bounded() {
        let mut g = GraphicsState::default();
        feed(&mut g, &transmit_rgb(1, 1, 1, &[1, 2, 3])); // store one image
        let _ = g.take_responses();

        // Distinct placement ids of the one image must cap, not grow forever.
        for pid in 1..=(MAX_PLACEMENTS as u32 + 1) {
            g.handle(&format!("i=1,a=p,p={pid}"), (0, 0));
        }

        assert_eq!(g.placement_count(), MAX_PLACEMENTS);
        // The overflowing placement was refused with ENOSPC.
        let responses = String::from_utf8(g.take_responses()).unwrap();
        assert!(responses.contains("ENOSPC"), "expected an ENOSPC refusal");
    }
}
