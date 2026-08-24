//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Baseview WindowHandler connecting Xilem's reactive cycle with masonry rendering.
//!
//! This is the integration point: it receives baseview events, feeds them to
//! masonry's RenderRoot, processes the resulting signals through the Xilem driver,
//! and renders the updated scene via Vello/wgpu.
//!
//! NOTE: Multi-window support
//! To support multiple windows, this handler would manage multiple RenderRoots
//! (one per window) and route events/signals accordingly. The current implementation
//! assumes a single window (audio plugin use case).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use baseview::{Event, EventStatus, Window, WindowHandler};
use masonry::app::{RenderRoot, RenderRootOptions, RenderRootSignal, WindowSizePolicy};
use masonry::core::WindowEvent as MasonryWindowEvent;
use masonry::theme::default_property_set;
use vello::peniko::Color;
use vello::Scene;
use xilem_masonry::WidgetView;

use crate::driver::{BaseviewDriver, MessagePackage};
use crate::event::{EventTranslator, MasonryEvent};
use crate::render::RenderContext;

/// The baseview WindowHandler that integrates Xilem with masonry.
pub(crate) struct XilemHandler<State: 'static, Logic> {
    driver: BaseviewDriver<State, Logic>,
    render_root: Option<RenderRoot>,
    render_ctx: Option<RenderContext>,
    event_translator: EventTranslator,
    pending_signals: Arc<Mutex<Vec<RenderRootSignal>>>,
    async_receiver: tokio::sync::mpsc::UnboundedReceiver<MessagePackage>,
    scene: Scene,
    last_frame: Instant,
    base_color: Color,
    width: f64,
    height: f64,
    initialized: bool,
    /// Real backing scale factor, once known. `baseview::Window` has no
    /// public getter for this at `on_frame`/`ensure_initialized` time (macOS
    /// only reports it later via a `WindowEvent::Resized` carrying
    /// `WindowInfo::scale()`), so eagerly creating the GPU surface and
    /// masonry's RenderRoot at scale 1.0 leaves them permanently sized in
    /// physical pixels equal to the *logical* point size - on a 2x Retina
    /// display the NSView's real backing store is twice that in each
    /// dimension, so content only ever paints the top-left quarter of the
    /// view. Deferring init until the first Resize event lets us use the
    /// real scale from the start.
    known_scale: Option<f64>,
}

impl<State, Logic, View> XilemHandler<State, Logic>
where
    State: 'static,
    Logic: FnMut(&mut State) -> View,
    View: WidgetView<State>,
{
    pub(crate) fn new(
        driver: BaseviewDriver<State, Logic>,
        async_receiver: tokio::sync::mpsc::UnboundedReceiver<MessagePackage>,
        width: f64,
        height: f64,
    ) -> Self {
        Self {
            driver,
            render_root: None,
            render_ctx: None,
            event_translator: EventTranslator::new(1.0),
            pending_signals: Arc::new(Mutex::new(Vec::new())),
            async_receiver,
            scene: Scene::new(),
            last_frame: Instant::now(),
            base_color: Color::from_rgba8(30, 30, 35, 255),
            width,
            height,
            initialized: false,
            known_scale: None,
        }
    }

    /// Size to initialize the GPU surface and masonry's layout at, plus the
    /// real scale factor once known (via the first Resize event).
    ///
    /// NOTE: the wgpu surface here is backed directly by the NSView's
    /// CAMetalLayer, whose `contentsScale` this crate does not currently set
    /// explicitly - it stays at its AppKit default, so the layer presents
    /// whatever pixel dimensions we hand it 1:1 against the view's *point*
    /// frame. Sizing the surface at `width * scale` (e.g. 2x on Retina)
    /// without also setting `contentsScale` to match makes the content
    /// render at half its intended visual size, not sharper - confirmed by
    /// testing. So width/height here intentionally stay in logical points
    /// (unscaled) for now. `known_scale` is still tracked and returned so a
    /// future fix (setting contentsScale alongside a truly physical-pixel
    /// surface, for genuine Retina sharpness) has the value ready to use.
    fn physical_size(&self) -> (u32, u32, f64) {
        let scale = self.known_scale.unwrap_or(1.0);
        (self.width.round() as u32, self.height.round() as u32, scale)
    }

    fn ensure_initialized(&mut self, window: &mut Window) {
        if self.initialized {
            return;
        }

        let (phys_width, phys_height, _scale) = self.physical_size();

        // Initialize GPU context
        if self.render_ctx.is_none() {
            match unsafe { RenderContext::new(window, phys_width, phys_height) } {
                Ok(ctx) => {
                    self.render_ctx = Some(ctx);
                    tracing::info!("GPU context initialized");
                }
                Err(e) => {
                    tracing::error!("Failed to create GPU context: {}", e);
                    return;
                }
            }
        }

        // Build initial view tree and create RenderRoot
        if self.render_root.is_none() {
            let initial_widget = self.driver.build_initial();

            let signals = self.pending_signals.clone();
            let signal_sink = move |signal: RenderRootSignal| {
                signals.lock().unwrap().push(signal);
            };

            let options = RenderRootOptions {
                default_properties: Arc::new(default_property_set()),
                use_system_fonts: true,
                size_policy: WindowSizePolicy::User,
                size: masonry::dpi::PhysicalSize::new(phys_width, phys_height),
                // Intentionally always 1.0 for now - see physical_size()'s
                // doc comment. Passing the real backing scale here without
                // also setting the NSView layer's contentsScale would make
                // masonry lay out content at 2x into a 1x-sized surface,
                // shrinking everything instead of sharpening it.
                scale_factor: 1.0,
                test_font: None,
            };

            let render_root =
                RenderRoot::new(initial_widget.0.new_widget.erased(), signal_sink, options);
            self.render_root = Some(render_root);

            // Register fonts and set focus fallback
            let rr = self.render_root.as_mut().unwrap();
            self.driver.register_fonts(rr);
            self.driver.set_focus_fallback(rr);

            tracing::info!("Xilem widget tree initialized");
        }

        // Only mark fully initialized once we know the real scale factor -
        // if we had to fall back to 1.0 here, the next Resize event (which
        // carries the real scale) will still trigger a proper re-init via
        // handle_masonry_event's Resize branch.
        self.initialized = self.known_scale.is_some();
    }

    fn process_signals(&mut self) {
        let signals: Vec<_> = {
            let mut pending = self.pending_signals.lock().unwrap();
            std::mem::take(&mut *pending)
        };

        let render_root = match self.render_root.as_mut() {
            Some(rr) => rr,
            None => return,
        };

        for signal in signals {
            match signal {
                RenderRootSignal::Action(action, widget_id) => {
                    self.driver.handle_action(render_root, widget_id, action);
                    // Re-acquire render_root reference after potential rebuild
                    // (driver borrows render_root mutably via the reference we pass)
                }
                // Layer management - forward to render_root
                RenderRootSignal::NewLayer(_layer_type, widget, position) => {
                    render_root.add_layer(widget, position);
                }
                RenderRootSignal::RemoveLayer(widget_id) => {
                    render_root.remove_layer(widget_id);
                }
                RenderRootSignal::RepositionLayer(widget_id, position) => {
                    render_root.reposition_layer(widget_id, position);
                }
                // Redraw requests are handled naturally by the frame loop
                RenderRootSignal::RequestRedraw | RenderRootSignal::RequestAnimFrame => {}
                // Cursor changes - baseview doesn't support cursor changes in plugin context
                RenderRootSignal::SetCursor(_) => {}
                // Window management - no-ops for plugins
                RenderRootSignal::SetSize(_)
                | RenderRootSignal::SetTitle(_)
                | RenderRootSignal::DragWindow
                | RenderRootSignal::DragResizeWindow(_)
                | RenderRootSignal::ToggleMaximized
                | RenderRootSignal::Minimize
                | RenderRootSignal::ShowWindowMenu(_) => {}
                // IME - not yet supported
                RenderRootSignal::StartIme
                | RenderRootSignal::EndIme
                | RenderRootSignal::ImeMoved(_, _) => {}
                // Clipboard - not yet supported in plugin context
                RenderRootSignal::ClipboardStore(_) => {}
                // Focus
                RenderRootSignal::TakeFocus => {}
                // Exit - not applicable for plugins
                RenderRootSignal::Exit => {}
                // Debug
                RenderRootSignal::WidgetSelectedInInspector(_) => {}
            }
        }
    }

    fn process_async_messages(&mut self) {
        let render_root = match self.render_root.as_mut() {
            Some(rr) => rr,
            None => return,
        };

        while let Ok(msg) = self.async_receiver.try_recv() {
            let (path, message) = msg;
            self.driver.handle_async_action(render_root, path, message);
        }
    }

    fn handle_masonry_event(&mut self, event: MasonryEvent) {
        // First time we learn the real backing scale factor: if
        // ensure_initialized already ran with a guessed 1.0 scale, its GPU
        // surface and RenderRoot are sized wrong (physical pixels == logical
        // points, leaving content confined to a fraction of the real Retina
        // backing store). Drop them so ensure_initialized rebuilds everything
        // at the now-known-correct physical size on the next frame, instead
        // of papering over a wrong initial size with resize/rescale events
        // alone. Must happen before we borrow self.render_root below.
        if let MasonryEvent::Resize { width, height, scale } = event {
            if self.known_scale.is_none() && self.initialized {
                self.known_scale = Some(scale);
                self.render_ctx = None;
                self.render_root = None;
                self.initialized = false;
                self.width = width / scale;
                self.height = height / scale;
                self.event_translator.set_scale_factor(scale);
                return;
            }
            self.known_scale = Some(scale);
        }

        let Some(render_root) = &mut self.render_root else {
            return;
        };

        match event {
            MasonryEvent::Pointer(ptr_event) => {
                let _ = render_root.handle_pointer_event(ptr_event);
            }
            MasonryEvent::Keyboard(_kb_event) => {
                // TODO: Convert keyboard_types to masonry's TextEvent
            }
            MasonryEvent::Resize { width, height, scale } => {
                self.width = width / scale;
                self.height = height / scale;
                self.event_translator.set_scale_factor(scale);

                // Surface/layout stay sized in logical points (see
                // physical_size()'s doc comment) - resize them to match the
                // new logical size, not the event's raw physical pixels.
                let resize_width = self.width.round() as u32;
                let resize_height = self.height.round() as u32;

                if let Some(ctx) = &mut self.render_ctx {
                    ctx.resize(resize_width, resize_height);
                }

                let _ = render_root.handle_window_event(MasonryWindowEvent::Resize(
                    masonry::dpi::PhysicalSize::new(resize_width, resize_height),
                ));
            }
            MasonryEvent::Focus(_) => {}
            MasonryEvent::Close => {}
        }
    }

    fn render_frame(&mut self) {
        if self.render_root.is_none() || self.render_ctx.is_none() {
            return;
        }

        let render_root = self.render_root.as_mut().unwrap();
        let render_ctx = self.render_ctx.as_mut().unwrap();

        let now = Instant::now();
        let dt = now.duration_since(self.last_frame);
        self.last_frame = now;

        let _ = render_root.handle_window_event(MasonryWindowEvent::AnimFrame(dt));

        let (paint_result, _accessibility) = render_root.redraw();
        self.scene = paint_result.composite();

        if let Err(e) = render_ctx.render(&self.scene, self.base_color) {
            tracing::error!("Render error: {}", e);
        }
    }
}

impl<State, Logic, View> WindowHandler for XilemHandler<State, Logic>
where
    State: 'static,
    Logic: FnMut(&mut State) -> View,
    View: WidgetView<State>,
{
    fn on_frame(&mut self, window: &mut Window) {
        self.ensure_initialized(window);
        self.process_signals();
        self.process_async_messages();
        self.render_frame();
    }

    fn on_event(&mut self, _window: &mut Window, event: Event) -> EventStatus {
        if let Some(masonry_event) = self.event_translator.translate(&event) {
            self.handle_masonry_event(masonry_event);
            // Process any signals generated by the event
            self.process_signals();
            EventStatus::Captured
        } else {
            EventStatus::Ignored
        }
    }
}
