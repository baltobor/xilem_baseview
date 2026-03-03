//! This file is part of the xilem_baseview project.
//! (c) 2026 by Jacek Wisniowski
//!
//! This project was released as open source under the
//! Apache License, Version 2.0: http://www.apache.org/licenses/LICENSE-2.0
//! (compatible with Xilem).
//!
//! Rendering pipeline for Xilem in baseview
//!
//! Sets up wgpu surface and Vello renderer for drawing masonry widgets.
//! Uses an intermediate texture because Vello uses compute shaders that
//! can't directly target surface textures.
//!
//! Adapted from masonry_baseview.
//! (see https://github.com/baltobor/masonry_baseview for reference)

use std::sync::Arc;
use vello::peniko::Color;
use vello::wgpu;
use vello::{AaConfig, RenderParams, Renderer, RendererOptions, Scene};
use wgpu::{
    BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, BlendState, ColorTargetState,
    ColorWrites, CompositeAlphaMode, Device, DeviceDescriptor, Features, FragmentState, Instance,
    InstanceDescriptor, Limits, MultisampleState, PipelineLayoutDescriptor, PresentMode,
    PrimitiveState, Queue, RenderPipeline, RenderPipelineDescriptor, Sampler, SamplerBindingType,
    SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, Surface,
    SurfaceConfiguration, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureSampleType, TextureUsages, TextureView, TextureViewDescriptor, TextureViewDimension,
    VertexState,
};

/// GPU rendering context for Vello with intermediate texture blitting.
pub struct RenderContext {
    pub device: Arc<Device>,
    pub queue: Arc<Queue>,
    pub renderer: Renderer,
    pub surface: Surface<'static>,
    pub surface_config: SurfaceConfiguration,
    target_texture: Texture,
    target_view: TextureView,
    blit_pipeline: RenderPipeline,
    blit_bind_group_layout: BindGroupLayout,
    blit_sampler: Sampler,
}

impl RenderContext {
    /// Create a new render context for a window.
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
        .map_err(|e: wgpu::RequestAdapterError| RenderError::Device(format!("Adapter request failed: {:?}", e)))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &DeviceDescriptor {
                required_features: Features::empty(),
                required_limits: Limits::default(),
                label: Some("xilem_baseview"),
                memory_hints: wgpu::MemoryHints::default(),
                ..Default::default()
            },
        ))
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

        let alpha_mode = if caps.alpha_modes.contains(&CompositeAlphaMode::PreMultiplied) {
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

        let target_format = TextureFormat::Rgba8Unorm;
        let (target_texture, target_view) =
            create_target_texture(&device, width, height, target_format);

        let (blit_pipeline, blit_bind_group_layout, blit_sampler) =
            create_blit_pipeline(&device, surface_format);

        let renderer = Renderer::new(
            &*device,
            RendererOptions {
                use_cpu: false,
                antialiasing_support: vello::AaSupport::all(),
                num_init_threads: None,
                pipeline_cache: None,
            },
        )
        .map_err(|e: vello::Error| RenderError::Renderer(e.to_string()))?;

        Ok(Self {
            device,
            queue,
            renderer,
            surface,
            surface_config,
            target_texture,
            target_view,
            blit_pipeline,
            blit_bind_group_layout,
            blit_sampler,
        })
    }

    /// Resize the rendering surface.
    pub fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);

        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);

        let (target_texture, target_view) =
            create_target_texture(&self.device, width, height, TextureFormat::Rgba8Unorm);
        self.target_texture = target_texture;
        self.target_view = target_view;
    }

    /// Render a Vello scene to the surface.
    pub fn render(&mut self, scene: &Scene, base_color: Color) -> Result<(), RenderError> {
        let width = self.surface_config.width;
        let height = self.surface_config.height;

        let render_params = RenderParams {
            base_color,
            width,
            height,
            antialiasing_method: AaConfig::Msaa16,
        };

        self.renderer
            .render_to_texture(
                &*self.device,
                &*self.queue,
                scene,
                &self.target_view,
                &render_params,
            )
            .map_err(|e: vello::Error| RenderError::Renderer(format!("{:?}", e)))?;

        let surface_texture = self
            .surface
            .get_current_texture()
            .map_err(|e: wgpu::SurfaceError| RenderError::Surface(e.to_string()))?;

        let surface_view = surface_texture
            .texture
            .create_view(&TextureViewDescriptor::default());

        let bind_group = self.device.create_bind_group(&BindGroupDescriptor {
            label: Some("blit_bind_group"),
            layout: &self.blit_bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&self.target_view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&self.blit_sampler),
                },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("blit_encoder"),
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &surface_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            render_pass.set_pipeline(&self.blit_pipeline);
            render_pass.set_bind_group(0, &bind_group, &[]);
            render_pass.draw(0..3, 0..1);
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();

        Ok(())
    }
}

fn create_target_texture(
    device: &Device,
    width: u32,
    height: u32,
    format: TextureFormat,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some("vello_target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });

    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}

// TODO: Does this work on all platforms?
// TODO: Tested on MacOS and ArchLinux and Cosmic.
fn create_blit_pipeline(
    device: &Device,
    target_format: TextureFormat,
) -> (RenderPipeline, BindGroupLayout, Sampler) {
    let shader_source = r#"
        @group(0) @binding(0) var t_texture: texture_2d<f32>;
        @group(0) @binding(1) var s_sampler: sampler;

        struct VertexOutput {
            @builtin(position) position: vec4<f32>,
            @location(0) tex_coord: vec2<f32>,
        }

        @vertex
        fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
            var out: VertexOutput;
            let x = f32(i32(vertex_index) / 2) * 4.0 - 1.0;
            let y = f32(i32(vertex_index) % 2) * 4.0 - 1.0;
            out.position = vec4<f32>(x, y, 0.0, 1.0);
            out.tex_coord = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
            return out;
        }

        @fragment
        fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
            return textureSample(t_texture, s_sampler, in.tex_coord);
        }
    "#;

    let shader = device.create_shader_module(ShaderModuleDescriptor {
        label: Some("blit_shader"),
        source: ShaderSource::Wgsl(shader_source.into()),
    });

    let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: Some("blit_bind_group_layout"),
        entries: &[
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: true },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 1,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Sampler(SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("blit_pipeline_layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });

    let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("blit_pipeline"),
        layout: Some(&pipeline_layout),
        vertex: VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(ColorTargetState {
                format: target_format,
                blend: Some(BlendState::REPLACE),
                write_mask: ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: PrimitiveState::default(),
        depth_stencil: None,
        multisample: MultisampleState::default(),
        multiview: None,
        cache: None,
    });

    let sampler = device.create_sampler(&SamplerDescriptor {
        label: Some("blit_sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    (pipeline, bind_group_layout, sampler)
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

/// Convert raw_window_handle 0.5 display handle to 0.6 format.
fn convert_display_handle(
    handle: raw_window_handle::RawDisplayHandle,
) -> wgpu::rwh::RawDisplayHandle {
    use raw_window_handle::RawDisplayHandle as Old;
    use wgpu::rwh::RawDisplayHandle as New;

    match handle {
        #[cfg(target_os = "macos")]
        Old::AppKit(_) => New::AppKit(wgpu::rwh::AppKitDisplayHandle::new()),

        // TODO: I tested Wayland. Xlib (hopefully) works as well.
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

        // TODO: Untested! But might work.
        #[cfg(target_os = "windows")]
        Old::Windows(_) => New::Windows(wgpu::rwh::WindowsDisplayHandle::new()),

        _ => panic!("Unsupported display handle type"),
    }
}

/// Convert raw_window_handle 0.5 window handle to 0.6 format.
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

        // TODO: I tested Wayland. Xlib (hopefully) works as well.
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

        // TODO: Untested! But might work.
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
