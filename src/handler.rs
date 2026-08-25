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

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use baseview::{Event, EventStatus, HandlerError, WindowContext, WindowHandler, WindowSize};
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
///
/// `WindowHandler`'s methods all take `&self`, so the mutable state below is
/// wrapped in a `RefCell`. This is sound because baseview only ever calls
/// these methods from the window's own (main) thread, one at a time.
pub(crate) struct XilemHandler<State: 'static, Logic> {
    inner: RefCell<Inner<State, Logic>>,
}

struct Inner<State: 'static, Logic> {
    driver: BaseviewDriver<State, Logic>,
    render_root: RenderRoot,
    render_ctx: RenderContext,
    event_translator: EventTranslator,
    pending_signals: Arc<Mutex<Vec<RenderRootSignal>>>,
    async_receiver: tokio::sync::mpsc::UnboundedReceiver<MessagePackage>,
    last_frame: Instant,
    base_color: Color,
    /// Logical window size in points, tracked so resize handling can derive
    /// masonry's logical layout size independent of the physical/backing
    /// scale reported alongside it.
    width: f64,
    height: f64,
    /// Real backing scale factor. Fed to the RenderRoot (so it can derive
    /// its logical layout size and convert physical pointer positions to
    /// logical hit-test coordinates) and to render.rs's PreparedFrame at
    /// paint time. Important for Retina screens (e.g. on MacOS).
    scale: f64,
}

impl<State, Logic, View> XilemHandler<State, Logic>
where
    State: 'static,
    Logic: FnMut(&mut State) -> View,
    View: WidgetView<State>,
{
    /// Build the handler for a newly-created window.
    ///
    /// Unlike the old baseview API, `WindowContext::scale_factor()` and
    /// `WindowContext::size()` are available synchronously here, so the GPU
    /// context and RenderRoot can be sized correctly on the very first
    /// frame - no more deferred "guess scale 1.0, correct on first resize"
    /// initialization.
    pub(crate) fn new(
        ctx: &WindowContext,
        mut driver: BaseviewDriver<State, Logic>,
        async_receiver: tokio::sync::mpsc::UnboundedReceiver<MessagePackage>,
        width: f64,
        height: f64,
    ) -> Result<Self, HandlerError> {
        let scale = ctx.scale_factor();

        let phys_width = (width * scale).round().max(1.0) as u32;
        let phys_height = (height * scale).round().max(1.0) as u32;

        let render_ctx = unsafe { RenderContext::new(ctx, phys_width, phys_height) }
            .map_err(HandlerError::from)?;

        let initial_widget = driver.build_initial();

        let pending_signals = Arc::new(Mutex::new(Vec::new()));
        let signals = pending_signals.clone();
        let signal_sink = move |signal: RenderRootSignal| {
            signals.lock().unwrap().push(signal);
        };

        let options = RenderRootOptions {
            default_properties: Arc::new(default_property_set()),
            use_system_fonts: true,
            size_policy: WindowSizePolicy::User,
            // RenderRoot's `size` is physical pixels; it derives the logical
            // layout size internally via `size.to_logical(scale_factor)`
            // (masonry_core::app::render_root::get_kurbo_size), and the same
            // `scale_factor` is used to convert incoming physical pointer
            // positions to logical hit-test coordinates
            // (masonry_core::passes::event). Passing scale_factor: 1.0 here
            // while feeding it real physical pointer positions silently
            // breaks hit-testing on any display where scale != 1 (e.g.
            // Retina) - the pointer position is treated as already-logical,
            // so clicks land at 1/scale of the intended widget position.
            size: masonry::dpi::PhysicalSize::new(phys_width, phys_height),
            scale_factor: scale,
            test_font: None,
        };

        let mut render_root =
            RenderRoot::new(initial_widget.0.new_widget.erased(), signal_sink, options);

        driver.register_fonts(&mut render_root);
        driver.set_focus_fallback(&mut render_root);

        tracing::info!("Xilem widget tree initialized");

        Ok(Self {
            inner: RefCell::new(Inner {
                driver,
                render_root,
                render_ctx,
                event_translator: EventTranslator::new(scale),
                pending_signals,
                async_receiver,
                last_frame: Instant::now(),
                base_color: Color::from_rgba8(30, 30, 35, 255),
                width,
                height,
                scale,
            }),
        })
    }
}

impl<State, Logic> Inner<State, Logic> {
    fn process_signals<View>(&mut self)
    where
        Logic: FnMut(&mut State) -> View,
        View: WidgetView<State>,
    {
        let signals: Vec<_> = {
            let mut pending = self.pending_signals.lock().unwrap();
            std::mem::take(&mut *pending)
        };

        for signal in signals {
            match signal {
                RenderRootSignal::Action(action, widget_id) => {
                    self.driver
                        .handle_action(&mut self.render_root, widget_id, action);
                }
                RenderRootSignal::NewLayer(_layer_type, widget, position) => {
                    self.render_root.add_layer(widget, position);
                }
                RenderRootSignal::RemoveLayer(widget_id) => {
                    self.render_root.remove_layer(widget_id);
                }
                RenderRootSignal::RepositionLayer(widget_id, position) => {
                    self.render_root.reposition_layer(widget_id, position);
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

    fn process_async_messages<View>(&mut self)
    where
        Logic: FnMut(&mut State) -> View,
        View: WidgetView<State>,
    {
        while let Ok(msg) = self.async_receiver.try_recv() {
            let (path, message) = msg;
            self.driver
                .handle_async_action(&mut self.render_root, path, message);
        }
    }

    fn handle_masonry_event<View>(&mut self, event: MasonryEvent)
    where
        Logic: FnMut(&mut State) -> View,
        View: WidgetView<State>,
    {
        match event {
            MasonryEvent::Pointer(ptr_event) => {
                let _ = self.render_root.handle_pointer_event(ptr_event);
            }
            MasonryEvent::Keyboard(_kb_event) => {
                // TODO: Convert keyboard_types to masonry's TextEvent
            }
            MasonryEvent::Focus(_) => {}
            MasonryEvent::Close => {}
        }
    }

    fn handle_resize(&mut self, new_size: WindowSize) {
        let scale_changed = self.scale != new_size.scale_factor;

        self.width = new_size.logical.width;
        self.height = new_size.logical.height;
        self.scale = new_size.scale_factor;
        self.event_translator
            .set_scale_factor(new_size.scale_factor);

        let physical_width = new_size.physical.width;
        let physical_height = new_size.physical.height;

        // The GPU surface/target texture is physical pixels - PreparedFrame
        // (see render.rs) scales the logical-point content by `scale` when
        // compositing, so a surface sized only for 1x overflows and gets
        // clipped once scale > 1 (e.g. MacOS Retina Screen).
        self.render_ctx.resize(physical_width, physical_height);

        if scale_changed {
            let _ = self
                .render_root
                .handle_window_event(MasonryWindowEvent::Rescale(new_size.scale_factor));
        }

        // RenderRoot's `size` is physical pixels (it derives the logical
        // layout size internally via scale_factor - see the comment on
        // RenderRootOptions::size in `new` above).
        let _ = self
            .render_root
            .handle_window_event(MasonryWindowEvent::Resize(masonry::dpi::PhysicalSize::new(
                physical_width,
                physical_height,
            )));
    }

    fn render_frame(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame);
        self.last_frame = now;

        let _ = self
            .render_root
            .handle_window_event(MasonryWindowEvent::AnimFrame(dt));

        let (visual_layers, _tree_update) = self.render_root.redraw();

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

        if let Err(e) = self
            .render_ctx
            .render(root_scene, &overlays, self.base_color, self.scale)
        {
            tracing::error!("Render error: {}", e);
        }
    }
}

impl<State, Logic, View> WindowHandler for XilemHandler<State, Logic>
where
    State: 'static,
    Logic: FnMut(&mut State) -> View + 'static,
    View: WidgetView<State> + 'static,
{
    fn on_frame(&self) -> Result<(), HandlerError> {
        let mut inner = self.inner.borrow_mut();
        inner.process_signals();
        inner.process_async_messages();
        inner.render_frame();
        Ok(())
    }

    fn resized(&self, new_size: WindowSize) -> Result<(), HandlerError> {
        self.inner.borrow_mut().handle_resize(new_size);
        Ok(())
    }

    fn on_event(&self, event: Event) -> EventStatus {
        let mut inner = self.inner.borrow_mut();
        if let Some(masonry_event) = inner.event_translator.translate(&event) {
            inner.handle_masonry_event(masonry_event);
            inner.process_signals();
            EventStatus::Captured
        } else {
            EventStatus::Ignored
        }
    }
}
