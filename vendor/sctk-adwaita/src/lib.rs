use std::error::Error;
use std::mem;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use tiny_skia::{
    Color, FillRule, Mask, Path, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Point, Rect, Shader,
    Transform,
};

use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::protocol::wl_shm;
use smithay_client_toolkit::reexports::client::protocol::wl_subsurface::WlSubsurface;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::csd_frame::{
    CursorIcon, DecorationsFrame, FrameAction, FrameClick, WindowManagerCapabilities, WindowState,
};

use smithay_client_toolkit::compositor::{CompositorState, Region, SurfaceData};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::{slot::SlotPool, Shm};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::subcompositor::SubsurfaceData;

mod buttons;
mod config;
mod parts;
mod pointer;
pub mod shadow;
pub mod theme;
mod title;
mod wl_typed;

use crate::theme::{
    ColorMap, ColorTheme, BORDER_SIZE, CORNER_RADIUS, HEADER_SIZE, RESIZE_HANDLE_CORNER_SIZE,
    VISIBLE_BORDER_SIZE,
};

use buttons::Buttons;
use config::get_button_layout_config;
use parts::DecorationParts;
use pointer::{Location, MouseState};
use shadow::Shadow;
use title::TitleText;
use wl_typed::WlTyped;

/// XXX this is not result, so `must_use` when needed.
type SkiaResult = Option<()>;

/// A simple set of decorations
#[derive(Debug)]
pub struct AdwaitaFrame<State> {
    /// The base surface used to create the window.
    base_surface: WlTyped<WlSurface, SurfaceData>,

    compositor: Arc<CompositorState>,

    /// Subcompositor to create/drop subsurfaces ondemand.
    subcompositor: Arc<SubcompositorState>,

    /// Queue handle to perform object creation.
    queue_handle: QueueHandle<State>,

    /// The drawable decorations, `None` when hidden.
    decorations: Option<DecorationParts>,

    /// Memory pool to allocate the buffers for the decorations.
    pool: SlotPool,

    /// Whether the frame should be redrawn.
    dirty: bool,

    /// Whether the drawing should be synced with the main surface.
    should_sync: bool,

    /// Scale factor used for the surface.
    scale_factor: u32,

    /// Wether the frame is resizable.
    resizable: bool,

    buttons: Buttons,
    state: WindowState,
    wm_capabilities: WindowManagerCapabilities,
    mouse: MouseState,
    theme: ColorTheme,
    title: Option<String>,
    title_text: Option<TitleText>,
    shadow: Shadow,
}

impl<State> AdwaitaFrame<State>
where
    State: Dispatch<WlSurface, SurfaceData> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
{
    pub fn new(
        base_surface: &impl WaylandSurface,
        shm: &Shm,
        compositor: Arc<CompositorState>,
        subcompositor: Arc<SubcompositorState>,
        queue_handle: QueueHandle<State>,
        frame_config: FrameConfig,
    ) -> Result<Self, Box<dyn Error>> {
        let base_surface = WlTyped::wrap::<State>(base_surface.wl_surface().clone());

        let pool = SlotPool::new(1, shm)?;

        let decorations = Some(DecorationParts::new(
            &base_surface,
            &subcompositor,
            &queue_handle,
        ));

        let theme = frame_config.theme;

        Ok(AdwaitaFrame {
            base_surface,
            decorations,
            pool,
            compositor,
            subcompositor,
            queue_handle,
            dirty: true,
            scale_factor: 1,
            should_sync: true,
            title: None,
            title_text: TitleText::new(theme.active.font_color),
            theme,
            buttons: Buttons::new(get_button_layout_config()),
            mouse: Default::default(),
            state: WindowState::empty(),
            wm_capabilities: WindowManagerCapabilities::all(),
            resizable: true,
            shadow: Shadow::default(),
        })
    }

    /// Update the current frame config.
    pub fn set_config(&mut self, config: FrameConfig) {
        self.theme = config.theme;
        self.dirty = true;
    }

    fn precise_location(
        &self,
        location: Location,
        decoration: &DecorationParts,
        x: f64,
        y: f64,
    ) -> Location {
        let header_width = decoration.header().surface_rect.width;
        let side_height = decoration.side_height();

        let left_corner_x = BORDER_SIZE + RESIZE_HANDLE_CORNER_SIZE;
        let right_corner_x = (header_width + BORDER_SIZE).saturating_sub(RESIZE_HANDLE_CORNER_SIZE);
        let top_corner_y = RESIZE_HANDLE_CORNER_SIZE;
        let bottom_corner_y = side_height.saturating_sub(RESIZE_HANDLE_CORNER_SIZE);
        match location {
            Location::Head | Location::Button(_) => self.buttons.find_button(x, y),
            Location::Top | Location::TopLeft | Location::TopRight => {
                if x <= f64::from(left_corner_x) {
                    Location::TopLeft
                } else if x >= f64::from(right_corner_x) {
                    Location::TopRight
                } else {
                    Location::Top
                }
            }
            Location::Bottom | Location::BottomLeft | Location::BottomRight => {
                if x <= f64::from(left_corner_x) {
                    Location::BottomLeft
                } else if x >= f64::from(right_corner_x) {
                    Location::BottomRight
                } else {
                    Location::Bottom
                }
            }
            Location::Left => {
                if y <= f64::from(top_corner_y) {
                    Location::TopLeft
                } else if y >= f64::from(bottom_corner_y) {
                    Location::BottomLeft
                } else {
                    Location::Left
                }
            }
            Location::Right => {
                if y <= f64::from(top_corner_y) {
                    Location::TopRight
                } else if y >= f64::from(bottom_corner_y) {
                    Location::BottomRight
                } else {
                    Location::Right
                }
            }
            other => other,
        }
    }

    fn redraw_inner(&mut self) -> Option<bool> {
        let decorations = self.decorations.as_mut()?;

        // Reset the dirty bit.
        self.dirty = false;
        let should_sync = mem::take(&mut self.should_sync);

        // Don't draw borders if the frame explicitly hidden or fullscreened.
        if self.state.contains(WindowState::FULLSCREEN) {
            decorations.hide();
            return Some(true);
        }

        let colors = if self.state.contains(WindowState::ACTIVATED) {
            &self.theme.active
        } else {
            &self.theme.inactive
        };

        let draw_borders = if self.state.contains(WindowState::MAXIMIZED) {
            // Don't draw the borders.
            decorations.hide_borders();
            false
        } else {
            true
        };

        // Draw the borders.
        for (idx, part) in decorations
            .parts()
            .filter(|(idx, _)| *idx == DecorationParts::HEADER || draw_borders)
        {
            let scale = self.scale_factor;

            let mut rect = part.surface_rect;
            // XXX to perfectly align the visible borders we draw them with
            // the header, otherwise rounded corners won't look 'smooth' at the
            // start. To achieve that, we enlargen the width of the header by
            // 2 * `VISIBLE_BORDER_SIZE`, and move `x` by `VISIBLE_BORDER_SIZE`
            // to the left.
            if idx == DecorationParts::HEADER && draw_borders {
                rect.width += 2 * VISIBLE_BORDER_SIZE;
                rect.x -= VISIBLE_BORDER_SIZE as i32;
            }

            rect.width *= scale;
            rect.height *= scale;

            let (buffer, canvas) = match self.pool.create_buffer(
                rect.width as i32,
                rect.height as i32,
                rect.width as i32 * 4,
                wl_shm::Format::Argb8888,
            ) {
                Ok((buffer, canvas)) => (buffer, canvas),
                Err(_) => continue,
            };

            // Create the pixmap and fill with transparent color.
            let mut pixmap = PixmapMut::from_bytes(canvas, rect.width, rect.height)?;

            // Fill everything with transparent background, since we draw rounded corners and
            // do invisible borders to enlarge the input zone.
            pixmap.fill(Color::TRANSPARENT);

            if !self.state.intersects(WindowState::TILED) {
                self.shadow.draw(
                    &mut pixmap,
                    scale,
                    self.state.contains(WindowState::ACTIVATED),
                    idx,
                );
            }

            match idx {
                DecorationParts::HEADER => {
                    if let Some(title_text) = self.title_text.as_mut() {
                        title_text.update_scale(scale);
                        title_text.update_color(colors.font_color);
                    }

                    draw_headerbar(
                        &mut pixmap,
                        self.title_text.as_ref().map(|t| t.pixmap()).unwrap_or(None),
                        scale as f32,
                        self.resizable,
                        &self.state,
                        &self.theme,
                        &self.buttons,
                        self.mouse.location,
                    );
                }
                border => {
                    let rounded = !self.state.intersects(WindowState::TILED);
                    let border_rect = visible_border_rect(border, rect, scale, rounded);

                    // Fill the visible border, if present. It sits over the
                    // shadow's outermost pixel and continues the ring the header
                    // draws around itself, so the window has one outline from
                    // top to bottom.
                    if let Some(border_rect) = border_rect {
                        pixmap.fill_rect(
                            border_rect,
                            &colors.outer_border_paint(),
                            Transform::identity(),
                            None,
                        );
                    }

                    // ...and hand the corners over to the client's arc without a
                    // step, by fading out over the share of the ring that is
                    // still in this column.
                    for (slice, coverage) in corner_border_taper(border, rect, scale, rounded) {
                        let mut paint = colors.outer_border_paint();
                        if let Shader::SolidColor(color) = &mut paint.shader {
                            color.apply_opacity(coverage);
                        }
                        pixmap.fill_rect(slice, &paint, Transform::identity(), None);
                    }
                }
            };

            // Everything above drew into a `tiny_skia` pixmap, which is RGBA in
            // memory; the buffer under it is `Argb8888`, which on a
            // little-endian host is BGRA. Upstream never had to care — every
            // colour Adwaita gave this frame was a neutral grey, and a grey
            // survives the swap — but a tinted one arrives with its red and blue
            // exchanged.
            swap_red_and_blue(pixmap.data_mut());

            if should_sync {
                part.subsurface.set_sync();
            } else {
                part.subsurface.set_desync();
            }

            part.surface.set_buffer_scale(scale as i32);

            part.subsurface.set_position(rect.x, rect.y);
            buffer.attach_to(&part.surface).ok()?;

            if part.surface.version() >= 4 {
                part.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            } else {
                part.surface.damage(0, 0, i32::MAX, i32::MAX);
            }

            if let Some(input_rect) = part.input_rect {
                let input_region = Region::new(&*self.compositor).ok()?;
                input_region.add(
                    input_rect.x,
                    input_rect.y,
                    input_rect.width as i32,
                    input_rect.height as i32,
                );

                part.surface
                    .set_input_region(Some(input_region.wl_region()));
            }

            part.surface.commit();
        }

        Some(should_sync)
    }
}

impl<State> DecorationsFrame for AdwaitaFrame<State>
where
    State: Dispatch<WlSurface, SurfaceData> + Dispatch<WlSubsurface, SubsurfaceData> + 'static,
{
    fn update_state(&mut self, state: WindowState) {
        let difference = self.state.symmetric_difference(state);
        self.state = state;
        self.dirty |= difference.intersects(
            WindowState::ACTIVATED
                | WindowState::FULLSCREEN
                | WindowState::MAXIMIZED
                | WindowState::TILED,
        );
    }

    fn update_wm_capabilities(&mut self, wm_capabilities: WindowManagerCapabilities) {
        self.dirty |= self.wm_capabilities != wm_capabilities;
        self.wm_capabilities = wm_capabilities;
        self.buttons.update_wm_capabilities(wm_capabilities);
    }

    fn set_hidden(&mut self, hidden: bool) {
        if hidden {
            self.dirty = false;
            let _ = self.pool.resize(1);
            self.decorations = None;
        } else if self.decorations.is_none() {
            self.decorations = Some(DecorationParts::new(
                &self.base_surface,
                &self.subcompositor,
                &self.queue_handle,
            ));
            self.dirty = true;
            self.should_sync = true;
        }
    }

    fn set_resizable(&mut self, resizable: bool) {
        self.dirty |= self.resizable != resizable;
        self.resizable = resizable;
    }

    fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        let Some(decorations) = self.decorations.as_mut() else {
            log::error!("trying to resize the hidden frame.");
            return;
        };

        decorations.resize(width.get(), height.get());
        self.buttons
            .arrange(width.get(), get_margin_h_lp(&self.state));
        self.dirty = true;
        self.should_sync = true;
    }

    fn draw(&mut self) -> bool {
        self.redraw_inner().unwrap_or(true)
    }

    fn subtract_borders(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        if self.decorations.is_none() || self.state.contains(WindowState::FULLSCREEN) {
            (Some(width), Some(height))
        } else {
            (
                Some(width),
                NonZeroU32::new(height.get().saturating_sub(HEADER_SIZE)),
            )
        }
    }

    fn add_borders(&self, width: u32, height: u32) -> (u32, u32) {
        if self.decorations.is_none() || self.state.contains(WindowState::FULLSCREEN) {
            (width, height)
        } else {
            (width, height + HEADER_SIZE)
        }
    }

    fn location(&self) -> (i32, i32) {
        if self.decorations.is_none() || self.state.contains(WindowState::FULLSCREEN) {
            (0, 0)
        } else {
            (0, -(HEADER_SIZE as i32))
        }
    }

    fn set_title(&mut self, title: impl Into<String>) {
        let new_title = title.into();
        if let Some(title_text) = self.title_text.as_mut() {
            title_text.update_title(new_title.clone());
        }

        self.title = Some(new_title);
        self.dirty = true;
    }

    fn on_click(
        &mut self,
        timestamp: Duration,
        click: FrameClick,
        pressed: bool,
    ) -> Option<FrameAction> {
        match click {
            FrameClick::Normal => self.mouse.click(
                timestamp,
                pressed,
                self.resizable,
                &self.state,
                &self.wm_capabilities,
            ),
            FrameClick::Alternate => self.mouse.alternate_click(pressed, &self.wm_capabilities),
            _ => None,
        }
    }

    fn set_scaling_factor(&mut self, scale_factor: f64) {
        // NOTE: Clamp it just in case to some ok-ish range.
        self.scale_factor = scale_factor.clamp(0.1, 64.).ceil() as u32;
        self.dirty = true;
        self.should_sync = true;
    }

    fn click_point_moved(
        &mut self,
        _timestamp: Duration,
        surface: &ObjectId,
        x: f64,
        y: f64,
    ) -> Option<CursorIcon> {
        let decorations = self.decorations.as_ref()?;
        let location = decorations.find_surface(surface);
        if location == Location::None {
            return None;
        }

        let old_location = self.mouse.location;

        let location = self.precise_location(location, decorations, x, y);
        let new_cursor = self.mouse.moved(location, x, y, self.resizable);

        // Set dirty if we moved the cursor between the buttons.
        self.dirty |= (matches!(old_location, Location::Button(_))
            || matches!(self.mouse.location, Location::Button(_)))
            && old_location != self.mouse.location;

        Some(new_cursor)
    }

    fn click_point_left(&mut self) {
        self.mouse.left()
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn is_hidden(&self) -> bool {
        self.decorations.is_none()
    }
}

/// The configuration for the [`AdwaitaFrame`] frame.
#[derive(Debug, Clone)]
pub struct FrameConfig {
    pub theme: ColorTheme,
}

impl FrameConfig {
    /// Create the new configuration with the given `theme`.
    pub fn new(theme: ColorTheme) -> Self {
        Self { theme }
    }

    /// This is equivalent of calling `FrameConfig::new(ColorTheme::auto())`.
    ///
    /// For details see [`ColorTheme::auto`].
    pub fn auto() -> Self {
        Self {
            theme: ColorTheme::auto(),
        }
    }

    /// This is equivalent of calling `FrameConfig::new(ColorTheme::light())`.
    ///
    /// For details see [`ColorTheme::light`].
    pub fn light() -> Self {
        Self {
            theme: ColorTheme::light(),
        }
    }

    /// This is equivalent of calling `FrameConfig::new(ColorTheme::dark())`.
    ///
    /// For details see [`ColorTheme::dark`].
    pub fn dark() -> Self {
        Self {
            theme: ColorTheme::dark(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_headerbar(
    pixmap: &mut PixmapMut,
    text_pixmap: Option<&Pixmap>,
    scale: f32,
    resizable: bool,
    state: &WindowState,
    theme: &ColorTheme,
    buttons: &Buttons,
    mouse: Location,
) {
    let colors = theme.for_state(state.contains(WindowState::ACTIVATED));

    let _ = draw_headerbar_bg(pixmap, scale, colors, state);

    // Horizontal margin.
    let margin_h = get_margin_h_lp(state) * 2.0;

    let canvas_w = pixmap.width() as f32;
    let canvas_h = pixmap.height() as f32;

    let header_w = canvas_w - margin_h * 2.0;
    let header_h = canvas_h;

    if let Some(text_pixmap) = text_pixmap {
        const TEXT_OFFSET: f32 = 10.;
        let offset_x = TEXT_OFFSET * scale;

        let text_w = text_pixmap.width() as f32;
        let text_h = text_pixmap.height() as f32;

        let x = margin_h + header_w / 2. - text_w / 2.;
        let y = header_h / 2. - text_h / 2.;

        let left_buttons_end_x = buttons.left_buttons_end_x().unwrap_or(0.0) * scale;
        let right_buttons_start_x =
            buttons.right_buttons_start_x().unwrap_or(header_w / scale) * scale;

        {
            // We have enough space to center text
            let (x, y, text_canvas_start_x) = if (x + text_w < right_buttons_start_x - offset_x)
                && (x > left_buttons_end_x + offset_x)
            {
                let text_canvas_start_x = x;

                (x, y, text_canvas_start_x)
            } else {
                let x = left_buttons_end_x + offset_x;
                let text_canvas_start_x = left_buttons_end_x + offset_x;

                (x, y, text_canvas_start_x)
            };

            let text_canvas_end_x = right_buttons_start_x - x - offset_x;
            // Ensure that text start within the bounds.
            let x = x.max(margin_h + offset_x);

            if let Some(clip) =
                Rect::from_xywh(text_canvas_start_x, 0., text_canvas_end_x, canvas_h)
            {
                if let Some(mut mask) = Mask::new(canvas_w as u32, canvas_h as u32) {
                    mask.fill_path(
                        &PathBuilder::from_rect(clip),
                        FillRule::Winding,
                        false,
                        Transform::identity(),
                    );
                    pixmap.draw_pixmap(
                        x.round() as i32,
                        y as i32,
                        text_pixmap.as_ref(),
                        &PixmapPaint::default(),
                        Transform::identity(),
                        Some(&mask),
                    );
                } else {
                    log::error!(
                        "Invalid mask width and height: w: {}, h: {}",
                        canvas_w as u32,
                        canvas_h as u32
                    );
                }
            }
        }
    }

    // Draw the buttons.
    buttons.draw(
        margin_h, header_w, scale, colors, mouse, pixmap, resizable, state,
    );
}

#[must_use]
fn draw_headerbar_bg(
    pixmap: &mut PixmapMut,
    scale: f32,
    colors: &ColorMap,
    state: &WindowState,
) -> SkiaResult {
    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;

    let radius = if state.intersects(WindowState::MAXIMIZED | WindowState::TILED) {
        0.
    } else {
        CORNER_RADIUS as f32 * scale
    };

    // The header is drawn one visible border wider than the window on each side
    // (see `redraw_inner`), so that ring of pixels is the header's share of the
    // window's outer border. Lay the border down over the whole shape and let
    // the headerbar fill cover all but that ring — this is the line the border
    // parts continue straight down the sides of the window, and without it the
    // window's outline would stop where the headerbar ends.
    let ring = get_margin_h_lp(state) * scale;

    let bg = rounded_headerbar_shape(0., 0., w, h, radius)?;

    pixmap.fill_path(
        &bg,
        &colors.outer_border_paint(),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let inner = rounded_headerbar_shape(ring, ring, w - 2. * ring, h - ring, radius - ring)?;

    pixmap.fill_path(
        &inner,
        &colors.headerbar_paint(),
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    pixmap.fill_rect(
        Rect::from_xywh(ring, h - 1., w - 2. * ring, 1.)?,
        &colors.border_paint(),
        Transform::identity(),
        None,
    );

    Some(())
}

fn rounded_headerbar_shape(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<Path> {
    // https://stackoverflow.com/a/27863181
    let cubic_bezier_circle = 0.552_284_8 * radius;

    let mut pb = PathBuilder::new();
    let mut cursor = Point::from_xy(x, y);

    // !!!
    // This code is heavily "inspired" by https://gitlab.com/snakedye/snui/
    // So technically it should be licensed under MPL-2.0, sorry about that 🥺 👉👈
    // !!!

    // Positioning the cursor
    cursor.y += radius;
    pb.move_to(cursor.x, cursor.y);

    // Drawing the outline
    let next = Point::from_xy(cursor.x + radius, cursor.y - radius);
    pb.cubic_to(
        cursor.x,
        cursor.y - cubic_bezier_circle,
        next.x - cubic_bezier_circle,
        next.y,
        next.x,
        next.y,
    );
    cursor = next;
    pb.line_to(
        {
            cursor.x = x + width - radius;
            cursor.x
        },
        cursor.y,
    );
    let next = Point::from_xy(cursor.x + radius, cursor.y + radius);
    pb.cubic_to(
        cursor.x + cubic_bezier_circle,
        cursor.y,
        next.x,
        next.y - cubic_bezier_circle,
        next.x,
        next.y,
    );
    cursor = next;
    pb.line_to(cursor.x, {
        cursor.y = y + height;
        cursor.y
    });
    pb.line_to(
        {
            cursor.x = x;
            cursor.x
        },
        cursor.y,
    );

    pb.close();

    pb.finish()
}

/// Exchange the red and blue channel of every pixel in place: the one step from
/// `tiny_skia`'s RGBA to the `Argb8888` a Wayland buffer wants.
///
/// Premultiplication is untouched — it scales all three colour channels alike,
/// so swapping two of them is the same before and after.
fn swap_red_and_blue(data: &mut [u8]) {
    for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
}

/// How far short of a bottom corner a straight border has to stop, for a corner
/// of `radius` device pixels.
///
/// Not the radius itself: this border runs one pixel *outside* the window, so it
/// is a chord of the ring at radius + 1, and it stays straight for as long as
/// that ring is still within half a pixel of the window's edge. Cutting it at the
/// full radius instead leaves a visible nick at each end of the bottom edge,
/// where neither this border nor the client's arc has drawn anything.
/// How much of the frame's border column the window's outer ring still fills,
/// `along` device pixels from where the corner's arc begins.
///
/// One ring runs around the whole window: the band between `radius` and
/// `radius + 1` out from each corner's centre, which on the straight edges is
/// exactly the column this frame paints. At the tangent the two are the same
/// pixel and the column is full; as the arc curves away the ring slides off it
/// and into the client's own pixels, which draw the rest. Fading the column out
/// by the share that is still its own is what keeps the line continuous across
/// the handover instead of ending on a step.
fn corner_border_coverage(radius: f32, along: f32) -> f32 {
    let outer = ((radius + 1.0).powi(2) - along * along).max(0.0).sqrt();
    let inner = (radius * radius - along * along).max(0.0).sqrt();
    (outer.min(radius + 1.0) - inner.max(radius)).clamp(0.0, 1.0)
}

/// The slice of the window's outer border this decoration part owns, in that
/// part's own pixmap coordinates. `None` for parts with no border to draw.
///
/// `rect` is the part's surface rect with its *size* already scaled but its
/// origin still in logical points, as `redraw_inner` leaves it.
///
/// `rounded` stops the sides and the bottom [`CORNER_RADIUS`] short of the
/// window's bottom corners, matching the radius the header rounds its top ones
/// by. The client rounds those corners on its own pixels, and the arc's share of
/// the border has to be drawn there too: the notch outside the arc falls
/// *inside* the window rectangle, where no decoration subsurface reaches.
fn visible_border_rect(border: usize, rect: parts::Rect, scale: u32, rounded: bool) -> Option<Rect> {
    // The visible border is one pt.
    let visible_border_size = VISIBLE_BORDER_SIZE * scale;
    let corner = if rounded { CORNER_RADIUS * scale } else { 0 };

    // XXX we do all the match using integral types and then convert to f32 in the
    // end to ensure that result is finite.
    let rect = match border {
        DecorationParts::LEFT => {
            let x = (rect.x.unsigned_abs() * scale) - visible_border_size;
            let y = rect.y.unsigned_abs() * scale;
            Rect::from_xywh(
                x as f32,
                y as f32,
                visible_border_size as f32,
                rect.height.saturating_sub(y + corner) as f32,
            )
        }
        DecorationParts::RIGHT => {
            let y = rect.y.unsigned_abs() * scale;
            Rect::from_xywh(
                0.,
                y as f32,
                visible_border_size as f32,
                rect.height.saturating_sub(y + corner) as f32,
            )
        }
        // We draw small visible border only bellow the window surface, no need to
        // handle `TOP`.
        DecorationParts::BOTTOM => {
            let x = (rect.x.unsigned_abs() * scale) - visible_border_size;
            Rect::from_xywh(
                (x + corner) as f32,
                0.,
                rect.width.saturating_sub(2 * (x + corner)) as f32,
                visible_border_size as f32,
            )
        }
        _ => None,
    };

    // A window barely taller than its own corners has nothing left to draw.
    rect.filter(|rect| rect.width() > 0. && rect.height() > 0.)
}

/// Where [`visible_border_rect`] stops, the border's fade into the corner: one
/// device-pixel slice per step along the arc, with the coverage to paint it at.
///
/// Empty unless the window is rounded — a square corner keeps a full border all
/// the way in.
fn corner_border_taper(
    border: usize,
    rect: parts::Rect,
    scale: u32,
    rounded: bool,
) -> Vec<(Rect, f32)> {
    if !rounded {
        return Vec::new();
    }

    let visible_border_size = (VISIBLE_BORDER_SIZE * scale) as f32;
    let radius = (CORNER_RADIUS * scale) as f32;
    let mut slices = Vec::new();

    let mut push = |x: f32, y: f32, w: f32, h: f32, coverage: f32| {
        if coverage > 0. {
            if let Some(rect) = Rect::from_xywh(x, y, w, h) {
                slices.push((rect, coverage));
            }
        }
    };

    match border {
        DecorationParts::LEFT | DecorationParts::RIGHT => {
            let x = if border == DecorationParts::LEFT {
                (rect.x.unsigned_abs() * scale) as f32 - visible_border_size
            } else {
                0.
            };
            // The straight border ends a radius above the window's last row;
            // from there the arc takes over, one row at a time.
            let top = rect.height as f32 - radius;
            if top < (rect.y.unsigned_abs() * scale) as f32 {
                return Vec::new();
            }
            for step in 0..radius as u32 {
                let along = step as f32 + 0.5;
                push(
                    x,
                    top + step as f32,
                    visible_border_size,
                    1.,
                    corner_border_coverage(radius, along),
                );
            }
        }
        DecorationParts::BOTTOM => {
            // Mirror of the sides, a column at a time out towards each end.
            let outer_left = (rect.x.unsigned_abs() * scale) as f32 - visible_border_size;
            let outer_right = rect.width as f32 - outer_left;
            for step in 0..radius as u32 {
                let along = step as f32 + 0.5;
                let coverage = corner_border_coverage(radius, along);
                let inset = radius - 1. - step as f32;
                push(outer_left + inset, 0., 1., visible_border_size, coverage);
                push(outer_right - inset - 1., 0., 1., visible_border_size, coverage);
            }
        }
        _ => {}
    }

    slices
}

// returns horizontal margin, logical points
fn get_margin_h_lp(state: &WindowState) -> f32 {
    if state.intersects(WindowState::MAXIMIZED | WindowState::TILED) {
        0.
    } else {
        VISIBLE_BORDER_SIZE as f32
    }
}

#[cfg(test)]
mod header_outline_tests {
    use super::*;
    use tiny_skia::Pixmap;

    /// The header pixmap as `redraw_inner` sizes it: the window plus one visible
    /// border on each side.
    fn header(state: WindowState) -> Pixmap {
        let window_width = 40;
        let margin = if state.intersects(WindowState::MAXIMIZED | WindowState::TILED) {
            0
        } else {
            VISIBLE_BORDER_SIZE
        };
        let mut pixmap =
            Pixmap::new(window_width + 2 * margin, HEADER_SIZE).expect("a valid pixmap size");
        let theme = ColorTheme::dark();
        let colors = theme.for_state(state.contains(WindowState::ACTIVATED));
        draw_headerbar_bg(&mut pixmap.as_mut(), 1., colors, &state)
            .expect("the headerbar background to be drawable");
        pixmap
    }

    /// A theme colour as it lands in an opaque pixmap, to compare a drawn pixel
    /// against without copying the number into the test.
    fn opaque(color: theme::Color) -> (u8, u8, u8, u8) {
        let c = color.to_color_u8();
        (c.red(), c.green(), c.blue(), c.alpha())
    }

    fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixel(x, y).expect("a pixel inside the header");
        (p.red(), p.green(), p.blue(), p.alpha())
    }

    #[test]
    fn the_header_wears_the_windows_outer_border_on_its_own_edge() {
        let pixmap = header(WindowState::ACTIVATED);
        let (w, h) = (pixmap.width(), pixmap.height());
        // Below the rounded corner, where the sides run straight.
        let y = h - 8;

        // Premultiplied: 75% black over nothing is (0, 0, 0, 191).
        assert_eq!(rgba(&pixmap, 0, y), (0, 0, 0, 191));
        assert_eq!(rgba(&pixmap, w - 1, y), (0, 0, 0, 191));
        // And immediately inside it, the headerbar itself.
        let bar = opaque(ColorTheme::dark().active.headerbar);
        assert_eq!(rgba(&pixmap, 1, y), bar);
        assert_eq!(rgba(&pixmap, w - 2, y), bar);
    }

    #[test]
    fn the_outer_border_runs_over_the_top_corners_too() {
        let pixmap = header(WindowState::ACTIVATED);
        // A point on the top-left arc: the corner's centre is at (radius,
        // radius), so 45° out along the diagonal lands on the border.
        let d = (CORNER_RADIUS as f32) * (1. - std::f32::consts::FRAC_1_SQRT_2);
        let (x, y) = (d.round() as u32, d.round() as u32);
        let (r, g, b, a) = rgba(&pixmap, x, y);
        assert!(
            a > 128 && r < 24 && g < 24 && b < 24,
            "the arc at ({x}, {y}) should be border, not {:?}",
            (r, g, b, a)
        );
    }

    #[test]
    fn the_border_stops_at_the_bottom_where_the_window_takes_over() {
        let pixmap = header(WindowState::ACTIVATED);
        let (w, h) = (pixmap.width(), pixmap.height());
        // The last row is the headerbar's separator from the content, and it
        // runs the width of the *window*, leaving the outer border its columns.
        assert_eq!(
            rgba(&pixmap, w / 2, h - 1),
            opaque(ColorTheme::dark().active.border_color)
        );
        assert_eq!(rgba(&pixmap, 0, h - 1), (0, 0, 0, 191));
    }

    #[test]
    fn the_headerbar_is_parted_from_the_window_by_a_shadow_not_a_highlight() {
        // What sits under the titlebar is the line that makes it read as a bar
        // laid on the window rather than more window. Both stylesheets draw it
        // dark — libadwaita `inset 0 -1px rgb(0 0 6/36%)`, GTK's Adwaita a flat
        // near-black — and drawing it lighter than the headerbar, as this frame
        // used to, turns the relief inside out.
        for state in [WindowState::ACTIVATED, WindowState::empty()] {
            let pixmap = header(state);
            let (w, h) = (pixmap.width(), pixmap.height());
            let (bar, line) = (rgba(&pixmap, w / 2, h - 8), rgba(&pixmap, w / 2, h - 1));
            assert!(
                line.0 < bar.0 && line.1 < bar.1 && line.2 < bar.2,
                "{state:?}: the separator {line:?} must be darker than the headerbar {bar:?}"
            );
        }
    }

    #[test]
    fn a_maximized_header_has_no_outline_to_draw() {
        let pixmap = header(WindowState::ACTIVATED | WindowState::MAXIMIZED);
        let bar = opaque(ColorTheme::dark().active.headerbar);
        assert_eq!(rgba(&pixmap, 0, 0), bar);
        assert_eq!(rgba(&pixmap, pixmap.width() - 1, 0), bar);
    }
}

#[cfg(test)]
mod visible_border_tests {
    use super::*;

    /// The left and right parts as `DecorationParts` builds and resizes them for
    /// a `width` x `height` window.
    fn side(border: usize, width: u32, height: u32, scale: u32) -> parts::Rect {
        parts::Rect {
            x: if border == DecorationParts::LEFT {
                -(BORDER_SIZE as i32)
            } else {
                width as i32
            },
            y: -(HEADER_SIZE as i32),
            width: BORDER_SIZE * scale,
            height: (height + HEADER_SIZE) * scale,
        }
    }

    fn bottom(width: u32, height: u32, scale: u32) -> parts::Rect {
        parts::Rect {
            x: -(BORDER_SIZE as i32),
            y: height as i32,
            width: (width + 2 * BORDER_SIZE) * scale,
            height: BORDER_SIZE * scale,
        }
    }

    #[test]
    fn the_border_hands_the_ring_over_to_the_arc_without_a_step() {
        // Full at the tangent, where the column *is* the ring...
        assert!((corner_border_coverage(10., 0.) - 1.).abs() < 1e-3);
        // ...then only ever giving up ground as the arc curves away from it...
        let mut previous = 1.;
        for step in 0..10 {
            let coverage = corner_border_coverage(10., step as f32 + 0.5);
            assert!(
                coverage <= previous + 1e-6,
                "coverage rose again at {step}: {coverage} after {previous}"
            );
            previous = coverage;
        }
        // ...until the ring has left the column entirely.
        assert_eq!(corner_border_coverage(10., 9.5), 0.);
    }

    #[test]
    fn the_taper_takes_over_exactly_where_the_straight_border_stops() {
        let rect = side(DecorationParts::LEFT, 400, 300, 1);
        let straight = visible_border_rect(DecorationParts::LEFT, rect, 1, true)
            .expect("the left border to have a rect");
        let taper = corner_border_taper(DecorationParts::LEFT, rect, 1, true);

        let first = taper.first().expect("the corner to be tapered");
        assert_eq!(first.0.top(), straight.bottom(), "no gap, no overlap");
        assert_eq!(first.0.left(), straight.left(), "same column");
        assert!(first.1 > 0.9, "and starts about as dark as the border");

        // Every slice is one row, none of them reaches past the window's last,
        // and the rows the ring has left altogether are simply not drawn.
        let last = taper.last().expect("more than one row");
        assert!(taper.iter().all(|(r, _)| r.height() == 1.));
        assert!(last.0.bottom() <= (HEADER_SIZE + 300) as f32);
        assert!(last.1 < 0.5, "and it fades out rather than being cut off");
    }

    #[test]
    fn a_square_window_is_not_tapered() {
        let rect = side(DecorationParts::LEFT, 400, 300, 1);
        assert!(corner_border_taper(DecorationParts::LEFT, rect, 1, false).is_empty());
    }

    #[test]
    fn the_bottom_tapers_in_from_both_ends() {
        let rect = bottom(400, 300, 1);
        let straight = visible_border_rect(DecorationParts::BOTTOM, rect, 1, true)
            .expect("the bottom border to have a rect");
        let taper = corner_border_taper(DecorationParts::BOTTOM, rect, 1, true);

        let darkest = taper
            .iter()
            .filter(|(_, coverage)| *coverage > 0.9)
            .map(|(r, _)| r.left())
            .collect::<Vec<_>>();
        assert_eq!(darkest.len(), 2, "one column at each end");
        assert_eq!(darkest[0] + 1., straight.left(), "meets the straight run");
        assert_eq!(darkest[1], straight.right(), "at both of its ends");
    }

    #[test]
    fn the_sides_stop_where_the_bottom_corners_begin() {
        let rect = side(DecorationParts::LEFT, 400, 300, 1);
        let left = visible_border_rect(DecorationParts::LEFT, rect, 1, true)
            .expect("the left border to have a rect");

        assert_eq!(left.top(), HEADER_SIZE as f32, "starts below the header");
        assert_eq!(
            left.bottom(),
            (HEADER_SIZE + 300 - CORNER_RADIUS) as f32,
            "and stops a corner radius above the window's bottom"
        );
    }

    #[test]
    fn the_bottom_is_inset_by_a_corner_at_each_end() {
        let rect = bottom(400, 300, 1);
        let b = visible_border_rect(DecorationParts::BOTTOM, rect, 1, true)
            .expect("the bottom border to have a rect");

        // The part starts `BORDER_SIZE` left of the window, and the window's own
        // left edge is one visible border inside that.
        let window_left = (BORDER_SIZE - VISIBLE_BORDER_SIZE) as f32;
        assert_eq!(b.left(), window_left + CORNER_RADIUS as f32);
        assert_eq!(
            b.width(),
            400. + 2. * VISIBLE_BORDER_SIZE as f32 - 2. * CORNER_RADIUS as f32
        );
    }

    #[test]
    fn a_square_window_keeps_the_border_running_the_whole_way() {
        let rect = side(DecorationParts::RIGHT, 400, 300, 1);
        let right = visible_border_rect(DecorationParts::RIGHT, rect, 1, false)
            .expect("the right border to have a rect");

        assert_eq!(right.bottom(), (HEADER_SIZE + 300) as f32);

        let b = visible_border_rect(DecorationParts::BOTTOM, bottom(400, 300, 1), 1, false)
            .expect("the bottom border to have a rect");
        assert_eq!(b.width(), 400. + 2. * VISIBLE_BORDER_SIZE as f32);
    }

    #[test]
    fn every_length_scales_with_the_output() {
        let rect = side(DecorationParts::LEFT, 400, 300, 2);
        let left = visible_border_rect(DecorationParts::LEFT, rect, 2, true)
            .expect("the left border to have a rect");

        assert_eq!(left.width(), 2. * VISIBLE_BORDER_SIZE as f32);
        assert_eq!(left.top(), 2. * HEADER_SIZE as f32);
        assert_eq!(
            left.bottom(),
            (2 * (HEADER_SIZE + 300) - 2 * CORNER_RADIUS) as f32
        );
    }

    #[test]
    fn a_window_shorter_than_its_own_corners_asks_for_no_border() {
        let rect = side(DecorationParts::LEFT, 400, 4, 1);
        assert!(visible_border_rect(DecorationParts::LEFT, rect, 1, true).is_none());
    }
}

#[cfg(test)]
mod pixel_order_tests {
    use super::*;

    #[test]
    fn a_tinted_colour_reaches_the_buffer_the_way_it_was_drawn() {
        // libadwaita's headerbar is #2e2e32 — barely blue, but enough that
        // leaving it in RGBA order paints it #322e2e instead.
        let mut data = vec![0x2e, 0x2e, 0x32, 0xff];
        swap_red_and_blue(&mut data);
        assert_eq!(data, vec![0x32, 0x2e, 0x2e, 0xff], "want BGRA");
    }

    #[test]
    fn a_grey_is_the_same_either_way() {
        let grey = vec![0x30, 0x30, 0x30, 0xff, 0, 0, 0, 0x80];
        let mut swapped = grey.clone();
        swap_red_and_blue(&mut swapped);
        assert_eq!(swapped, grey);
    }
}
