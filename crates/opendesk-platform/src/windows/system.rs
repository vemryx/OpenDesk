//! Integração com o Windows fora da sessão em si: abrir junto com o sistema,
//! bloquear a estação e a hora local.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::HSTRING;

/// Programas listados aqui abrem quando o usuário entra no Windows. É a chave
/// do próprio usuário: não exige administrador.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "OpenDesk";

/// O comando registrado para abrir com o Windows, se houver.
pub fn autostart_command() -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            &HSTRING::from(RUN_VALUE),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    // O tamanho devolvido inclui o terminador nulo.
    let len = (bytes as usize / 2).saturating_sub(1).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// Registra `command` para abrir com o Windows, ou remove o registro.
pub fn set_autostart(command: Option<&str>) -> Result<()> {
    let (key, value) = (HSTRING::from(RUN_KEY), HSTRING::from(RUN_VALUE));
    match command {
        Some(command) => {
            let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
            let status = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    &key,
                    &value,
                    REG_SZ.0,
                    Some(wide.as_ptr().cast()),
                    (wide.len() * 2) as u32,
                )
            };
            status.ok().context("gravar a inicialização automática no registro")
        }
        None => {
            let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &value) };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            status.ok().context("remover a inicialização automática do registro")
        }
    }
}

/// Volta para a tela de bloqueio do Windows.
pub fn lock_workstation() {
    if let Err(err) = unsafe { LockWorkStation() } {
        tracing::warn!("não foi possível bloquear o computador: {err}");
    }
}

/// Data e hora locais, como `2026-11-12 22:41`.
pub fn local_time() -> String {
    let now = unsafe { GetLocalTime() };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute)
}
