//! O agente de ponta a ponta, sem o serviço e sem privilégio: o protocolo, a
//! prova de identidade, a memória compartilhada, a captura e a entrada. O que
//! só existe com o serviço instalado (rodar como SYSTEM, seguir o UAC e a tela
//! de bloqueio) não tem como ser testado daqui.
//!
//! Mexem no computador de verdade, então ficam ignorados por padrão; rode com
//! `cargo test -p opendesk-platform -- --ignored` numa sessão com área de
//! trabalho.
#![cfg(windows)]

use opendesk_platform::agent::{RemoteCapturer, RemoteInjector, serve_unverified};
use opendesk_proto::control::InputEvent;
use std::time::Duration;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursorPos(point: *mut [i32; 2]) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
}

fn cursor() -> (i32, i32) {
    let mut point = [0i32; 2];
    assert_ne!(unsafe { GetCursorPos(&mut point) }, 0);
    (point[0], point[1])
}

/// Sobe um agente de teste nesta thread de fundo e devolve o nome do pipe.
fn start_agent(tag: &str) -> String {
    opendesk_platform::init_process();
    let pipe = format!(r"\\.\pipe\OpenDesk.Test.{tag}.{}", std::process::id());
    let name = pipe.clone();
    std::thread::spawn(move || panic!("o agente de teste parou: {:#}", serve_unverified(&name).unwrap_err()));
    pipe
}

/// O pipe leva um instante para existir depois que a thread do agente sobe.
fn eventually<T>(mut attempt: impl FnMut() -> anyhow::Result<T>) -> T {
    let mut last = None;
    for _ in 0..100 {
        match attempt() {
            Ok(value) => return value,
            Err(err) => last = Some(err),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("o agente não respondeu: {:#}", last.unwrap());
}

#[test]
#[ignore = "captura a tela real"]
fn frames_arrive_through_shared_memory() {
    let pipe = start_agent("frames");
    let mut capturer = eventually(|| RemoteCapturer::open_unverified(&pipe, 0));
    let display = capturer.display().clone();
    assert!(display.width > 0 && display.height > 0);

    // O primeiro quadro da duplicação é sempre a tela inteira.
    let first = (0..50).find_map(|_| {
        capturer
            .frame(100)
            .unwrap()
            .map(|f| (f.width, f.height, f.stride, f.data.len(), f.data.iter().any(|&b| b != 0)))
    });
    let (width, height, stride, len, has_pixels) = first.expect("nenhum quadro em 5 s");
    assert_eq!((width as u32, height as u32), (display.width, display.height));
    assert_eq!(stride, width * 4);
    assert_eq!(len, width * height * 4);
    assert!(has_pixels, "o quadro chegou vazio: a memória compartilhada não foi escrita");

    // Pedidos seguintes reutilizam a mesma memória e não travam com a tela parada.
    for _ in 0..5 {
        capturer.frame(20).unwrap();
    }
}

#[test]
#[ignore = "move o cursor real por um instante"]
fn input_reaches_the_expected_pixel() {
    let pipe = start_agent("input");
    let display = eventually(|| RemoteCapturer::open_unverified(&pipe, 0)).display().clone();
    let mut injector = RemoteInjector::open_unverified(&pipe, &display).unwrap();
    let original = cursor();

    let mut failures = Vec::new();
    for (nx, ny) in [(32768u16, 32768u16), (16384, 49152)] {
        injector.apply(InputEvent::MouseMove { x: nx, y: ny }).unwrap();
        let expected = (
            display.x + (nx as f64 / 65535.0 * (display.width - 1) as f64).round() as i32,
            display.y + (ny as f64 / 65535.0 * (display.height - 1) as f64).round() as i32,
        );
        // A entrada vai por outra thread: espera o cursor chegar.
        let arrived = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            let got = cursor();
            (got.0 - expected.0).abs() <= 1 && (got.1 - expected.1).abs() <= 1
        });
        if !arrived {
            failures.push(format!("({nx}, {ny}): esperava {expected:?}, cursor em {:?}", cursor()));
        }
    }
    injector.release_all().unwrap();

    unsafe { SetCursorPos(original.0, original.1) };
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
#[ignore = "abre um pipe nomeado"]
fn a_connection_that_skips_the_proof_gets_nothing() {
    use std::io::{Read, Write};
    let pipe = start_agent("proof");
    let mut raw = eventually(|| Ok(std::fs::File::options().read(true).write(true).open(&pipe)?));

    // Lê o desafio e responde com um pedido de captura, sem a prova.
    let mut len = [0u8; 4];
    raw.read_exact(&mut len).unwrap();
    let mut challenge = vec![0u8; u32::from_le_bytes(len) as usize];
    raw.read_exact(&mut challenge).unwrap();
    // Variante 1 de `Request` (Capture), monitor 0.
    raw.write_all(&[2, 0, 0, 0, 1, 0]).unwrap();

    // O agente responde com a recusa e fecha; nunca com dados de captura.
    raw.read_exact(&mut len).unwrap();
    let mut reply = vec![0u8; u32::from_le_bytes(len) as usize];
    raw.read_exact(&mut reply).unwrap();
    assert_eq!(reply[0], 1, "esperava a variante Denied, veio {reply:?}");
    assert_eq!(raw.read(&mut [0u8; 1]).unwrap_or(0), 0, "a conexão devia ter sido fechada");
}
