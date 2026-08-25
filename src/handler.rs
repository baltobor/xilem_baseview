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
//! and renders the resulting VisualLayerPlan via masonry_imaging (the same
//! rendering path masonry_winit uses).
//!
//! NOTE: Multi-window support
//! To support multiple windows, this handler would manage multiple RenderRoots
//! (one per window) and route events/signals accordingly. The current implementation
//! assumes a single window (audio plugin use case).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use baseview::{Event, EventStatus, Window, WindowHandler};
use masonry::app::VisualLayerKind;
use masonry::app::{RenderRoot, RenderRootOptions, RenderRootSignal, WindowSizePolicy};
use masonry::core::WindowEvent as MasonryWindowEvent;
use masonry::peniko::Color;
use masonry::theme::default_property_set;
use masonry_imaging::Layer as ImagingLayer;
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
    last_frame: Instant,
    base_color: Color,
    width: f64,
    height: f64,
    initialized: bool,
    /// Real backing scale factor, applied only at paint time (see
    /// render.rs's PreparedFrame usage) - masonry lays out widgets in
    /// logical points regardless of display scale, so this must never be
    /// baked into the RenderRoot's size/scale_factor or the surface's
    /// pixel dimensions. Falls back to 1.0 until the first Resize event
    /// reports the real value. This is important for the Retina Screen
    /// of i.e. MacOS computers.
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
            last_frame: Instant::now(),
            base_color: Color::from_rgba8(30, 30, 35, 255),
            width,
            height,
            initialized: false,
            known_scale: None,
        }
    }

    fn ensure_initialized(&mut self, window: &mut Window) {
        if self.initialized {
            return;
        }

        let width = self.width.round() as u32;
        let height = self.height.round() as u32;

        // Initialize GPU context. The surface/target texture must be sized
        // in physical pixels, since PreparedFrame (render.rs) scales the
        // logical-point content by the backing scale when painting into it -
        // a surface sized only for 1x overflows once scale > 1 (Retina on MacOS).
        // The real scale isn't known this early (baseview only reports it
        // via the first Resize event), so this guesses scale 1.0; the
        // Resize handler corrects the surface size once the real scale
        // arrives.
        let init_scale = self.known_scale.unwrap_or(1.0);
        let phys_width = (self.width * init_scale).round() as u32;
        let phys_height = (self.height * init_scale).round() as u32;
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
                // Logical points, not physical pixels - masonry always lays
                // out in logical units; scale is applied only at paint time.
                size: masonry::dpi::PhysicalSize::new(width, height),
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

        self.initialized = true;
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
        if let MasonryEvent::Resize { scale, .. } = event {
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
            MasonryEvent::Resize {
                width,
                height,
                scale,
            } => {
                self.width = width / scale;
                self.height = height / scale;
                self.event_translator.set_scale_factor(scale);

                // Masonry's layout stays in logical points (Resize here uses
                // the logical size), but the actual render surface/target
                // texture must be physical pixels - PreparedFrame::paint_into
                // (see render.rs) scales the logical-point content by `scale`
                // when compositing, so a surface sized only for 1x overflows
                // and gets clipped once scale > 1 (e.g. MacOS Retina Screen).
                let logical_width = self.width.round() as u32;
                let logical_height = self.height.round() as u32;
                let physical_width = width.round() as u32;
                let physical_height = height.round() as u32;

                if let Some(ctx) = &mut self.render_ctx {
                    ctx.resize(physical_width, physical_height);
                }

                let _ = render_root.handle_window_event(MasonryWindowEvent::Resize(
                    masonry::dpi::PhysicalSize::new(logical_width, logical_height),
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

        let (visual_layers, _tree_update) = render_root.redraw();

        let overlays: Vec<_> = visual_layers
            .overlay_layers()
            .map(|layer| {
                let VisualLayerKind::Scene(scene) = &layer.kind else {
                    unreachable!("overlay_layers only returns scene layers");
                };
                ImagingLayer {
                    scene,
                    transform: layer.transform,
                }
            })
            .collect();

        let Some(root_layer) = visual_layers.root_layer() else {
            return;
        };
        let VisualLayerKind::Scene(root_scene) = &root_layer.kind else {
            return;
        };

        let scale = self.known_scale.unwrap_or(1.0);
        if let Err(e) = render_ctx.render(root_scene, &overlays, self.base_color, scale) {
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
