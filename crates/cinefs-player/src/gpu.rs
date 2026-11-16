//! wgpu renderer: uploads a Bayer mosaic (`R16Uint`), debayers + colour-converts on the GPU into a
//! linear `Rgba16Float` image, then displays it aspect-fitted with the Rec.709 transfer curve.

use bytemuck::{Pod, Zeroable};

/// Everything the GPU needs to know about the frames of one clip.
#[derive(Clone, Debug)]
pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    /// CFA colours of the 2x2 block in row-major order: 0 = R, 1 = G, 2 = B.
    pub cfa: [u32; 4],
    pub black: f32,
    pub white: f32,
    /// R, G, B white-balance gains.
    pub wb: [f32; 3],
    /// Row-major 3x3 camera(WB) -> linear Rec.709 matrix (identity when unknown).
    pub matrix: [f32; 9],
    /// Linear exposure gain applied after the colour matrix.
    pub exposure: f32,
    /// Pixel height / width ratio (anamorphic stretch); 1.0 = square pixels.
    pub stretch_y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    size: [u32; 2],
    cfa_shift: [u32; 2],
    cfa: [u32; 4],
    black: f32,
    inv_range: f32,
    exposure: f32,
    _pad: f32,
    wb: [f32; 4],
    m0: [f32; 4],
    m1: [f32; 4],
    m2: [f32; 4],
}

pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Creates a device; pass the target surface when presenting to a window so the adapter is compatible.
    pub fn new(surface: Option<&wgpu::Surface<'_>>) -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        Self::with_instance(instance, surface)
    }

    pub fn with_instance(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU adapter: {e}"))?;
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("cinefs-player"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: limits.max_texture_dimension_2d,
                ..wgpu::Limits::default()
            },
            ..Default::default()
        }))
        .map_err(|e| format!("GPU device: {e}"))?;
        Ok(Self { instance, adapter, device, queue })
    }
}

struct Resources {
    fmt: FrameFormat,
    bayer: wgpu::Texture,
    lin_view: wgpu::TextureView,
    debayer_bg: wgpu::BindGroup,
    display_bg: wgpu::BindGroup,
}

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    debayer_pipe: wgpu::RenderPipeline,
    debayer_bgl: wgpu::BindGroupLayout,
    display_pipe: wgpu::RenderPipeline,
    display_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    res: Option<Resources>,
}

impl Renderer {
    /// `target_format` must be a non-sRGB format (the shader applies the Rec.709 curve itself).
    pub fn new(gpu: &Gpu, target_format: wgpu::TextureFormat) -> Self {
        let device = gpu.device.clone();
        let queue = gpu.queue.clone();

        let debayer_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("debayer-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let display_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("display-bgl"),
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
            ],
        });

        let debayer_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("debayer"),
            source: wgpu::ShaderSource::Wgsl(include_str!("debayer.wgsl").into()),
        });
        let display_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("display"),
            source: wgpu::ShaderSource::Wgsl(include_str!("display.wgsl").into()),
        });

        let make_pipe = |label: &str,
                         module: &wgpu::ShaderModule,
                         bgl: &wgpu::BindGroupLayout,
                         fs: &str,
                         format: wgpu::TextureFormat| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let debayer_pipe = make_pipe("debayer", &debayer_mod, &debayer_bgl, "fs_debayer", wgpu::TextureFormat::Rgba16Float);
        let display_pipe = make_pipe("display", &display_mod, &display_bgl, "fs_display", target_format);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self { device, queue, debayer_pipe, debayer_bgl, display_pipe, display_bgl, sampler, params, res: None }
    }

    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// (Re)allocates textures and uniforms for a clip's frame format.
    pub fn set_format(&mut self, fmt: &FrameFormat) -> Result<(), String> {
        let max = self.max_texture_dimension();
        if fmt.width > max || fmt.height > max {
            return Err(format!("frame {}x{} exceeds the GPU texture limit {max}", fmt.width, fmt.height));
        }
        let size = wgpu::Extent3d { width: fmt.width, height: fmt.height, depth_or_array_layers: 1 };
        let bayer = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bayer"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R16Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let lin = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("linear"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let bayer_view = bayer.create_view(&Default::default());
        let lin_view = lin.create_view(&Default::default());
        let debayer_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("debayer-bg"),
            layout: &self.debayer_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&bayer_view) },
            ],
        });
        let display_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("display-bg"),
            layout: &self.display_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&lin_view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });

        let m = &fmt.matrix;
        let p = Params {
            size: [fmt.width, fmt.height],
            cfa_shift: [0, 0],
            cfa: fmt.cfa,
            black: fmt.black,
            inv_range: 1.0 / (fmt.white - fmt.black).max(1.0),
            exposure: fmt.exposure,
            _pad: 0.0,
            wb: [fmt.wb[0], fmt.wb[1], fmt.wb[2], 0.0],
            m0: [m[0], m[1], m[2], 0.0],
            m1: [m[3], m[4], m[5], 0.0],
            m2: [m[6], m[7], m[8], 0.0],
        };
        self.queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&p));
        self.res = Some(Resources { fmt: fmt.clone(), bayer, lin_view, debayer_bg, display_bg });
        Ok(())
    }

    /// Uploads one Bayer frame (`width * height` samples).
    pub fn upload(&self, samples: &[u16]) {
        let Some(r) = &self.res else { return };
        let (w, h) = (r.fmt.width, r.fmt.height);
        if samples.len() < (w * h) as usize {
            return;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &r.bayer,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&samples[..(w * h) as usize]),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 2), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Aspect-fitted viewport `(x, y, w, h)` inside a `tw x th` target.
    pub fn fit_viewport(&self, tw: u32, th: u32) -> (f32, f32, f32, f32) {
        let Some(r) = &self.res else { return (0.0, 0.0, tw as f32, th as f32) };
        let stretch = if r.fmt.stretch_y.is_finite() && r.fmt.stretch_y > 0.0 { r.fmt.stretch_y } else { 1.0 };
        let aspect = r.fmt.width as f64 / (r.fmt.height as f64 * stretch);
        let (tw_f, th_f) = (tw as f64, th as f64);
        let (w, h) = if tw_f / th_f > aspect { (th_f * aspect, th_f) } else { (tw_f, tw_f / aspect) };
        (((tw_f - w) / 2.0).floor() as f32, ((th_f - h) / 2.0).floor() as f32, w.floor() as f32, h.floor() as f32)
    }

    /// Debayers the last uploaded frame and draws it into `target` (a `tw x th` colour attachment).
    pub fn render(&self, target: &wgpu::TextureView, tw: u32, th: u32) {
        let Some(r) = &self.res else { return };
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("debayer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &r.lin_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.debayer_pipe);
            pass.set_bind_group(0, &r.debayer_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("display"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let (x, y, w, h) = self.fit_viewport(tw, th);
            pass.set_viewport(x, y, w.max(1.0), h.max(1.0), 0.0, 1.0);
            pass.set_pipeline(&self.display_pipe);
            pass.set_bind_group(0, &r.display_bg, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(enc.finish()));
    }

    /// Renders into an offscreen `Rgba8Unorm` target and reads it back (tests / screenshots).
    /// The renderer must have been created with `TextureFormat::Rgba8Unorm`.
    pub fn render_to_rgba8(&self, tw: u32, th: u32) -> Vec<u8> {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        self.render(&view, tw, th);

        let row = (tw * 4).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (row * th) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(th) },
            },
            wgpu::Extent3d { width: tw, height: th, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(enc.finish()));
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range().expect("map readback");
        let mut out = vec![0u8; (tw * th * 4) as usize];
        for y in 0..th as usize {
            let s = y * row as usize;
            let d = y * tw as usize * 4;
            out[d..d + tw as usize * 4].copy_from_slice(&data[s..s + tw as usize * 4]);
        }
        drop(data);
        buf.unmap();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec709_oetf(v: f32) -> f32 {
        let c = v.clamp(0.0, 1.0);
        if c < 0.018 { 4.5 * c } else { 1.099 * c.powf(0.45) - 0.099 }
    }

    /// Smooth RGB test scene, mosaicked as RGGB, must come back (almost) unchanged after the GPU
    /// debayer + display chain. Skips when the machine has no GPU adapter.
    #[test]
    fn gpu_debayer_reproduces_smooth_scene() {
        let Ok(gpu) = Gpu::new(None) else {
            eprintln!("no GPU adapter, skipping");
            return;
        };
        let (w, h) = (64u32, 48u32);
        let scene = |x: u32, y: u32| -> [f32; 3] {
            let fx = x as f32 / (w - 1) as f32;
            let fy = y as f32 / (h - 1) as f32;
            [0.15 + 0.5 * fx, 0.2 + 0.3 * fy + 0.1 * fx, 0.7 - 0.4 * fx * fy]
        };
        let cfa = [0u32, 1, 1, 2]; // RGGB
        let (black, white) = (64.0f32, 4095.0f32);
        let mut mosaic = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let c = cfa[((y & 1) * 2 + (x & 1)) as usize] as usize;
                mosaic[(y * w + x) as usize] = (black + scene(x, y)[c] * (white - black)).round() as u16;
            }
        }
        let fmt = FrameFormat {
            width: w,
            height: h,
            cfa,
            black,
            white,
            wb: [1.0, 1.0, 1.0],
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            exposure: 1.0,
            stretch_y: 1.0,
        };
        let mut r = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
        r.set_format(&fmt).unwrap();
        r.upload(&mosaic);
        let out = r.render_to_rgba8(w, h);

        let mut worst = 0.0f32;
        for y in 4..h - 4 {
            for x in 4..w - 4 {
                let exp = scene(x, y);
                for c in 0..3 {
                    let got = out[((y * w + x) * 4) as usize + c] as f32 / 255.0;
                    worst = worst.max((got - rec709_oetf(exp[c])).abs());
                }
            }
        }
        assert!(worst < 0.02, "worst channel error {worst}");
    }

    #[test]
    fn viewport_letterboxes_with_stretch() {
        let Ok(gpu) = Gpu::new(None) else { return };
        let mut r = Renderer::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
        let fmt = FrameFormat {
            width: 1408,
            height: 846,
            cfa: [0, 1, 1, 2],
            black: 0.0,
            white: 1.0,
            wb: [1.0; 3],
            matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            exposure: 1.0,
            stretch_y: 3.0,
        };
        r.set_format(&fmt).unwrap();
        // 1408 x (846*3) is portrait: in a 1000x500 window it must be pillarboxed.
        let (x, _, w, h) = r.fit_viewport(1000, 500);
        assert!(x > 0.0 && (h - 500.0).abs() <= 1.0 && w < 1000.0);
    }
}
