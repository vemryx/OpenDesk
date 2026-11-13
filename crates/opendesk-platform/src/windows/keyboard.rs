//! Captura de teclado no visualizador com um gancho de baixo nível.
//!
//! Enquanto a visão remota está em foco, as teclas são desviadas para a
//! sessão em vez de irem para o Windows local; é assim que Alt+Tab, a tecla
//! Windows e afins chegam ao computador remoto. O gancho só desvia teclas
//! enquanto está ativo, e eventos injetados por software são sempre deixados
//! passar (senão, com host e visualizador na mesma máquina, a entrada injetada
//! pelo host voltaria para a sessão em laço).

use crate::keyboard::KeyEvent;
use anyhow::{Context, Result, anyhow};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, SetWindowsHookExW,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<Mutex<State>> = OnceLock::new();

struct State {
    sender: Sender<KeyEvent>,
    down: HashSet<(u16, bool)>,
}

/// Gancho de teclado instalado para a vida do processo.
pub struct KeyboardGrab(());

impl KeyboardGrab {
    /// Instala o gancho (inativo) e devolve o canal por onde chegam as teclas.
    /// Só pode ser chamado uma vez por processo.
    pub fn install() -> Result<(Self, Receiver<KeyEvent>)> {
        let (sender, receiver) = channel();
        STATE
            .set(Mutex::new(State { sender, down: HashSet::new() }))
            .map_err(|_| anyhow!("o gancho de teclado já foi instalado"))?;

        // Ganchos de baixo nível são chamados na thread que os instalou, que
        // precisa ter um laço de mensagens.
        let (ready_tx, ready_rx) = channel::<Result<()>>();
        std::thread::Builder::new()
            .name("opendesk-keyboard".into())
            .spawn(move || unsafe {
                let module = GetModuleHandleW(None).ok().map(Into::into);
                match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module, 0) {
                    Ok(_hook) => {
                        let _ = ready_tx.send(Ok(()));
                        let mut msg = MSG::default();
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(anyhow!(err).context("SetWindowsHookExW")));
                    }
                }
            })
            .context("criar thread do teclado")?;
        ready_rx.recv().context("thread do teclado morreu")??;
        Ok((Self(()), receiver))
    }

    /// Liga ou desliga o desvio de teclas. Ao desligar, solta no remoto tudo
    /// o que ainda estava pressionado, para não deixar teclas presas.
    pub fn set_active(&self, active: bool) {
        if ACTIVE.swap(active, Ordering::SeqCst) == active || active {
            return;
        }
        if let Some(state) = STATE.get() {
            let mut state = state.lock().unwrap();
            let held: Vec<_> = state.down.drain().collect();
            for (scancode, extended) in held {
                let _ = state.sender.send(KeyEvent { scancode, extended, down: false });
            }
        }
    }
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && ACTIVE.load(Ordering::SeqCst) {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if (info.flags & LLKHF_INJECTED).0 == 0 {
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let extended = (info.flags & LLKHF_EXTENDED).0 != 0;
            let mut scancode = info.scanCode as u16;
            if scancode == 0 {
                scancode = unsafe { MapVirtualKeyW(info.vkCode, MAPVK_VK_TO_VSC) } as u16;
            }
            if scancode != 0 {
                if let Some(state) = STATE.get() {
                    let mut state = state.lock().unwrap();
                    if down {
                        state.down.insert((scancode, extended));
                    } else {
                        state.down.remove(&(scancode, extended));
                    }
                    let _ = state.sender.send(KeyEvent { scancode, extended, down });
                }
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
