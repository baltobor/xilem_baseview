//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Rendering pipeline for Xilem in baseview.
//!
//! Sets up a wgpu surface and renders masonry's paint output into it via
//! `masonry_imaging`'s own renderer (the same one `masonry_winit` uses)
//! `masonry_imaging` owns the translation from masonry's
//! `VisualLayerPlan`/`PreparedFrame` into a backend and applies the
//! paint-time backing-scale transform itself. This solves scaling
//! issues with MacOS Retina screens.
//!
//! TODO: Test on linux and Windows.

use std::sync::Arc;

use masonry::peniko::Color;
use masonry_imaging::vello::{Renderer as ImagingRenderer, TextureTarget};
use masonry_imaging::{Layer as ImagingLayer, PreparedFrame, TextureRenderer};
use wgpu::util::TextureBlitter;
use wgpu::{
    CompositeAlphaMode, Device, DeviceDescriptor, Features, Instance, InstanceDescriptor, Limits,
    PresentMode, Queue, Surface, SurfaceConfiguration, Texture, TextureDescriptor,
    TextureDimension, TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};

/// GPU rendering context: owns the wgpu surface and masonry's own renderer.
///
/// masonry_imaging's Vello backend renders into an Rgba8Unorm storage
/// texture internally (its compute shaders require that binding format),
/// so it can't target the surface's own texture view directly when the
/// surface format differs (e.g. macOS/Metal surfaces are commonly
/// Bgra8Unorm) - doing so is a wgpu validation error. Render into an
/// intermediate Rgba8Unorm texture instead, then blit that into the real
/// surface texture, matching masonry_winit's own render pipeline.
pub struct RenderContext {
    pub device: Arc<Device>,
    pub queue: Arc<Queue>,
    renderer: ImagingRenderer,
    surface: Surface<'static>,
    surface_config: SurfaceConfiguration,
    target_texture: Texture,
    target_view: TextureView,
    blitter: TextureBlitter,
}

impl RenderContext {
    /// Create a new render context for a window.
    ///
    /// `width`/`height` are in logical points, matching masonry's own
    /// layout units - masonry applies the backing scale itself at paint
    /// time (see `masonry_imaging::imaging::render::PreparedFrame`), so
    /// layout must not be pre-scaled here.
    ///
    /// # Safety
    ///
    /// The window handle must remain valid for the lifetime of this context.
    pub unsafe fn new<W>(window: &W, width: u32, height: u32) -> Result<Self, RenderError>
    where
        W: raw_window_handle::HasRawWindowHandle + raw_window_handle::HasRawDisplayHandle,
    {
        #[allow(unused_imports)]
        use raw_window_handle::{HasRawDisplayHandle, HasRawWindowHandle};
        let instance = Instance::new(&InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let raw_window = window.raw_window_handle();
        let raw_display = window.raw_display_handle();

        let surface = instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: convert_display_handle(raw_display),
                raw_window_handle: convert_window_handle(raw_window),
            })
            .map_err(|e: wgpu::CreateSurfaceError| RenderError::Surface(e.to_string()))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e: wgpu::RequestAdapterError| {
            RenderError::Device(format!("Adapter request failed: {:?}", e))
        })?;

        let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
            required_features: Features::empty(),
            required_limits: Limits::default(),
            label: Some("xilem_baseview"),
            memory_hints: wgpu::MemoryHints::default(),
            ..Default::default()
        }))
        .map_err(|e: wgpu::RequestDeviceError| RenderError::Device(format!("{:?}", e)))?;

        let device = Arc::new(device);
        let queue = Arc::new(queue);

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .find(|f: &&TextureFormat| !f.is_srgb())
            .copied()
            .unwrap_or(TextureFormat::Bgra8Unorm);

        let alpha_mode = if caps
            .alpha_modes
            .contains(&CompositeAlphaMode::PreMultiplied)
        {
            CompositeAlphaMode::PreMultiplied
        } else {
            CompositeAlphaMode::Auto
        };

        let width = width.max(1);
        let height = height.max(1);

        let surface_config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode: PresentMode::AutoVsync,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };

        surface.configure(&device, &surface_config);

        let (target_texture, target_view) = create_target_texture(&device, width, height);
        let blitter = TextureBlitter::new(&device, surface_format);

        let renderer = ImagingRenderer::new((*device).clone(), (*queue).clone())
            .map_err(|e| RenderError::Renderer(e.to_string()))?;

        Ok(Self {
            device,
            queue,
            renderer,
            surface,
            surface_config,
            target_texture,
            target_view,
            blitter,
        })
    }

    /// Resize the rendering surface. `width`/`height` are physical pixels
    /// (the actual on-screen surface resolution).
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);

        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);

        let (target_texture, target_view) = create_target_texture(&self.device, width, height);
        self.target_texture = target_texture;
        self.target_view = target_view;
    }

    /// Render a masonry frame (base scene plus overlays) to the surface.
    ///
    /// `scale` is the display's backing scale factor; masonry_imaging's
    /// `PreparedFrame` applies it internally when compositing, so the base
    /// scene and overlay layers passed in must be in logical-point
    /// coordinates, not pre-scaled.
    pub fn render(
        &mut self,
        base: &masonry::imaging::record::Scene,
        overlays: &[ImagingLayer<'_>],
        base_color: Color,
        scale: f64,
    ) -> Result<(), RenderError> {
        let width = self.surface_config.width;
        let height = self.surface_config.height;

        let mut frame = PreparedFrame::new(width, height, scale, base_color, base, overlays);

        self.renderer
            .render_source_into_texture(
                &mut frame,
                TextureTarget {
                    view: self.target_view.clone(),
                    width,
                    height,
                },
            )
            .map_err(|e| RenderError::Renderer(e.to_string()))?;

        let surface_texture = self
            .surface
            .get_current_texture()
            .map_err(|e: wgpu::SurfaceError| RenderError::Surface(e.to_string()))?;

        let surface_view = surface_texture
            .texture
            .create_view(&TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xilem_baseview_surface_blit"),
            });
        self.blitter
            .copy(&self.device, &mut encoder, &self.target_view, &surface_view);
        self.queue.submit(std::iter::once(encoder.finish()));

        surface_texture.present();

        Ok(())
    }
}

/// Create the intermediate Rgba8Unorm render target masonry_imaging's Vello
/// backend renders into (see RenderContext's doc comment for why this is
/// needed instead of targeting the surface directly).
fn create_target_texture(device: &Device, width: u32, height: u32) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some("xilem_baseview_target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::STORAGE_BINDING
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}

/// Errors that can occur during rendering.
#[derive(Debug)]
#[allow(dead_code)]
pub enum RenderError {
    NoAdapter,
    Device(String),
    Surface(String),
    Renderer(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAdapter => write!(f, "No suitable GPU adapter found"),
            Self::Device(e) => write!(f, "Device error: {}", e),
            Self::Surface(e) => write!(f, "Surface error: {}", e),
            Self::Renderer(e) => write!(f, "Renderer error: {}", e),
        }
    }
}

impl std::error::Error for RenderError {}

/// Convert raw_window_handle 0.5 display handle to wgpu's rwh format.
fn convert_display_handle(
    handle: raw_window_handle::RawDisplayHandle,
) -> wgpu::rwh::RawDisplayHandle {
    use raw_window_handle::RawDisplayHandle as Old;
    use wgpu::rwh::RawDisplayHandle as New;

    match handle {
        #[cfg(target_os = "macos")]
        Old::AppKit(_) => New::AppKit(wgpu::rwh::AppKitDisplayHandle::new()),

        #[cfg(target_os = "linux")]
        Old::Xlib(h) => New::Xlib(wgpu::rwh::XlibDisplayHandle::new(
            std::ptr::NonNull::new(h.display),
            h.screen,
        )),

        #[cfg(target_os = "linux")]
        Old::Xcb(h) => New::Xcb(wgpu::rwh::XcbDisplayHandle::new(
            std::ptr::NonNull::new(h.connection),
            h.screen,
        )),

        #[cfg(target_os = "linux")]
        Old::Wayland(h) => New::Wayland(wgpu::rwh::WaylandDisplayHandle::new(
            std::ptr::NonNull::new(h.display).unwrap(),
        )),

        #[cfg(target_os = "windows")]
        Old::Windows(_) => New::Windows(wgpu::rwh::WindowsDisplayHandle::new()),

        _ => panic!("Unsupported display handle type"),
    }
}

/// Convert raw_window_handle 0.5 window handle to wgpu's rwh format.
fn convert_window_handle(handle: raw_window_handle::RawWindowHandle) -> wgpu::rwh::RawWindowHandle {
    use raw_window_handle::RawWindowHandle as Old;
    use wgpu::rwh::RawWindowHandle as New;

    match handle {
        #[cfg(target_os = "macos")]
        Old::AppKit(h) => {
            let new_handle =
                wgpu::rwh::AppKitWindowHandle::new(std::ptr::NonNull::new(h.ns_view).unwrap());
            New::AppKit(new_handle)
        }

        #[cfg(target_os = "linux")]
        Old::Xlib(h) => New::Xlib(wgpu::rwh::XlibWindowHandle::new(h.window)),

        #[cfg(target_os = "linux")]
        Old::Xcb(h) => New::Xcb(wgpu::rwh::XcbWindowHandle::new(
            std::num::NonZeroU32::new(h.window).unwrap(),
        )),

        #[cfg(target_os = "linux")]
        Old::Wayland(h) => New::Wayland(wgpu::rwh::WaylandWindowHandle::new(
            std::ptr::NonNull::new(h.surface).unwrap(),
        )),

        #[cfg(target_os = "windows")]
        Old::Win32(h) => {
            let mut new_handle = wgpu::rwh::Win32WindowHandle::new(
                std::num::NonZeroIsize::new(h.hwnd as isize).unwrap(),
            );
            new_handle.hinstance = std::num::NonZeroIsize::new(h.hinstance as isize);
            New::Win32(new_handle)
        }

        _ => panic!("Unsupported window handle type"),
    }
}
