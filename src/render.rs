use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use winit::dpi::PhysicalSize;
use winit::window::Window;

use crate::types::{ColorInfo, FrameData, VideoFormat, VideoFrame};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, PartialEq)]
struct ColorParams {
    y_offset: f32,
    y_scale: f32,
    m_rv: f32,
    m_gu: f32,
    m_gv: f32,
    m_bu: f32,
    srgb_output: f32,
    _pad: f32,
}

fn color_params_from_info(color: ColorInfo, output_is_srgb: bool) -> ColorParams {
    let (y_offset, y_scale) = match color.range {
        crate::types::ColorRange::Limited => (-16.0 / 255.0, 1.164_383_6),
        crate::types::ColorRange::Full => (0.0, 1.0),
    };
    let (m_rv, m_gu, m_gv, m_bu) = match (color.matrix, color.range) {
        (crate::types::ColorMatrix::Bt709, crate::types::ColorRange::Limited) => {
            (1.793, 0.213, 0.533, 2.112)
        }
        (crate::types::ColorMatrix::Bt2020, crate::types::ColorRange::Limited) => {
            (1.678, 0.187, 0.650, 2.141)
        }
        (crate::types::ColorMatrix::Bt601, crate::types::ColorRange::Limited) => {
            (1.596, 0.392, 0.813, 2.017)
        }
        (crate::types::ColorMatrix::Bt709, crate::types::ColorRange::Full) => {
            (1.575, 0.187, 0.468, 1.856)
        }
        (crate::types::ColorMatrix::Bt2020, crate::types::ColorRange::Full) => {
            (1.4746, 0.1645, 0.5714, 1.8814)
        }
        (crate::types::ColorMatrix::Bt601, crate::types::ColorRange::Full) => {
            (1.402, 0.344, 0.714, 1.772)
        }
    };
    ColorParams {
        y_offset,
        y_scale,
        m_rv,
        m_gu,
        m_gv,
        m_bu,
        srgb_output: if output_is_srgb { 1.0 } else { 0.0 },
        _pad: 0.0,
    }
}

const VERTICES: [Vertex; 4] = [
    Vertex {
        pos: [-1.0, -1.0],
        uv: [0.0, 1.0],
    },
    Vertex {
        pos: [1.0, -1.0],
        uv: [1.0, 1.0],
    },
    Vertex {
        pos: [1.0, 1.0],
        uv: [1.0, 0.0],
    },
    Vertex {
        pos: [-1.0, 1.0],
        uv: [0.0, 0.0],
    },
];

const INDICES: [u16; 6] = [0, 1, 2, 2, 3, 0];

pub struct RenderState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    pipeline_rgba: wgpu::RenderPipeline,
    pipeline_yuyv: wgpu::RenderPipeline,
    pipeline_nv12: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    nv12_bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    video_texture: wgpu::Texture,
    video_view: wgpu::TextureView,
    video_bind_group: wgpu::BindGroup,
    nv12_y_texture: wgpu::Texture,
    nv12_uv_texture: wgpu::Texture,
    nv12_y_view: wgpu::TextureView,
    nv12_uv_view: wgpu::TextureView,
    nv12_bind_group: wgpu::BindGroup,
    video_size: (u32, u32),
    video_format: VideoFormat,
    output_is_srgb: bool,
    color_params: ColorParams,
    color_buffer: wgpu::Buffer,
    aspect_correct: bool,
    staging: Vec<u8>,
}

impl RenderState {
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub async fn new(window: Arc<Window>) -> Result<Self> {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window)?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| anyhow!("{e:?}"))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: None,
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                ..Default::default()
            })
            .await?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Immediate) {
            wgpu::PresentMode::Immediate
        } else if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![format],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);
        let output_is_srgb = format.is_srgb();
        let color_params = color_params_from_info(ColorInfo::default(), output_is_srgb);
        let color_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("color_params"),
            contents: bytemuck::bytes_of(&color_params),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("video_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let nv12_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("nv12_bind_group_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video_shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER.into()),
        });
        let nv12_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12_shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video_pipeline_layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline_nv12_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12_pipeline_layout"),
            bind_group_layouts: &[&nv12_bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline_rgba = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video_pipeline_rgba"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let pipeline_yuyv = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video_pipeline_yuyv"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_yuyv"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let pipeline_nv12 = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video_pipeline_nv12"),
            layout: Some(&pipeline_nv12_layout),
            vertex: wgpu::VertexState {
                module: &nv12_shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                        wgpu::VertexAttribute {
                            offset: 8,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x2,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &nv12_shader,
                entry_point: Some("fs_nv12"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("video_vertex_buffer"),
            contents: bytemuck::cast_slice(&VERTICES),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("video_index_buffer"),
            contents: bytemuck::cast_slice(&INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });
        let (video_texture, video_view) = create_video_texture(&device, 1, 1, wgpu::TextureFormat::Rgba8Unorm);
        let video_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video_bind_group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&video_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: color_buffer.as_entire_binding(),
                },
            ],
        });
        let (nv12_y_texture, nv12_y_view) =
            create_video_texture(&device, 1, 1, wgpu::TextureFormat::R8Unorm);
        let (nv12_uv_texture, nv12_uv_view) =
            create_video_texture(&device, 1, 1, wgpu::TextureFormat::Rg8Unorm);
        let nv12_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12_bind_group"),
            layout: &nv12_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&nv12_y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&nv12_uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: color_buffer.as_entire_binding(),
                },
            ],
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            size,
            pipeline_rgba,
            pipeline_yuyv,
            pipeline_nv12,
            bind_group_layout,
            nv12_bind_group_layout,
            sampler,
            vertex_buffer,
            index_buffer,
            num_indices: INDICES.len() as u32,
            video_texture,
            video_view,
            video_bind_group,
            nv12_y_texture,
            nv12_uv_texture,
            nv12_y_view,
            nv12_uv_view,
            nv12_bind_group,
            video_size: (1, 1),
            video_format: VideoFormat::Rgba,
            output_is_srgb,
            color_params,
            color_buffer,
            aspect_correct: true,
            staging: Vec::new(),
        })
    }

    pub fn resize(&mut self, new_size: PhysicalSize<u32>) {
        if new_size.width > 0 && new_size.height > 0 {
            self.size = new_size;
            self.config.width = new_size.width;
            self.config.height = new_size.height;
            self.surface.configure(&self.device, &self.config);
            self.update_vertices();
        }
    }

    pub fn set_aspect_correction(&mut self, enabled: bool) {
        if self.aspect_correct != enabled {
            self.aspect_correct = enabled;
            self.update_vertices();
        }
    }

    pub fn update_frame(&mut self, frame: &VideoFrame) {
        self.update_color_params(frame.color);
        match &frame.data {
            FrameData::Owned(data) => self.upload_frame(frame, data),
            #[cfg(target_os = "linux")]
            FrameData::Gst(buffer) => {
                if let Ok(map) = buffer.map_readable() {
                    self.upload_frame(frame, map.as_slice());
                }
            }
        }
    }

    fn update_color_params(&mut self, color: ColorInfo) {
        let params = color_params_from_info(color, self.output_is_srgb);
        if params != self.color_params {
            self.color_params = params;
            self.queue
                .write_buffer(&self.color_buffer, 0, bytemuck::bytes_of(&params));
        }
    }

    fn upload_frame(&mut self, frame: &VideoFrame, data: &[u8]) {
        let size_changed = self.video_size != (frame.width, frame.height);
        let format_changed = self.video_format != frame.format;
        self.video_format = frame.format;
        self.video_size = (frame.width, frame.height);
        if size_changed {
            self.update_vertices();
        }
        match frame.format {
            VideoFormat::Rgba => {
                if size_changed || format_changed {
                    let (tex, view) = create_video_texture(
                        &self.device,
                        frame.width,
                        frame.height,
                        wgpu::TextureFormat::Rgba8Unorm,
                    );
                    self.video_texture = tex;
                    self.video_view = view;
                    self.video_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("video_bind_group"),
                        layout: &self.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&self.video_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: self.color_buffer.as_entire_binding(),
                            },
                        ],
                    });
                }
                let texture = self.video_texture.clone();
                self.write_texture_padded(
                    &texture,
                    frame.width,
                    frame.height,
                    frame.stride as u32,
                    data,
                );
            }
            VideoFormat::Yuyv => {
                if size_changed || format_changed {
                    let (tex, view) = create_video_texture(
                        &self.device,
                        frame.width,
                        frame.height,
                        wgpu::TextureFormat::Rg8Unorm,
                    );
                    self.video_texture = tex;
                    self.video_view = view;
                    self.video_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("video_bind_group"),
                        layout: &self.bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&self.video_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: self.color_buffer.as_entire_binding(),
                            },
                        ],
                    });
                }
                let texture = self.video_texture.clone();
                self.write_texture_padded(
                    &texture,
                    frame.width,
                    frame.height,
                    frame.stride as u32,
                    data,
                );
            }
            VideoFormat::Nv12 => {
                let uv_width = frame.width.div_ceil(2);
                let uv_height = frame.height.div_ceil(2);
                if size_changed || format_changed {
                    let (y_tex, y_view) =
                        create_video_texture(&self.device, frame.width, frame.height, wgpu::TextureFormat::R8Unorm);
                    let (uv_tex, uv_view) =
                        create_video_texture(&self.device, uv_width, uv_height, wgpu::TextureFormat::Rg8Unorm);
                    self.nv12_y_texture = y_tex;
                    self.nv12_uv_texture = uv_tex;
                    self.nv12_y_view = y_view;
                    self.nv12_uv_view = uv_view;
                    self.nv12_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("nv12_bind_group"),
                        layout: &self.nv12_bind_group_layout,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::TextureView(&self.nv12_y_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::TextureView(&self.nv12_uv_view),
                            },
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Sampler(&self.sampler),
                            },
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: self.color_buffer.as_entire_binding(),
                            },
                        ],
                    });
                }
                let data_len = data.len();
                let y_bytes = (frame.stride * frame.height as usize).min(data_len);
                let y_data = &data[..y_bytes];
                let y_texture = self.nv12_y_texture.clone();
                self.write_texture_padded(
                    &y_texture,
                    frame.width,
                    frame.height,
                    frame.stride as u32,
                    y_data,
                );
                let uv_bytes = frame.uv_stride * uv_height as usize;
                let uv_start = y_bytes;
                let uv_len = uv_bytes.min(data_len.saturating_sub(uv_start));
                let uv_data = &data[uv_start..uv_start + uv_len];
                let uv_texture = self.nv12_uv_texture.clone();
                self.write_texture_padded(
                    &uv_texture,
                    uv_width,
                    uv_height,
                    frame.uv_stride as u32,
                    uv_data,
                );
            }
        }
    }

    pub fn render(
        &mut self,
        window: &Window,
        egui_renderer: &mut egui_wgpu::Renderer,
        textures_delta: &egui::TexturesDelta,
        clipped_primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
    ) -> Result<()> {
        let output = self.surface.get_current_texture()?;
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("render_encoder"),
                });
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        for (id, image_delta) in &textures_delta.set {
            egui_renderer.update_texture(&self.device, &self.queue, *id, image_delta);
        }
        let has_ui = !clipped_primitives.is_empty();
        if has_ui {
            egui_renderer.update_buffers(
                &self.device,
                &self.queue,
                &mut encoder,
                clipped_primitives,
                &screen_descriptor,
            );
        }
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
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
            match self.video_format {
                VideoFormat::Rgba => {
                    rpass.set_pipeline(&self.pipeline_rgba);
                    rpass.set_bind_group(0, &self.video_bind_group, &[]);
                }
                VideoFormat::Yuyv => {
                    rpass.set_pipeline(&self.pipeline_yuyv);
                    rpass.set_bind_group(0, &self.video_bind_group, &[]);
                }
                VideoFormat::Nv12 => {
                    rpass.set_pipeline(&self.pipeline_nv12);
                    rpass.set_bind_group(0, &self.nv12_bind_group, &[]);
                }
            }
            rpass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            rpass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            rpass.draw_indexed(0..self.num_indices, 0, 0..1);
            if has_ui {
                let mut rpass = rpass.forget_lifetime();
                egui_renderer.render(&mut rpass, clipped_primitives, &screen_descriptor);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        window.pre_present_notify();
        output.present();
        for id in &textures_delta.free {
            egui_renderer.free_texture(id);
        }
        Ok(())
    }
}

fn create_video_texture(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video_texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

const VIDEO_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct ColorParams {
    y_offset: f32,
    y_scale: f32,
    m_rv: f32,
    m_gu: f32,
    m_gv: f32,
    m_bu: f32,
    srgb_output: f32,
    _pad: f32,
};

@group(0) @binding(0) var video_tex: texture_2d<f32>;
@group(0) @binding(1) var video_sampler: sampler;
@group(0) @binding(2) var<uniform> color: ColorParams;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.04045);
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(low, high, c > cutoff);
}

fn apply_output_color(rgb: vec3<f32>) -> vec3<f32> {
    if color.srgb_output > 0.5 {
        return srgb_to_linear(rgb);
    }
    return rgb;
}

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let src = textureSample(video_tex, video_sampler, in.uv);
    let rgb = apply_output_color(src.rgb);
    return vec4<f32>(rgb, src.a);
}

@fragment
fn fs_yuyv(in: VsOut) -> @location(0) vec4<f32> {
    let tex_size = textureDimensions(video_tex);
    let x = clamp(i32(floor(in.uv.x * f32(tex_size.x))), 0, i32(tex_size.x) - 1);
    let y = clamp(i32(floor(in.uv.y * f32(tex_size.y))), 0, i32(tex_size.y) - 1);
    let even = (x & 1) == 0;
    let x_prev = max(x - 1, 0);
    let x_next = min(x + 1, i32(tex_size.x) - 1);
    let cur = textureLoad(video_tex, vec2<i32>(x, y), 0).rg;
    let other = textureLoad(video_tex, vec2<i32>(select(x_prev, x_next, even), y), 0).rg;
    let yv = cur.r;
    let u = select(other.g, cur.g, even);
    let v = select(cur.g, other.g, even);
    let c = (yv + color.y_offset) * color.y_scale;
    let d = u - 0.5;
    let e = v - 0.5;
    let r = c + color.m_rv * e;
    let g = c - color.m_gu * d - color.m_gv * e;
    let b = c + color.m_bu * d;
    let rgb = apply_output_color(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)));
    return vec4<f32>(rgb, 1.0);
}
"#;

const NV12_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct ColorParams {
    y_offset: f32,
    y_scale: f32,
    m_rv: f32,
    m_gu: f32,
    m_gv: f32,
    m_bu: f32,
    srgb_output: f32,
    _pad: f32,
};

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var nv_sampler: sampler;
@group(0) @binding(3) var<uniform> color: ColorParams;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let cutoff = vec3<f32>(0.04045);
    let low = c / 12.92;
    let high = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(low, high, c > cutoff);
}

fn apply_output_color(rgb: vec3<f32>) -> vec3<f32> {
    if color.srgb_output > 0.5 {
        return srgb_to_linear(rgb);
    }
    return rgb;
}

@vertex
fn vs_main(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_nv12(in: VsOut) -> @location(0) vec4<f32> {
    let y = textureSample(y_tex, nv_sampler, in.uv).r;
    let uv = textureSample(uv_tex, nv_sampler, in.uv).rg;
    let c = (y + color.y_offset) * color.y_scale;
    let d = uv.x - 0.5;
    let e = uv.y - 0.5;
    let r = c + color.m_rv * e;
    let g = c - color.m_gu * d - color.m_gv * e;
    let b = c + color.m_bu * d;
    let rgb = apply_output_color(clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0)));
    return vec4<f32>(rgb, 1.0);
}
"#;

impl RenderState {
    fn write_texture_padded(
        &mut self,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        bytes_per_row: u32,
        data: &[u8],
    ) {
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let expected = (bytes_per_row * height) as usize;
        let needs_pad = bytes_per_row % align != 0;
        let needs_fill = data.len() < expected;
        let (data, padded_stride) = if !needs_pad && !needs_fill {
            self.staging.clear();
            (data, bytes_per_row)
        } else {
            let padded_stride = bytes_per_row.div_ceil(align) * align;
            self.staging
                .resize((padded_stride * height) as usize, 0);
            for y in 0..height as usize {
                let row_start = y * bytes_per_row as usize;
                if row_start >= data.len() {
                    break;
                }
                let row_end = (row_start + bytes_per_row as usize).min(data.len());
                let dst = &mut self.staging[y * padded_stride as usize..][..row_end - row_start];
                dst.copy_from_slice(&data[row_start..row_end]);
            }
            (self.staging.as_slice(), padded_stride)
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_stride),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn update_vertices(&mut self) {
        let window_w = self.size.width as f32;
        let window_h = self.size.height as f32;
        if window_w <= 0.0 || window_h <= 0.0 {
            return;
        }
        let (sx, sy) = if self.aspect_correct {
            let video_w = self.video_size.0 as f32;
            let video_h = self.video_size.1 as f32;
            if video_w <= 0.0 || video_h <= 0.0 {
                return;
            }
            let window_aspect = window_w / window_h;
            let video_aspect = video_w / video_h;
            if window_aspect >= video_aspect {
                (video_aspect / window_aspect, 1.0)
            } else {
                (1.0, window_aspect / video_aspect)
            }
        } else {
            (1.0, 1.0)
        };
        let vertices = [
            Vertex {
                pos: [-sx, -sy],
                uv: [0.0, 1.0],
            },
            Vertex {
                pos: [sx, -sy],
                uv: [1.0, 1.0],
            },
            Vertex {
                pos: [sx, sy],
                uv: [1.0, 0.0],
            },
            Vertex {
                pos: [-sx, sy],
                uv: [0.0, 0.0],
            },
        ];
        self.queue
            .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
    }
}
