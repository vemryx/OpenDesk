//! Captura com DXGI Desktop Duplication.
//!
//! O Windows compõe a área de trabalho na GPU; a duplicação entrega a textura
//! já composta só quando algo mudou, o que dá captura barata e em sincronia
//! com a atualização do monitor. A textura é copiada para uma textura de
//! "staging" e lida pela CPU, porque o codec atual roda na CPU.
//!
//! Limitações conhecidas: monitores rotacionados não são corrigidos, e a área
//! de trabalho segura (UAC, tela de logon) só pode ser capturada por um
//! processo SYSTEM, o que exige o serviço previsto no roteiro.

use crate::capture::{DisplayInfo, Frame};
use anyhow::{Context, Result, anyhow, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

struct Output {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
    info: DisplayInfo,
}

fn enumerate() -> Result<Vec<Output>> {
    let mut found = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = output.GetDesc().context("IDXGIOutput::GetDesc")?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let rect = desc.DesktopCoordinates;
                let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
                found.push(Output {
                    adapter: adapter.clone(),
                    output,
                    info: DisplayInfo {
                        name: String::from_utf16_lossy(&desc.DeviceName[..len]),
                        x: rect.left,
                        y: rect.top,
                        width: (rect.right - rect.left) as u32,
                        height: (rect.bottom - rect.top) as u32,
                        primary: rect.left == 0 && rect.top == 0,
                    },
                });
            }
        }
    }
    // O monitor principal primeiro, para que o índice 0 seja sempre ele.
    found.sort_by_key(|o| !o.info.primary);
    Ok(found)
}

/// Monitores ligados à área de trabalho, o principal primeiro.
pub fn displays() -> Result<Vec<DisplayInfo>> {
    Ok(enumerate()?.into_iter().map(|o| o.info).collect())
}

pub struct Capturer {
    display_index: usize,
    info: DisplayInfo,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    buffer: Vec<u8>,
    size: (usize, usize),
    retry_after: Instant,
}

impl Capturer {
    /// Começa a capturar o monitor `display` (índice em [`displays`]).
    pub fn new(display: usize) -> Result<Self> {
        let (device, context, duplication, info) = open(display)?;
        Ok(Self {
            display_index: display,
            info,
            device,
            context,
            duplication: Some(duplication),
            staging: None,
            buffer: Vec::new(),
            size: (0, 0),
            retry_after: Instant::now(),
        })
    }

    pub fn display(&self) -> &DisplayInfo {
        &self.info
    }

    /// Espera até `timeout_ms` por uma mudança na tela. `Ok(None)` significa
    /// que nada mudou (ou que a captura está temporariamente indisponível,
    /// como durante um prompt do UAC).
    pub fn frame(&mut self, timeout_ms: u32) -> Result<Option<Frame<'_>>> {
        if self.duplication.is_none() {
            // A duplicação foi perdida (troca de resolução, UAC, tela cheia
            // exclusiva). Tenta recriar, sem martelar o sistema.
            let now = Instant::now();
            if now < self.retry_after {
                std::thread::sleep((self.retry_after - now).min(Duration::from_millis(timeout_ms as u64)));
                return Ok(None);
            }
            match open(self.display_index) {
                Ok((device, context, duplication, info)) => {
                    self.device = device;
                    self.context = context;
                    self.duplication = Some(duplication);
                    self.info = info;
                    self.staging = None;
                }
                Err(err) => {
                    tracing::debug!("captura ainda indisponível: {err:#}");
                    self.retry_after = now + Duration::from_millis(250);
                    return Ok(None);
                }
            }
        }
        let duplication = self.duplication.clone().expect("verificado acima");

        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if let Err(err) = duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) {
                return match err.code() {
                    code if code == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
                    code if code == DXGI_ERROR_ACCESS_LOST => {
                        self.duplication = None;
                        Ok(None)
                    }
                    _ => Err(anyhow!(err).context("AcquireNextFrame")),
                };
            }

            // LastPresentTime zero: só o ponteiro do mouse se mexeu.
            let image_changed = frame_info.LastPresentTime != 0;
            let copied = if image_changed { self.copy_out(resource) } else { Ok(()) };
            let _ = duplication.ReleaseFrame();
            copied?;
            if !image_changed {
                return Ok(None);
            }
        }

        let (width, height) = self.size;
        Ok(Some(Frame { data: &self.buffer, width, height, stride: width * 4 }))
    }

    unsafe fn copy_out(&mut self, resource: Option<IDXGIResource>) -> Result<()> {
        unsafe {
            let texture: ID3D11Texture2D =
                resource.ok_or_else(|| anyhow!("quadro sem textura"))?.cast().context("textura do quadro")?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);

            let reuse = matches!(&self.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
            if !reuse {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                    MipLevels: 1,
                    ArraySize: 1,
                    ..desc
                };
                let mut staging = None;
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .context("criar textura de leitura")?;
                let staging = staging.ok_or_else(|| anyhow!("CreateTexture2D não devolveu textura"))?;
                self.staging = Some((staging, desc.Width, desc.Height));
            }
            let staging = &self.staging.as_ref().expect("criada acima").0;

            self.context.CopyResource(staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).context("mapear textura")?;

            let (width, height) = (desc.Width as usize, desc.Height as usize);
            let row = width * 4;
            self.buffer.resize(row * height, 0);
            let pitch = mapped.RowPitch as usize;
            let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (height - 1) + row);
            for y in 0..height {
                self.buffer[y * row..(y + 1) * row].copy_from_slice(&src[y * pitch..y * pitch + row]);
            }
            self.context.Unmap(staging, 0);
            self.size = (width, height);
        }
        Ok(())
    }
}

fn open(display: usize) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutputDuplication, DisplayInfo)> {
    let mut outputs = enumerate()?;
    if display >= outputs.len() {
        bail!("monitor {display} não existe ({} encontrados)", outputs.len());
    }
    let Output { adapter, output, info } = outputs.swap_remove(display);
    unsafe {
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        let device = device.ok_or_else(|| anyhow!("sem dispositivo D3D11"))?;
        let context = context.ok_or_else(|| anyhow!("sem contexto D3D11"))?;
        let duplication = output
            .cast::<IDXGIOutput1>()
            .context("IDXGIOutput1")?
            .DuplicateOutput(&device)
            .context("DuplicateOutput (a tela está bloqueada ou em um prompt do UAC?)")?;
        Ok((device, context, duplication, info))
    }
}
