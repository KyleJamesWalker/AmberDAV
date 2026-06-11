//! Wayland sink: a fullscreen `wl_shm` client that paints the connection-info
//! canvas under a Wayland compositor. This is the Steam Deck **Game Mode** path
//! — Gamescope owns DRM there, so the framebuffer sink is invisible and we must
//! be a real Wayland client for anything to appear on the panel.
//!
//! Software buffers only (no GPU/EGL), so `smithay-client-toolkit` stays on its
//! default pure-Rust backend and the static musl binary keeps working.
//!
//! See also — content: `canvas.rs` → choice: `display.rs` → sinks:
//! `screen.rs`/`sdl.rs`/`wayland.rs` → state: `screen::Mode`.

use smithay_client_toolkit::reexports::client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};

use crate::canvas::{black_canvas, info_canvas, Canvas};
use crate::screen::{Mode, ModeHandle, Status};

/// Entry point: open the compositor connection and run the paint loop. Returns
/// only on error (so the caller can fall back / log). Blocks the calling thread.
pub fn run(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    socket: Option<String>,
    startup_error: Option<String>,
) -> Result<(), String> {
    // Connect to the resolved socket if we have one (so Game Mode works even
    // when Steam launches us with no $WAYLAND_DISPLAY); else the env default.
    let conn = match socket.as_deref() {
        Some(name) => {
            let path = if name.starts_with('/') {
                std::path::PathBuf::from(name)
            } else {
                std::path::PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_default())
                    .join(name)
            };
            let stream = std::os::unix::net::UnixStream::connect(&path)
                .map_err(|e| format!("wayland connect {}: {e}", path.display()))?;
            Connection::from_socket(stream).map_err(|e| format!("wayland from_socket: {e}"))?
        }
        None => Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?,
    };
    eprintln!("wayland: connected (socket={socket:?})");
    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| format!("wayland registry: {e}"))?;
    let qh: QueueHandle<App> = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
    let xdg_shell = XdgShell::bind(&globals, &qh).map_err(|e| format!("xdg_shell: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;
    eprintln!("wayland: globals bound; creating fullscreen surface");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::RequestServer, &qh);
    window.set_title("amber-dav");
    window.set_app_id("dev.amberdav.AmberDAV");
    window.set_fullscreen(None);
    window.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).map_err(|e| format!("slot pool: {e}"))?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        window,
        port,
        password,
        status,
        mode,
        startup_error,
        width: 1280,
        height: 800,
        configured: false,
        closed: false,
        frame: 0,
        buffer: None,
        buf_dims: (0, 0),
    };

    while !app.closed {
        event_queue
            .blocking_dispatch(&mut app)
            .map_err(|e| format!("wayland dispatch: {e}"))?;
    }
    Ok(())
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    /// Startup problem (config parse failure, issue #19; failed bind,
    /// issue #35) to surface on the info screen.
    startup_error: Option<String>,
    width: u32,
    height: u32,
    configured: bool,
    closed: bool,
    frame: u64,
    /// The single shm buffer, reused across frames. `SlotPool` only grows
    /// (doubling, never shrinking), so allocating a fresh buffer each frame
    /// would ratchet the mmap up over a long session — instead we recreate it
    /// only on first paint or when the surface is resized.
    buffer: Option<Buffer>,
    /// Dimensions the retained `buffer` was created for; a mismatch forces a
    /// recreate.
    buf_dims: (u32, u32),
}

impl App {
    fn set_status(&self, msg: String) {
        if let Ok(mut s) = self.status.lock() {
            *s = msg;
        }
    }

    fn build_canvas(&self) -> Canvas {
        let (w, h) = (self.width as usize, self.height as usize);
        let mode = self.mode.lock().map(|m| *m).unwrap_or(Mode::Info);
        match mode {
            // Bounce is framebuffer-only for now; under Wayland it shows Info.
            Mode::Info | Mode::Bounce => info_canvas(
                w,
                h,
                crate::state::current_ip(),
                self.port,
                self.password.as_deref(),
                self.startup_error.as_deref(),
            ),
            Mode::Black => black_canvas(w, h),
        }
    }

    /// Paint and present one frame.
    ///
    /// Repaint (and therefore the periodic `current_ip()` refresh) is driven
    /// entirely by Wayland frame callbacks: `frame()` re-enters here, and each
    /// `draw()` requests the next callback. While the surface is occluded (e.g.
    /// the Steam overlay covers it) the compositor stops delivering callbacks,
    /// so the loop pauses and resumes when the surface is visible again. This
    /// is intentional — we deliberately avoid a timer thread to keep the sink
    /// simple. Late-Wi-Fi recovery still works because the surface is visible
    /// at startup, which is exactly when we need the IP to re-resolve.
    ///
    /// To keep the loop self-sustaining, EVERY path through `draw()` — success
    /// or bail — re-arms the next frame callback and commits the surface. A
    /// frame callback is the only thing that re-enters `draw()`, so skipping it
    /// on a transient error (buffer busy, attach failure) would freeze the
    /// screen permanently; instead we always retry on the next frame.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (w, h) = (self.width as i32, self.height as i32);
        let stride = w * 4;
        // Clone the surface handle (cheap, Arc-backed) so we never hold a
        // `&self`-derived borrow across the `&mut self.pool` borrow below.
        let surface = self.window.wl_surface().clone();

        // (Re)create the retained buffer only on first paint or a resize.
        if self.buffer.is_none() || self.buf_dims != (self.width, self.height) {
            match self
                .pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
            {
                Ok((buffer, _)) => {
                    self.buffer = Some(buffer);
                    self.buf_dims = (self.width, self.height);
                }
                Err(e) => {
                    self.set_status(format!("wayland buffer: {e}"));
                    // Re-arm so we retry allocating next frame instead of
                    // freezing the screen.
                    surface.frame(qh, surface.clone());
                    self.window.commit();
                    return;
                }
            }
        }

        let canvas = self.build_canvas();
        // Borrow `pool` and `buffer` as distinct fields so the canvas slice
        // (a `&mut self.pool` borrow keyed by `&self.buffer`) doesn't conflict.
        let buffer = self.buffer.as_ref().expect("buffer set above");
        match self.pool.canvas(buffer) {
            Some(data) => {
                // Argb8888 little-endian on the wire is byte order B,G,R,A.
                for (i, px) in canvas.px.iter().enumerate() {
                    let o = i * 4;
                    if o + 4 <= data.len() {
                        data[o] = px[2];
                        data[o + 1] = px[1];
                        data[o + 2] = px[0];
                        data[o + 3] = 0xff;
                    }
                }
                if let Err(e) = buffer.attach_to(&surface) {
                    self.set_status(format!("wayland attach: {e}"));
                } else {
                    surface.damage_buffer(0, 0, w, h);
                    self.frame = self.frame.wrapping_add(1);
                    self.set_status(format!(
                        "ok (wayland {}x{}) frame={}",
                        self.width, self.height, self.frame
                    ));
                }
            }
            // The compositor still holds the buffer; skip this frame's paint and
            // retry on the next callback rather than freezing.
            None => self.set_status("wayland: buffer busy, retrying".to_string()),
        }

        // Always re-arm the next callback and commit so the loop never stalls.
        surface.frame(qh, surface.clone());
        self.window.commit();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.closed = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // Compositors (esp. Gamescope) may send (None, None) meaning "you
        // choose"; keep current dims. Defaults (1280x800) match the Deck panel.
        if let (Some(w), Some(h)) = (configure.new_size.0, configure.new_size.1) {
            self.width = w.get();
            self.height = h.get();
        }
        let first = !self.configured;
        self.configured = true;
        if first {
            eprintln!(
                "wayland: surface configured {}x{}; painting connection info",
                self.width, self.height
            );
            self.draw(qh);
        }
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_xdg_window!(App);
delegate_registry!(App);
