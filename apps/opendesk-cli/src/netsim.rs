//! Uma internet ruim de mentira, para medir o OpenDesk sem sair de casa.
//!
//! Fica entre o visualizador e o host, repassando os pacotes UDP dos dois com
//! o que uma conexão residencial tem de pior: banda limitada, atraso, perda e
//! a fila do roteador (que é onde o atraso se esconde quando alguém manda mais
//! do que o enlace escoa). O visualizador conecta na porta de entrada e o
//! host nem percebe.
//!
//! Usa threads e sockets bloqueantes de propósito: a precisão de um
//! temporizador de runtime assíncrono no Windows não basta para espaçar
//! pacotes de 1 ms em 1 ms.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// O enlace simulado. Vale igual para os dois sentidos.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// Banda, em megabits por segundo.
    pub mbit: f64,
    /// Atraso de propagação de cada sentido, em milissegundos.
    pub delay_ms: f64,
    /// Pacotes perdidos ao acaso, em porcentagem.
    pub loss_pct: f64,
    /// Tamanho da fila do roteador, em milissegundos de banda. Acima disso o
    /// pacote é descartado.
    pub queue_ms: f64,
}

#[derive(Default)]
struct Counters {
    forwarded: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
}

/// Uma fila entre quem recebe do socket e quem entrega no destino.
#[derive(Default)]
struct Pipe {
    queue: Mutex<VecDeque<(Instant, Vec<u8>)>>,
    arrived: Condvar,
}

/// Repassa pacotes entre `listen` e `to` até o processo ser encerrado.
pub fn run(listen: u16, to: SocketAddr, link: Link) -> Result<()> {
    let front = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, listen)).context("abrir a porta de entrada")?);
    let back = Arc::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).context("abrir a porta de saída")?);
    // Quem está do lado do visualizador. Aprendido no primeiro pacote.
    let client: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let counters: [Arc<Counters>; 2] = Default::default();

    println!(
        "Rede simulada em 127.0.0.1:{listen} -> {to}: {} Mbit/s, {} ms por sentido, {}% de perda, fila de {} ms",
        link.mbit, link.delay_ms, link.loss_pct, link.queue_ms
    );

    // Subida: do visualizador para o host.
    {
        let (pipe, counters) = (Arc::new(Pipe::default()), counters[0].clone());
        let (socket, client) = (front.clone(), client.clone());
        let (enqueue, counters_in) = (pipe.clone(), counters.clone());
        spawn("netsim-sobe-entra", move || {
            receive(&socket, &enqueue, link, 0x9E37_79B9, &counters_in, |from| *client.lock().unwrap() = Some(from))
        });
        let socket = back.clone();
        spawn("netsim-sobe-sai", move || deliver(&pipe, |packet| drop(socket.send_to(packet, to))));
    }
    // Descida: do host para o visualizador.
    {
        let (pipe, counters_in) = (Arc::new(Pipe::default()), counters[1].clone());
        let (socket, enqueue) = (back, pipe.clone());
        spawn("netsim-desce-entra", move || receive(&socket, &enqueue, link, 0x7F4A_7C15, &counters_in, |_| {}));
        spawn("netsim-desce-sai", move || {
            deliver(&pipe, |packet| {
                if let Some(client) = *client.lock().unwrap() {
                    let _ = front.send_to(packet, client);
                }
            })
        });
    }

    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let line = |name: &str, c: &Counters| {
            let (ok, lost) = (c.forwarded.load(Ordering::Relaxed), c.dropped.load(Ordering::Relaxed));
            let mbit = c.bytes.load(Ordering::Relaxed) as f64 * 8.0 / 1e6 / started.elapsed().as_secs_f64();
            format!("{name}: {ok} pacotes, {lost} descartados, média {mbit:.2} Mbit/s")
        };
        println!("{} | {}", line("sobe", &counters[0]), line("desce", &counters[1]));
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().name(name.into()).spawn(body).expect("criar thread da rede simulada");
}

/// Recebe pacotes e decide quando (e se) cada um chega ao outro lado.
fn receive(
    socket: &UdpSocket,
    pipe: &Pipe,
    link: Link,
    seed: u64,
    counters: &Counters,
    mut seen_from: impl FnMut(SocketAddr),
) {
    let bytes_per_second = (link.mbit * 1e6 / 8.0).max(1.0);
    let delay = Duration::from_secs_f64(link.delay_ms.max(0.0) / 1e3);
    let queue_limit = Duration::from_secs_f64(link.queue_ms.max(1.0) / 1e3);
    let mut random = seed;
    // Quando o enlace termina de transmitir o que já aceitou.
    let mut busy_until = Instant::now();
    let mut buffer = vec![0u8; 65536];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buffer) else { continue };
        seen_from(from);
        let now = Instant::now();
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let lost = (random % 100_000) as f64 / 1000.0 < link.loss_pct;
        let start = busy_until.max(now);
        // Fila cheia: o roteador joga fora, como um de verdade.
        if lost || start - now > queue_limit {
            counters.dropped.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        busy_until = start + Duration::from_secs_f64(len as f64 / bytes_per_second);
        counters.forwarded.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(len as u64, Ordering::Relaxed);
        pipe.queue.lock().unwrap().push_back((busy_until + delay, buffer[..len].to_vec()));
        pipe.arrived.notify_one();
    }
}

/// Entrega cada pacote na hora marcada. Os horários são crescentes, então a
/// fila sai na ordem em que entrou.
fn deliver(pipe: &Pipe, mut send: impl FnMut(&[u8])) {
    loop {
        let (at, packet) = {
            let mut queue = pipe.queue.lock().unwrap();
            loop {
                match queue.pop_front() {
                    Some(next) => break next,
                    None => queue = pipe.arrived.wait(queue).unwrap(),
                }
            }
        };
        let now = Instant::now();
        if at > now {
            std::thread::sleep(at - now);
        }
        send(&packet);
    }
}
