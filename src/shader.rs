use std::collections::HashMap;

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};

use crate::theme::ResolvedTheme;

const PDF_THEME_WGSL: &str = r#"
struct Uniforms {
    bg: vec4<f32>,
    fg: vec4<f32>,
    uv_rect: vec4<f32>, // (u_min, v_min, u_max, v_max)
    recolor: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var page_tex: texture_2d<f32>;
@group(0) @binding(1) var page_sampler: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );
    var t = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );

    var out: VertexOutput;
    out.position = vec4<f32>(pos[idx], 0.0, 1.0);
    let u = mix(uniforms.uv_rect.x, uniforms.uv_rect.z, t[idx].x);
    let v = mix(uniforms.uv_rect.y, uniforms.uv_rect.w, t[idx].y);
    out.uv = vec2<f32>(u, v);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let src = textureSample(page_tex, page_sampler, in.uv);
    if (uniforms.recolor < 0.5) {
        return vec4<f32>(src.rgb, 1.0);
    }

    let luma = dot(src.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let chroma = src.rgb - vec3<f32>(luma);
    let mapped = mix(uniforms.fg.rgb, uniforms.bg.rgb, luma) + chroma * 0.85;
    return vec4<f32>(clamp(mapped, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct ThemeUniforms {
    bg: [f32; 4],
    fg: [f32; 4],
    uv_rect: [f32; 4],
    recolor: f32,
    _pad: [f32; 3],
}

impl ThemeUniforms {
    fn from_theme(theme: &ResolvedTheme, uv_rect: [f32; 4]) -> Self {
        Self {
            bg: [
                theme.background[0] as f32 / 255.0,
                theme.background[1] as f32 / 255.0,
                theme.background[2] as f32 / 255.0,
                1.0,
            ],
            fg: [
                theme.foreground[0] as f32 / 255.0,
                theme.foreground[1] as f32 / 255.0,
                theme.foreground[2] as f32 / 255.0,
                1.0,
            ],
            uv_rect,
            recolor: if theme.recolor { 1.0 } else { 0.0 },
            _pad: [0.0; 3],
        }
    }

    fn to_bytes(self) -> [u8; 64] {
        let mut out = [0u8; 64];
        let floats: [f32; 16] = [
            self.bg[0],
            self.bg[1],
            self.bg[2],
            self.bg[3],
            self.fg[0],
            self.fg[1],
            self.fg[2],
            self.fg[3],
            self.uv_rect[0],
            self.uv_rect[1],
            self.uv_rect[2],
            self.uv_rect[3],
            self.recolor,
            self._pad[0],
            self._pad[1],
            self._pad[2],
        ];
        for (i, f) in floats.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&f.to_ne_bytes());
        }
        out
    }
}

struct PageGpuEntry {
    _texture: wgpu::Texture,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

pub struct PdfShaderResources {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    bind_group_layout: wgpu::BindGroupLayout,
    entries: HashMap<u64, PageGpuEntry>,
}

impl PdfShaderResources {
    pub fn init(render_state: &egui_wgpu::RenderState) {
        let device = &render_state.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pdf_theme_shader"),
            source: wgpu::ShaderSource::Wgsl(PDF_THEME_WGSL.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pdf_page_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
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
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pdf_theme_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pdf_theme_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: render_state.target_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pdf_page_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        render_state
            .renderer
            .write()
            .callback_resources
            .insert(Self {
                pipeline,
                sampler,
                bind_group_layout,
                entries: HashMap::new(),
            });
    }

    pub fn upload_page(
        render_state: &egui_wgpu::RenderState,
        key: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) {
        let device = &render_state.device;
        let queue = &render_state.queue;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pdf_page_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pdf_page_uniforms"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut renderer = render_state.renderer.write();
        if let Some(res) = renderer.callback_resources.get_mut::<Self>() {
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("pdf_page_bind_group"),
                layout: &res.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&res.sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: uniform_buffer.as_entire_binding(),
                    },
                ],
            });

            res.entries.insert(
                key,
                PageGpuEntry {
                    _texture: texture,
                    uniform_buffer,
                    bind_group,
                },
            );
        }
    }

    pub fn remove_page(render_state: &egui_wgpu::RenderState, key: u64) {
        let mut renderer = render_state.renderer.write();
        if let Some(res) = renderer.callback_resources.get_mut::<Self>() {
            res.entries.remove(&key);
        }
    }
}

struct PdfPageCallback {
    key: u64,
    uniforms: ThemeUniforms,
}

impl egui_wgpu::CallbackTrait for PdfPageCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        callback_resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(res) = callback_resources.get::<PdfShaderResources>()
            && let Some(entry) = res.entries.get(&self.key)
        {
            queue.write_buffer(&entry.uniform_buffer, 0, &self.uniforms.to_bytes());
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(res) = callback_resources.get::<PdfShaderResources>() else {
            return;
        };
        let Some(entry) = res.entries.get(&self.key) else {
            return;
        };

        render_pass.set_pipeline(&res.pipeline);
        render_pass.set_bind_group(0, &entry.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
    }
}

/// Build an `egui::PaintCallback` that renders a cached GPU page texture through the WGSL theme shader.
pub fn paint_page_callback(
    page_rect: egui::Rect,
    clip_rect: egui::Rect,
    key: u64,
    theme: &ResolvedTheme,
) -> Option<egui::PaintCallback> {
    let draw_rect = page_rect.intersect(clip_rect);
    if !draw_rect.is_positive() || page_rect.width() <= 0.0 || page_rect.height() <= 0.0 {
        return None;
    }

    let u_min = ((draw_rect.min.x - page_rect.min.x) / page_rect.width()).clamp(0.0, 1.0);
    let v_min = ((draw_rect.min.y - page_rect.min.y) / page_rect.height()).clamp(0.0, 1.0);
    let u_max = ((draw_rect.max.x - page_rect.min.x) / page_rect.width()).clamp(0.0, 1.0);
    let v_max = ((draw_rect.max.y - page_rect.min.y) / page_rect.height()).clamp(0.0, 1.0);

    let uniforms = ThemeUniforms::from_theme(theme, [u_min, v_min, u_max, v_max]);
    let cb = PdfPageCallback { key, uniforms };
    Some(egui_wgpu::Callback::new_paint_callback(draw_rect, cb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::builtin_themes;

    #[test]
    fn theme_uniforms_pack_to_64_bytes() {
        let themes = builtin_themes();
        let dark = ResolvedTheme::from_spec("dark", &themes["dark"]);
        let u = ThemeUniforms::from_theme(&dark, [0.0, 0.25, 1.0, 0.75]);
        let bytes = u.to_bytes();
        assert_eq!(bytes.len(), 64);
        assert_eq!(f32::from_ne_bytes(bytes[48..52].try_into().unwrap()), 1.0);
    }
}
