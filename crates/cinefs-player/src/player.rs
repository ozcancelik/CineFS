//! A player bound to one clip and one native window surface.

use crate::audio::AudioOutput;
use crate::engine::{Engine, FrameProvider, Stats};
use crate::gpu::{FrameFormat, Gpu, Renderer};
use cinefs_core::vfs::MountedClip;
use std::sync::Arc;

/// Decodes frames of a mounted clip with focus-pixel correction (the stretch is done by the viewport).
pub struct ClipProvider {
    clip: Arc<MountedClip>,
}

impl ClipProvider {
    pub fn new(clip: Arc<MountedClip>) -> Self {
        Self { clip }
    }
}

impl FrameProvider for ClipProvider {
    fn frame_count(&self) -> usize {
        self.clip.clip.frames.len()
    }

    fn fps(&self) -> f64 {
        self.clip.clip.fps_f64()
    }

    fn decode(&self, index: usize) -> Result<Vec<u16>, String> {
        let c = &self.clip.clip;
        let rt = self.clip.runtime();
        let (w, h) = (c.width as usize, c.height as usize);
        let mut px = cinefs_core::focus_probe::decode_frame_u16(c, index).map_err(|e| e.to_string())?;
        if let Some(fix) = &rt.focus_fix {
            fix.apply_u16_ex(&mut px, w, h, rt.dual_iso.is_some());
        }
        if let Some(bp) = &rt.bad_pixels {
            bp.apply_u16_ex(&mut px, w, h, rt.dual_iso.is_some());
        }
        if let Some(diso) = &rt.dual_iso {
            diso.apply_u16(&mut px, w, h);
        }
        if let Some(st) = &rt.stripes {
            st.apply_u16(&mut px, w, h);
        }
        Ok(px)
    }
}

/// GPU frame description for `clip` (levels, CFA, as-shot white balance, camera -> Rec.709).
pub fn format_for_clip(clip: &MountedClip) -> FrameFormat {
    let c = &clip.clip;
    let rt = clip.runtime();
    let meta = c.dynamic_metas.first();
    let gains = meta.map(|m| [m.wb_gain_r, m.wb_gain_g, m.wb_gain_b]).unwrap_or([1.0; 3]);
    let g = gains[1].max(0.001);
    let wb = [gains[0] / g, 1.0, gains[2] / g];
    let cfa = c.cfa_pattern.dng_bytes().map(|v| v as u32);
    let matrix = cinefs_core::thumbnail::camera_to_rec709(&c.color_matrix1, c.forward_matrix1.as_ref(), gains)
        .unwrap_or([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    let (black, white) = if rt.dual_iso.is_some() {
        (0.0f32, 65535.0f32)
    } else {
        (c.levels.black as f32, c.levels.white as f32)
    };
    FrameFormat {
        width: c.width,
        height: c.height,
        cfa,
        black,
        white,
        wb,
        matrix,
        exposure: 1.0,
        stretch_y: rt.stretch_y,
    }
}

pub struct Player {
    gpu: Gpu,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    pub engine: Engine,
    /// Declared after `engine` so the engine (which holds the audio clock) is dropped first.
    audio: Option<AudioOutput>,
    dirty: bool,
}

impl Player {
    /// # Safety
    /// `target` must refer to a live native view/window that outlives the returned player.
    pub unsafe fn new(
        clip: Arc<MountedClip>,
        target: wgpu::SurfaceTargetUnsafe,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface_unsafe(target).map_err(|e| format!("surface: {e}"))?;
        let gpu = Gpu::with_instance(instance, Some(&surface))?;

        let mut config = surface
            .get_default_config(&gpu.adapter, width.max(1), height.max(1))
            .ok_or("surface is not supported by the GPU adapter")?;
        // The shader applies the Rec.709 curve itself, so the surface must not be an sRGB format.
        let caps = surface.get_capabilities(&gpu.adapter);
        if let Some(f) = caps.formats.iter().copied().find(|f| !f.is_srgb()) {
            config.format = f;
        }
        config.present_mode = wgpu::PresentMode::Fifo;
        surface.configure(&gpu.device, &config);

        let mut renderer = Renderer::new(&gpu, config.format);
        let fmt = format_for_clip(&clip);
        renderer.set_format(&fmt)?;

        let frame_bytes = (fmt.width * fmt.height * 2) as usize;
        let fps = clip.clip.fps_f64();
        let (threads, window) = Engine::auto_config(frame_bytes, fps);
        crate::log_info(&format!(
            "player: '{}' {}x{} @ {:.2} fps, {} decode threads, {}-frame read-ahead, GPU {:?}",
            clip.folder_name,
            fmt.width,
            fmt.height,
            fps,
            threads,
            window,
            gpu.adapter.get_info().name
        ));
        let audio = match AudioOutput::for_clip(&clip) {
            Ok(a) => a,
            Err(e) => {
                // A missing / busy output device must not stop the picture: play silently.
                crate::log_warn(&format!("player: no audio output ({e}); using the wall clock"));
                None
            }
        };
        let mut engine = Engine::new(Arc::new(ClipProvider::new(clip)), threads, window);
        if let Some(a) = &audio {
            engine.set_audio(a.core.clone());
        }
        Ok(Self { gpu, surface, config, renderer, engine, audio, dirty: true })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if w == self.config.width && h == self.config.height {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.gpu.device, &self.config);
        self.dirty = true;
    }

    /// Call at display rate. Uploads and draws when a new frame is due (or after a resize).
    /// Returns `true` if something was drawn.
    pub fn tick(&mut self) -> bool {
        if let Some(a) = &mut self.audio {
            a.sync_device();
        }
        if let Some((_, frame)) = self.engine.next_frame() {
            self.renderer.upload(&frame);
            self.dirty = true;
        }
        if !self.dirty {
            return false;
        }
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
                let view = t.texture.create_view(&Default::default());
                self.renderer.render(&view, self.config.width, self.config.height);
                self.gpu.queue.present(t);
                self.dirty = false;
                true
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                false
            }
            _ => false, // occluded / timeout: try again next tick
        }
    }

    /// Forces the next `tick` to draw (after the window was uncovered / restored).
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// Output volume 0..1 (0 = muted; the audio clock keeps running).
    pub fn set_volume(&self, v: f32) {
        if let Some(a) = &self.audio {
            a.core.set_volume(v);
        }
    }

    pub fn stats(&self) -> Stats {
        self.engine.stats()
    }
}
