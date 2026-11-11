//! The existing capture, blurred once and shared by all text backgrounds. Only this module owns
//! GPU resources. The watcher publishes pixels; the UI thread uploads, draws and presents them.

use crate::{Patch, Region, config};
use std::cell::RefCell;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{Interface, Result, s};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const WM_SOURCE: u32 = WM_APP + 11;
static ENABLED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);
static MAIL: Mutex<Mail> = Mutex::new(Mail { source: Source { job: 0, revision: 0, w: 0, h: 0, pixels: Vec::new() }, pending: false, revision: 0 });

#[derive(Default)]
struct Source {
    job: u64,
    revision: u64,
    w: i32,
    h: i32,
    pixels: Vec<u8>,
}

struct Mail {
    source: Source,
    pending: bool,
    revision: u64,
}

#[derive(Default)]
struct State {
    source: Source,
    renderer: Option<Renderer>,
    text_dirty: bool,
    failed: bool,
    reported: bool,
    retry_after: Option<u64>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static BYTECODE: RefCell<Option<Vec<Vec<u8>>>> = const { RefCell::new(None) };
}

/// No capture happens here. In particular a stationary one-shot result publishes its held frame.
pub fn publish(hwnd: usize, job: u64, region: Region, pixels: &[u8], changed: bool) {
    if !ENABLED.load(Ordering::Relaxed) || pixels.is_empty() {
        return;
    }
    if !REQUESTED.swap(false, Ordering::Relaxed) && !changed {
        return;
    }
    let mut mail = MAIL.lock().unwrap();
    if !ENABLED.load(Ordering::Relaxed) { return; }
    mail.revision = mail.revision.wrapping_add(1);
    let revision = mail.revision;
    let source = &mut mail.source;
    source.job = job;
    source.revision = revision;
    source.w = region.w;
    source.h = region.h;
    source.pixels.clear();
    source.pixels.extend_from_slice(pixels);
    if !mail.pending {
        mail.pending = unsafe { PostMessageW(hwnd as _, WM_SOURCE, 0, 0) } != 0;
        if !mail.pending { REQUESTED.store(true, Ordering::Relaxed); }
    }
}

pub fn receive(job: u64) {
    STATE.with_borrow_mut(|state| {
        let mut mail = MAIL.lock().unwrap();
        if mail.pending && mail.source.job == job {
            // Two reusable CPU buffers; no frame queue and no GPU call while this lock is held.
            std::mem::swap(&mut state.source, &mut mail.source);
        }
        mail.pending = false;
    });
}

pub fn configure() {
    let enabled = config::get().blur_background;
    if ENABLED.swap(enabled, Ordering::Relaxed) != enabled {
        STATE.with_borrow_mut(|state| *state = State::default());
        let mut mail = MAIL.lock().unwrap();
        mail.source = Source::default();
        mail.pending = false;
        REQUESTED.store(enabled, Ordering::Relaxed);
    }
    text_changed();
}

pub fn text_changed() {
    STATE.with_borrow_mut(|state| state.text_dirty = true);
}

pub fn active(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    STATE.with_borrow(|state| state.renderer.as_ref().is_some_and(|r| r.targets.iter().any(|t| t.0 == hwnd as usize)))
}

pub fn release(hwnd: windows_sys::Win32::Foundation::HWND) {
    STATE.with_borrow_mut(|state| {
        if let Some(renderer) = &mut state.renderer {
            renderer.detach(hwnd as usize);
        }
    });
}

/// False leaves the existing GDI renderer in charge. The off-screen copy attaches to the same
/// swap chain: it never uploads a source, rasterizes text, or runs either blur pass again.
pub fn paint(hwnd: windows_sys::Win32::Foundation::HWND, main: windows_sys::Win32::Foundation::HWND, job: u64, region: Region, size: (i32, i32), patches: &[Patch]) -> bool {
    if !ENABLED.load(Ordering::Relaxed) || size.0 <= 0 || size.1 <= 0 {
        return false;
    }
    STATE.with_borrow_mut(|state| {
        let source = &state.source;
        if source.job != job || job == 0 || (source.w, source.h) != (region.w, region.h) || source.pixels.len() != source.w as usize * source.h as usize * 4 || state.failed || state.retry_after == Some(source.revision) {
            return false;
        }
        if state.renderer.is_none() {
            match Renderer::new() {
                Ok(renderer) => { state.renderer = Some(renderer); state.text_dirty = true; }
                Err(e) => {
                    crate::log(&format!("background blur unavailable: {e}"));
                    state.failed = true;
                    return false;
                }
            }
        }
        let renderer = state.renderer.as_mut().unwrap();
        let result = if hwnd == main {
            renderer.draw(source, size, patches, state.text_dirty).and_then(|()| renderer.attach(hwnd as usize))
        } else if renderer.job == job {
            renderer.attach(hwnd as usize)
        } else {
            return false;
        };
        match result {
            Ok(()) => { if hwnd == main { state.text_dirty = false; } true }
            Err(e) => {
                if !state.reported {
                    crate::log(&format!("background blur failed: {e}"));
                    state.reported = true;
                }
                let lost = e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET;
                state.failed = !lost;
                state.retry_after = Some(source.revision);
                state.renderer = None;
                false
            }
        }
    })
}

fn bytecode() -> Result<Vec<Vec<u8>>> {
    BYTECODE.with_borrow_mut(|cached| {
        if let Some(code) = cached { return Ok(code.clone()); }
        let mut code = Vec::new();
        let common = include_str!("shaders/pixel.hlsl");
        for (source, target) in [
            (include_str!("shaders/quad.hlsl").to_string(), s!("vs_4_0")),
            (format!("{common}\n{}", include_str!("shaders/horizontal.hlsl")), s!("ps_4_0")),
            (format!("{common}\n{}", include_str!("shaders/vertical.hlsl")), s!("ps_4_0")),
            (format!("{common}\n{}", include_str!("shaders/background.hlsl")), s!("ps_4_0")),
            (format!("{common}\n{}", include_str!("shaders/text.hlsl")), s!("ps_4_0")),
        ] {
            unsafe {
                let (mut blob, mut errors) = (None, None);
                let result = D3DCompile(source.as_ptr().cast(), source.len(), s!("sonoba"), None, None, s!("main"), target, D3DCOMPILE_OPTIMIZATION_LEVEL3, 0, &mut blob, Some(&mut errors));
                if let Some(errors) = errors {
                    let message = std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize());
                    crate::log(&String::from_utf8_lossy(message));
                }
                result?;
                let blob = blob.unwrap();
                code.push(std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()).to_vec());
            }
        }
        *cached = Some(code.clone());
        Ok(code)
    })
}

struct Texture {
    image: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    target: Option<ID3D11RenderTargetView>,
}

impl Texture {
    fn new(device: &ID3D11Device, (w, h): (i32, i32), render: bool) -> Result<Self> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32, Height: h as u32, MipLevels: 1, ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | if render { D3D11_BIND_RENDER_TARGET.0 } else { 0 }) as u32,
                ..Default::default()
            };
            let (mut image, mut view, mut target) = (None, None, None);
            device.CreateTexture2D(&desc, None, Some(&mut image))?;
            let image = image.unwrap();
            device.CreateShaderResourceView(&image, None, Some(&mut view))?;
            if render { device.CreateRenderTargetView(&image, None, Some(&mut target))?; }
            Ok(Self { image, view: view.unwrap(), target })
        }
    }
}

struct Picture {
    size: (i32, i32),
    source: Texture,
    horizontal: Texture,
    blurred: Texture,
}

struct Presentation {
    size: (i32, i32),
    chain: IDXGISwapChain1,
    target: ID3D11RenderTargetView,
    text: Texture,
    color: Bitmap,
    coverage: Bitmap,
}

// Background pixels can change on every frame while the letters stay the same. Keep only the
// fields that affect text rasterization, so an updated backdrop does not redraw either text DIB.
struct TextKey {
    geometry: [i32; 11],
    colors: [u32; 2],
    bold: bool,
    text: String,
}

impl TextKey {
    fn geometry(p: &Patch) -> [i32; 11] {
        let (w, h) = p.backdrop.as_ref().map_or((p.w, p.h), |b| (b.w, b.h));
        [p.x, p.y, p.w, p.h, p.px, p.narrow, p.text_h, w, h, p.backdrop.is_some() as i32, p.source.is_empty() as i32]
    }

    fn colors(p: &Patch) -> [u32; 2] {
        [p.fg, if p.source.is_empty() { p.bg } else { 0 }]
    }

    fn matches(&self, p: &Patch) -> bool {
        self.geometry == Self::geometry(p) && self.colors == Self::colors(p) && self.bold == p.bold && self.text == p.text
    }

    fn of(p: &Patch) -> Self {
        Self { geometry: Self::geometry(p), colors: Self::colors(p), bold: p.bold, text: p.text.clone() }
    }
}

// Retaining the Arc makes identity comparisons safe even after patches are replaced.
struct BackgroundKey {
    area: [i32; 4],
    bg: u32,
    backdrop: Option<std::sync::Arc<crate::patch::Backdrop>>,
}

impl BackgroundKey {
    fn of(p: &Patch) -> Self {
        let (w, h) = p.backdrop.as_ref().map_or((p.w, p.h), |b| (b.w, b.h));
        Self { area: [p.x, p.y, w, h], bg: p.bg, backdrop: p.backdrop.clone() }
    }
    fn matches(&self, p: &Patch) -> bool {
        let other = Self::of(p);
        self.area == other.area && match (&self.backdrop, &other.backdrop) {
            (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
            (None, None) => self.bg == other.bg,
            _ => false,
        }
    }
}

#[derive(Default)]
struct Cleaned {
    pixels: Vec<u8>,
    keys: Vec<BackgroundKey>,
    frame: Option<(u64, u64, i32, i32)>,
}

impl Cleaned {
    fn refresh(&mut self, source: &Source, patches: &[Patch]) -> bool {
        let frame = (source.job, source.revision, source.w, source.h);
        let translated = || patches.iter().filter(|p| !p.source.is_empty());
        if self.frame == Some(frame) && self.keys.len() == translated().count()
            && self.keys.iter().zip(translated()).all(|(key, p)| key.matches(p)) {
            return false;
        }
        self.pixels.clear();
        self.pixels.extend_from_slice(&source.pixels);
        self.keys.clear();
        for p in translated() {
            let key = BackgroundKey::of(p);
            let [x, y, w, h] = key.area;
            let x0 = x.clamp(0, source.w);
            let y0 = y.clamp(0, source.h);
            let x1 = x.saturating_add(w).clamp(0, source.w);
            let y1 = y.saturating_add(h).clamp(0, source.h);
            if x1 > x0 && y1 > y0 {
                for row in y0..y1 {
                    let start = (row as usize * source.w as usize + x0 as usize) * 4;
                    let len = (x1 - x0) as usize * 4;
                    let dest = &mut self.pixels[start..start + len];
                    if let Some(b) = &key.backdrop {
                        let offset = ((row - y) as usize * b.w as usize + (x0 - x) as usize) * 4;
                        dest.copy_from_slice(&b.bgra[offset..offset + len]);
                    } else {
                        let color = [(p.bg >> 16) as u8, (p.bg >> 8) as u8, p.bg as u8, 255];
                        for pixel in dest.chunks_exact_mut(4) { pixel.copy_from_slice(&color); }
                    }
                }
            }
            self.keys.push(key);
        }
        self.frame = Some(frame);
        true
    }
}

// The fade starts outside the removal rectangle; clipping must not move it inward.
fn background_area(p: &Patch) -> [f32; 4] {
    let [x, y, w, h] = BackgroundKey::of(p).area;
    [x as f32 - 3.0, y as f32 - 3.0, w as f32 + 6.0, h as f32 + 6.0]
}

struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    composition: IDCompositionDevice,
    opacity: IDCompositionEffectGroup,
    vertex: ID3D11VertexShader,
    horizontal: ID3D11PixelShader,
    vertical: ID3D11PixelShader,
    background: ID3D11PixelShader,
    text: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    rasterizer: ID3D11RasterizerState,
    blend: ID3D11BlendState,
    picture: Option<Picture>,
    presentation: Option<Presentation>,
    targets: Vec<(usize, IDCompositionTarget, IDCompositionVisual)>,
    job: u64,
    revision: u64,
    text_keys: Vec<TextKey>,
    cleaned: Cleaned,
}

impl Renderer {
    fn new() -> Result<Self> {
        unsafe {
            let code = bytecode()?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0]), D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))?;
            let (device, context) = (device.unwrap(), context.unwrap());
            let dxgi: IDXGIDevice = device.cast()?;
            let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let opacity = composition.CreateEffectGroup()?;
            let mut vertex = None;
            device.CreateVertexShader(&code[0], None, Some(&mut vertex))?;
            let pixel = |code: &[u8]| -> Result<ID3D11PixelShader> {
                let mut shader = None;
                device.CreatePixelShader(code, None, Some(&mut shader))?;
                Ok(shader.unwrap())
            };
            let (horizontal, vertical, background, text) = (pixel(&code[1])?, pixel(&code[2])?, pixel(&code[3])?, pixel(&code[4])?);
            let mut constants = None;
            device.CreateBuffer(&D3D11_BUFFER_DESC { ByteWidth: 32, Usage: D3D11_USAGE_DEFAULT, BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32, ..Default::default() }, None, Some(&mut constants))?;
            let mut sampler = None;
            device.CreateSamplerState(&D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT, AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP, AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxAnisotropy: 1, ComparisonFunc: D3D11_COMPARISON_NEVER, MaxLOD: f32::MAX, ..Default::default()
            }, Some(&mut sampler))?;
            let mut rasterizer = None;
            device.CreateRasterizerState(&D3D11_RASTERIZER_DESC { FillMode: D3D11_FILL_SOLID, CullMode: D3D11_CULL_NONE, DepthClipEnable: true.into(), ..Default::default() }, Some(&mut rasterizer))?;
            let mut blend = None;
            let mut blending = D3D11_BLEND_DESC::default();
            blending.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(), SrcBlend: D3D11_BLEND_ONE, DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD, SrcBlendAlpha: D3D11_BLEND_ONE, DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD, RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            device.CreateBlendState(&blending, Some(&mut blend))?;
            Ok(Self { device, context, composition, opacity, vertex: vertex.unwrap(), horizontal, vertical, background, text, constants: constants.unwrap(), sampler: sampler.unwrap(), rasterizer: rasterizer.unwrap(), blend: blend.unwrap(), picture: None, presentation: None, targets: Vec::new(), job: 0, revision: 0, text_keys: Vec::new(), cleaned: Cleaned::default() })
        }
    }

    fn attach(&mut self, hwnd: usize) -> Result<()> {
        let Some(presentation) = &self.presentation else { return Ok(()); };
        unsafe {
            if !self.targets.iter().any(|t| t.0 == hwnd) {
                let target = self.composition.CreateTargetForHwnd(HWND(hwnd as _), true)?;
                let visual = self.composition.CreateVisual()?;
                visual.SetContent(&presentation.chain)?;
                visual.SetEffect(&self.opacity)?;
                target.SetRoot(&visual)?;
                self.targets.push((hwnd, target, visual));
            }
            let opacity = config::get().opacity as f32 / 100.0;
            self.opacity.SetOpacity2(opacity)?;
            self.composition.Commit()
        }
    }

    fn detach(&mut self, hwnd: usize) {
        if let Some(index) = self.targets.iter().position(|t| t.0 == hwnd) {
            let (_, target, _) = self.targets.remove(index);
            unsafe { let _ = target.SetRoot(None); let _ = self.composition.Commit(); }
        }
    }

    fn resize(&mut self, source: (i32, i32), size: (i32, i32)) -> Result<bool> {
        unsafe {
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(None, None);
            if self.picture.as_ref().is_none_or(|p| p.size != source) {
                self.picture = Some(Picture { size: source, source: Texture::new(&self.device, source, false)?, horizontal: Texture::new(&self.device, source, true)?, blurred: Texture::new(&self.device, source, true)? });
                self.revision = 0;
            }
            if self.presentation.as_ref().is_some_and(|p| p.size == size) { return Ok(false); }
            let dxgi: IDXGIDevice = self.device.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0 as u32, Height: size.1 as u32, Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2, Scaling: DXGI_SCALING_STRETCH, SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED, ..Default::default()
            };
            let chain = factory.CreateSwapChainForComposition(&self.device, &desc, None)?;
            let buffer: ID3D11Texture2D = chain.GetBuffer(0)?;
            let mut target = None;
            self.device.CreateRenderTargetView(&buffer, None, Some(&mut target))?;
            let presentation = Presentation { size, chain, target: target.unwrap(), text: Texture::new(&self.device, size, false)?, color: Bitmap::new(size)?, coverage: Bitmap::new(size)? };
            for (_, _, visual) in &self.targets { visual.SetContent(&presentation.chain)?; }
            self.presentation = Some(presentation);
            Ok(true)
        }
    }

    fn quad(&self, shader: &ID3D11PixelShader, input: &Texture, target: &ID3D11RenderTargetView, canvas: (i32, i32), image: (i32, i32), area: [f32; 4]) {
        unsafe {
            let data = [area[0], area[1], area[2], area[3], canvas.0 as f32, canvas.1 as f32, image.0 as f32, image.1 as f32];
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context.UpdateSubresource(&self.constants, 0, None, data.as_ptr().cast(), 0, 0);
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: canvas.0 as f32, Height: canvas.1 as f32, MaxDepth: 1.0, ..Default::default() }]));
            self.context.PSSetShaderResources(0, Some(&[Some(input.view.clone())]));
            self.context.PSSetShader(shader, None);
            self.context.Draw(4, 0);
        }
    }

    fn draw(&mut self, source: &Source, size: (i32, i32), patches: &[Patch], text_dirty: bool) -> Result<()> {
        let resized = self.resize((source.w, source.h), size)?;
        let background_changed = self.cleaned.refresh(source, patches);
        let same_text = self.text_keys.len() == patches.len() && self.text_keys.iter().zip(patches).all(|(key, p)| key.matches(p));
        unsafe {
            if resized || text_dirty || !same_text {
                let p = self.presentation.as_mut().unwrap();
                p.color.clear();
                p.coverage.clear();
                crate::draw_text_layer(p.color.dc, patches, false);
                crate::draw_text_layer(p.coverage.dc, patches, true);
                GdiFlush();
                let pixels = p.color.pixels();
                for (color, coverage) in pixels.chunks_exact_mut(4).zip(p.coverage.pixels().chunks_exact(4)) {
                    color[3] = coverage[0]; // grayscale coverage; GDI itself does not write alpha
                }
                self.context.UpdateSubresource(&p.text.image, 0, None, pixels.as_ptr().cast(), size.0 as u32 * 4, 0);
                self.text_keys.clear();
                self.text_keys.extend(patches.iter().map(TextKey::of));
            }
            let p = self.picture.as_ref().unwrap();
            let out = self.presentation.as_ref().unwrap();
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.context.VSSetShader(&self.vertex, None);
            self.context.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            self.context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.RSSetState(&self.rasterizer);
            self.context.OMSetBlendState(None, None, u32::MAX);
            if self.revision != source.revision || background_changed {
                self.context.UpdateSubresource(&p.source.image, 0, None, self.cleaned.pixels.as_ptr().cast(), source.w as u32 * 4, 0);
                let area = [0.0, 0.0, source.w as f32, source.h as f32];
                // One context, in order. Unbinding the first target is not a wait or a readback.
                self.quad(&self.horizontal, &p.source, p.horizontal.target.as_ref().unwrap(), p.size, p.size, area);
                self.quad(&self.vertical, &p.horizontal, p.blurred.target.as_ref().unwrap(), p.size, p.size, area);
                self.revision = source.revision;
            }
            self.context.ClearRenderTargetView(&out.target, &[0.0; 4]);
            self.context.OMSetBlendState(&self.blend, None, u32::MAX);
            for patch in patches.iter().filter(|p| !p.source.is_empty()) {
                self.quad(&self.background, &p.blurred, &out.target, size, p.size, background_area(patch));
            }
            self.quad(&self.text, &out.text, &out.target, size, size, [0.0, 0.0, size.0 as f32, size.1 as f32]);
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(None, None);
            out.chain.Present(1, DXGI_PRESENT(0)).ok()?;
            self.job = source.job;
            Ok(())
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            for (_, target, _) in &self.targets { let _ = target.SetRoot(None); }
            let _ = self.composition.Commit();
            self.context.ClearState();
        }
    }
}

/// CPU-only text rasterization. These bitmaps never contain a blurred background.
struct Bitmap {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    size: (i32, i32),
}

impl Bitmap {
    fn new(size: (i32, i32)) -> Result<Self> {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() { return Err(windows::core::Error::from_thread()); }
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = size.0;
            info.bmiHeader.biHeight = -size.1;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            let mut bits = null_mut();
            let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                let error = windows::core::Error::from_thread();
                if !bitmap.is_null() { DeleteObject(bitmap); }
                DeleteDC(dc);
                return Err(error);
            }
            let old = SelectObject(dc, bitmap);
            Ok(Self { dc, bitmap, old, bits: bits.cast(), size })
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.size.0 as usize * self.size.1 as usize * 4) }
    }

    fn clear(&mut self) {
        unsafe { GdiFlush(); }
        self.pixels().fill(0);
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe { SelectObject(self.dc, self.old); DeleteObject(self.bitmap); DeleteDC(self.dc); }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_shaders_compile() {
        let code = bytecode().expect("embedded blur shaders must compile");
        assert_eq!(code.len(), 5);
        assert!(code.iter().all(|shader| !shader.is_empty()));
    }

    #[test]
    fn cleaned_inputs_and_frozen_frame_invalidation() {
        let source = Source { job: 1, revision: 1, w: 4, h: 4, pixels: vec![9; 64] };
        let mut p = Patch::status("translation", 2);
        p.source = "original".into();
        (p.x, p.y, p.w, p.h, p.bg) = (-1, 1, 3, 2, 0x123456);
        let mut clean = Cleaned::default();
        assert!(clean.refresh(&source, &[p.clone()]));
        assert_eq!(&clean.pixels[16..24], &[0x12, 0x34, 0x56, 255, 0x12, 0x34, 0x56, 255]);
        assert_eq!(&clean.pixels[24..32], &[9; 8]);
        assert_eq!(source.pixels, vec![9; 64]);
        assert!(!clean.refresh(&source, &[p.clone()]));
        p.text = "different translation".into();
        assert!(!clean.refresh(&source, &[p.clone()]));
        p.bg = 0;
        assert!(clean.refresh(&source, &[p.clone()]));
        p.x = 1;
        assert!(clean.refresh(&source, &[p.clone()]));
        assert_eq!(&clean.pixels[16..20], &[9; 4]);
        assert_eq!(background_area(&p), [-2.0, -2.0, 9.0, 8.0]);
        p.source.clear();
        assert!(clean.refresh(&source, &[p]));
        assert_eq!(clean.pixels, source.pixels);
    }

    #[test]
    fn cached_backdrop_pixels_and_replacement() {
        let source = Source { job: 1, revision: 1, w: 8, h: 8, pixels: vec![80; 256] };
        let mut p = Patch::status("translation", 4);
        (p.x, p.y, p.w, p.h) = (1, 1, 4, 4);
        p.source = "original".into();
        p = p.keep_picture(&source.pixels, 8, 8);
        let mut clean = Cleaned::default();
        assert!(clean.refresh(&source, &[p.clone()]));
        let b = p.backdrop.as_ref().unwrap();
        for row in 0..b.h as usize {
            assert_eq!(&clean.pixels[((row + 1)*8+1)*4..((row+1)*8+1+b.w as usize)*4], &b.bgra[row*b.w as usize*4..(row+1)*b.w as usize*4]);
        }
        assert!(!clean.refresh(&source, &[p.clone()]));
        p = p.keep_picture(&source.pixels, 8, 8);
        assert!(clean.refresh(&source, &[p]));
        assert!(clean.refresh(&source, &[]));
        assert_eq!(clean.pixels, source.pixels);
    }
}
