//! O ritmo do vídeo: quando produzir o próximo quadro, e de que tamanho.
//!
//! O atraso de um acesso remoto quase nunca está na captura ou no codec: está
//! em filas. Um host que codifica quadros mais rápido do que a conexão escoa
//! empilha quadros prontos, e cada um deles já está velho quando chega. Numa
//! conexão lenta isso vira segundos entre o clique e a resposta na tela.
//!
//! Aqui não há fila. O host só produz um quadro quando a conexão tem caminho
//! livre para ele, e por isso todo quadro enviado mostra a tela como ela
//! estava instantes antes. Quem vê confirma cada quadro assim que ele está na
//! tela ([`ViewerMsg::Seen`]); dessas confirmações saem as duas coisas de que
//! o host precisa:
//!
//! * **se há caminho livre**: o que foi enviado e ainda não foi visto;
//! * **quanto a conexão escoa**: bytes confirmados sobre o tempo que levaram.
//!
//! A vazão medida vira o alvo de bytes do próximo quadro (o que atravessa a
//! conexão em [`FRAME_TIME`]), e o codec desce a qualidade para caber nele. O
//! resultado é uma imagem que borra por um instante numa conexão ruim, em vez
//! de uma imagem nítida de segundos atrás.
//!
//! [`ViewerMsg::Seen`]: opendesk_proto::control::ViewerMsg::Seen

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Quanto tempo um quadro pode levar para atravessar a conexão. É o que
/// limita o atraso quando a tela inteira muda sem parar.
pub const FRAME_TIME: Duration = Duration::from_millis(33);
/// O primeiro quadro (a tela inteira) pode levar mais: ainda não há imagem
/// nenhuma para envelhecer.
const FIRST_FRAME_TIME: Duration = Duration::from_millis(130);
/// Vazão presumida antes da primeira medida, em bytes por segundo (8 Mbit/s).
/// Errar para baixo custa um primeiro quadro borrado por um instante; errar
/// para cima custaria segundos de espera numa conexão lenta.
const INITIAL_RATE: f64 = 1_000_000.0;
const MIN_BUDGET: usize = 6 * 1024;
const MAX_BUDGET: usize = 32 * 1024 * 1024;
/// Quadros pequenos assim seguem sem esperar confirmação: é o que mantém 60
/// quadros por segundo quando só um botão muda de cor.
const SLACK_TIME: Duration = Duration::from_millis(8);
const MIN_SLACK: usize = 6 * 1024;
/// O próximo quadro começa a ser produzido este tempo antes de o anterior
/// terminar de sair, para a conexão não ficar ociosa durante a codificação.
const LEAD: Duration = Duration::from_millis(8);
/// As medidas de vazão que contam: as mais recentes, dentro desta janela.
const WINDOW: Duration = Duration::from_secs(2);
const MAX_SAMPLES: usize = 8;
/// Uma medida só puxa a vazão para cima até este fator: um acerto de sorte no
/// relógio não dobra o tamanho dos quadros de uma vez. Para baixo não há
/// limite, porque é o lado que evita atraso.
const MAX_GROWTH: f64 = 2.0;
/// Sem confirmação nenhuma, sai um quadro de vez em quando assim mesmo: um
/// par que não confirma não pode congelar a imagem. A espera conta do último
/// envio e cresce com o que já está a caminho (1 s, depois 2 s, 3 s…). Sem
/// isso, uma conexão que leva mais de um segundo por quadro receberia quadros
/// sem parar, que é a fila que tudo aqui existe para evitar.
const STALL: Duration = Duration::from_secs(1);

struct Flying {
    id: u32,
    bytes: usize,
    sent_at: Instant,
}

/// Decide o ritmo. Não toca em rede nem em relógio: recebe os instantes de
/// quem chama, para poder ser testado com uma conexão de mentira.
pub struct Pacer {
    /// Bytes por segundo que a conexão escoou nas últimas medidas.
    rate: f64,
    /// (quando, bytes, segundos) de cada medida.
    samples: VecDeque<(Instant, usize, f64)>,
    /// Enviados e ainda não vistos, do mais antigo para o mais novo.
    flying: VecDeque<Flying>,
    next_id: u32,
    /// Quando o quadro confirmado mais recente terminou de chegar a quem vê.
    last_seen: Option<Instant>,
    /// O menor tempo de ida e volta já observado: o da conexão vazia.
    base_rtt: Option<Duration>,
    confirmed_any: bool,
}

impl Default for Pacer {
    fn default() -> Self {
        Self {
            rate: INITIAL_RATE,
            samples: VecDeque::new(),
            flying: VecDeque::new(),
            next_id: 0,
            last_seen: None,
            base_rtt: None,
            confirmed_any: false,
        }
    }
}

impl Pacer {
    /// Bytes por segundo que a conexão está escoando.
    pub fn rate(&self) -> f64 {
        self.rate
    }

    /// Quanto o próximo quadro pode pesar.
    pub fn budget(&self) -> usize {
        let time = if self.confirmed_any { FRAME_TIME } else { FIRST_FRAME_TIME };
        ((self.rate * time.as_secs_f64()) as usize).clamp(MIN_BUDGET, MAX_BUDGET)
    }

    fn slack(&self) -> usize {
        ((self.rate * SLACK_TIME.as_secs_f64()) as usize).max(MIN_SLACK)
    }

    /// Quando a conexão deve ter terminado de transmitir o que está a caminho,
    /// supondo a vazão medida. `None` se não dá para prever (há mais de um
    /// quadro grande a caminho: melhor esperar a confirmação).
    fn clears_at(&self) -> Option<Instant> {
        let newest = self.flying.back()?;
        let pending: usize = self.flying.iter().map(|frame| frame.bytes).sum();
        if pending - newest.bytes > self.slack() {
            return None;
        }
        Some(newest.sent_at + Duration::from_secs_f64(pending as f64 / self.rate))
    }

    /// Há caminho livre para mais um quadro?
    pub fn ready(&self, now: Instant) -> bool {
        self.wait(now).is_zero()
    }

    /// Quanto esperar antes de olhar de novo, se nenhuma confirmação chegar.
    /// Zero quando já há caminho livre.
    pub fn wait(&self, now: Instant) -> Duration {
        let pending: usize = self.flying.iter().map(|frame| frame.bytes).sum();
        if pending <= self.slack() {
            return Duration::ZERO;
        }
        let stalled = self.flying.back().map_or(now, |newest| newest.sent_at + STALL * self.flying.len() as u32);
        let clears = self.clears_at().map_or(stalled, |at| at.checked_sub(LEAD).unwrap_or(at));
        clears.min(stalled).saturating_duration_since(now)
    }

    /// Reserva o número do próximo quadro.
    pub fn begin(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        id
    }

    /// O quadro `id`, de `bytes` bytes, foi entregue à rede.
    pub fn sent(&mut self, id: u32, bytes: usize, now: Instant) {
        self.flying.push_back(Flying { id, bytes, sent_at: now });
    }

    /// O quadro `id` foi descartado antes de ir para a rede.
    pub fn dropped(&mut self, id: u32) {
        self.flying.retain(|frame| frame.id != id);
    }

    /// Quem vê confirmou o quadro `id` e todos os anteriores. `rtt` é o tempo
    /// de ida e volta da conexão neste momento, e `spent` o que quem vê levou
    /// para decodificar e aplicar o quadro: esse tempo é do computador dele,
    /// não da conexão, e não entra na conta da vazão.
    pub fn seen(&mut self, id: u32, now: Instant, rtt: Duration, spent: Duration) {
        let base = *self.base_rtt.insert(self.base_rtt.map_or(rtt, |base| base.min(rtt)));
        let (mut bytes, mut first_sent) = (0, None);
        // Os números dão a volta em 2³²; "anterior" é estar até meia volta atrás.
        while let Some(oldest) = self.flying.front().filter(|oldest| id.wrapping_sub(oldest.id) < u32::MAX / 2) {
            bytes += oldest.bytes;
            first_sent.get_or_insert(oldest.sent_at);
            self.flying.pop_front();
        }
        let Some(first_sent) = first_sent else { return };
        self.confirmed_any = true;

        // O tempo em que a conexão trabalhou nestes bytes: desde que eles
        // puderam começar a chegar (o envio mais a ida e volta, ou a chegada
        // do quadro anterior, se já havia um na frente) até terminarem de
        // chegar. A confirmação sai depois de quem vê decodificar, então a
        // chegada foi `spent` antes dela.
        let arrived = now.checked_sub(spent).unwrap_or(now);
        let started = self.last_seen.map_or(first_sent + base, |previous| previous.max(first_sent + base));
        self.last_seen = Some(arrived);
        // Quadro pequeno demais mede o relógio, não a conexão.
        if bytes < (self.budget() / 2).max(4 * 1024) {
            return;
        }
        let seconds = arrived.saturating_duration_since(started).as_secs_f64().max(0.001);
        self.samples.push_back((now, bytes, seconds));
        while self.samples.len() > MAX_SAMPLES
            || self.samples.front().is_some_and(|&(at, _, _)| now.duration_since(at) > WINDOW)
        {
            self.samples.pop_front();
        }
        let (total_bytes, total_seconds) =
            self.samples.iter().fold((0.0, 0.0), |(b, s), &(_, bytes, seconds)| (b + bytes as f64, s + seconds));
        self.rate = (total_bytes / total_seconds).min(self.rate * MAX_GROWTH).max(1.0);
    }
}

/// O [`Pacer`] entre a thread que captura e as tarefas de rede.
#[derive(Default)]
pub struct Gate {
    pacer: Mutex<Pacer>,
    changed: Condvar,
}

impl Gate {
    /// Bloqueia até haver caminho livre para mais um quadro e devolve o alvo
    /// de bytes dele. `None` se `limit` passou antes (quem chama aproveita
    /// para ver se deve parar).
    pub fn wait(&self, limit: Duration) -> Option<usize> {
        let deadline = Instant::now() + limit;
        let mut pacer = self.pacer.lock().unwrap();
        loop {
            let now = Instant::now();
            let wait = pacer.wait(now);
            if wait.is_zero() {
                return Some(pacer.budget());
            }
            if now >= deadline {
                return None;
            }
            let sleep = wait.min(deadline - now);
            pacer = self.changed.wait_timeout(pacer, sleep).unwrap().0;
        }
    }

    pub fn begin(&self) -> u32 {
        self.pacer.lock().unwrap().begin()
    }

    pub fn sent(&self, id: u32, bytes: usize) {
        self.pacer.lock().unwrap().sent(id, bytes, Instant::now());
    }

    pub fn dropped(&self, id: u32) {
        self.pacer.lock().unwrap().dropped(id);
        self.changed.notify_all();
    }

    pub fn seen(&self, id: u32, rtt: Duration, spent: Duration) {
        self.pacer.lock().unwrap().seen(id, Instant::now(), rtt, spent);
        self.changed.notify_all();
    }

    /// Bytes por segundo que a conexão está escoando.
    pub fn rate(&self) -> f64 {
        self.pacer.lock().unwrap().rate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RTT: Duration = Duration::from_millis(30);

    /// Uma conexão de mentira: escoa `rate` bytes por segundo, e a
    /// confirmação de cada quadro chega uma ida e volta depois de ele sair.
    struct Link {
        rate: f64,
        free_at: Instant,
    }

    impl Link {
        /// Envia e devolve quando a confirmação chega.
        fn send(&mut self, now: Instant, bytes: usize) -> Instant {
            self.free_at = self.free_at.max(now) + Duration::from_secs_f64(bytes as f64 / self.rate);
            self.free_at + RTT
        }
    }

    /// Roda uma sessão em que a tela muda sem parar: cada quadro tem o
    /// tamanho do alvo. Devolve o maior tempo que um quadro levou do envio
    /// até a confirmação, descontada a ida e volta.
    fn saturate(pacer: &mut Pacer, link: &mut Link, start: Instant, frames: usize) -> (Instant, Duration) {
        let mut now = start;
        let mut pending: VecDeque<(Instant, u32, Instant)> = VecDeque::new();
        let mut worst = Duration::ZERO;
        let mut sent = 0;
        while sent < frames || !pending.is_empty() {
            // Entrega as confirmações que já chegaram.
            while let Some(&(at, id, sent_at)) = pending.front().filter(|(at, _, _)| *at <= now) {
                pacer.seen(id, at, RTT, Duration::ZERO);
                worst = worst.max(at.duration_since(sent_at).saturating_sub(RTT));
                pending.pop_front();
            }
            if sent < frames && pacer.ready(now) {
                let (id, bytes) = (pacer.begin(), pacer.budget());
                pacer.sent(id, bytes, now);
                pending.push_back((link.send(now, bytes), id, now));
                sent += 1;
            }
            now += Duration::from_millis(1);
        }
        (now, worst)
    }

    #[test]
    fn small_frames_flow_without_waiting() {
        let mut pacer = Pacer::default();
        let now = Instant::now();
        for _ in 0..3 {
            assert!(pacer.ready(now));
            let id = pacer.begin();
            pacer.sent(id, 1500, now);
        }
        assert!(pacer.ready(now), "três quadros de 1,5 kB não enchem conexão nenhuma");
    }

    #[test]
    fn a_big_frame_holds_the_next_until_it_clears() {
        let mut pacer = Pacer::default();
        let now = Instant::now();
        let id = pacer.begin();
        pacer.sent(id, 400_000, now);
        assert!(!pacer.ready(now));
        // A 1 MB/s presumidos, 400 kB levam 400 ms para sair.
        assert!(!pacer.ready(now + Duration::from_millis(300)));
        assert!(pacer.ready(now + Duration::from_millis(400)));
        // A confirmação libera na hora, sem esperar a previsão.
        pacer.seen(id, now + Duration::from_millis(100), RTT, Duration::ZERO);
        assert!(pacer.ready(now + Duration::from_millis(100)));
    }

    #[test]
    fn two_big_frames_wait_for_a_confirmation() {
        let mut pacer = Pacer::default();
        let now = Instant::now();
        let (first, second) = (pacer.begin(), pacer.begin());
        pacer.sent(first, 400_000, now);
        pacer.sent(second, 400_000, now + Duration::from_millis(395));
        // Com dois a caminho a previsão não vale (por ela, o segundo já teria
        // saído aos 795 ms): só a confirmação do primeiro destrava.
        assert!(!pacer.ready(now + Duration::from_millis(900)));
        pacer.seen(first, now + Duration::from_millis(900), RTT, Duration::ZERO);
        // A confirmação mostrou uma conexão mais lenta que a presumida (400 kB
        // em 870 ms), e é com essa vazão que o segundo passa a ser esperado.
        assert!(pacer.rate() < 500_000.0);
        assert!(!pacer.ready(now + Duration::from_millis(900)));
        assert!(pacer.ready(now + Duration::from_millis(1300)));
    }

    #[test]
    fn a_silent_peer_gets_a_trickle_not_a_flood() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let id = pacer.begin();
        pacer.sent(id, 50_000_000, start);
        assert!(!pacer.ready(start + Duration::from_millis(900)));
        // Um quadro por intervalo, e não todos os que couberem depois dele.
        let mut sent = 0;
        for ms in (1000..=5000).step_by(10) {
            let now = start + Duration::from_millis(ms);
            if pacer.ready(now) {
                let id = pacer.begin();
                pacer.sent(id, 50_000_000, now);
                sent += 1;
            }
        }
        // Aos 1 s e aos 3 s; o seguinte só sairia aos 6 s.
        assert_eq!(sent, 2, "{sent} quadros em 4 s sem nenhuma confirmação");
    }

    #[test]
    fn a_link_slower_than_a_frame_per_second_never_piles_up() {
        // 80 kbit/s: até o menor quadro leva mais de meio segundo.
        let (mut pacer, start) = (Pacer::default(), Instant::now());
        let mut link = Link { rate: 10_000.0, free_at: start };
        let (now, _) = saturate(&mut pacer, &mut link, start, 12);
        let (_, worst) = saturate(&mut pacer, &mut link, now, 12);
        // O alvo mínimo (6 kB) leva 0,6 s nessa conexão; a espera nunca passa
        // de um quadro na frente.
        assert!(worst < Duration::from_millis(1500), "um quadro levou {worst:?} para atravessar");
    }

    #[test]
    fn learns_the_rate_of_a_slow_link_and_keeps_frames_short() {
        // 5 Mbit/s: bem abaixo dos 8 presumidos.
        let (mut pacer, start) = (Pacer::default(), Instant::now());
        let mut link = Link { rate: 625_000.0, free_at: start };
        let (now, _) = saturate(&mut pacer, &mut link, start, 20);
        let error = (pacer.rate() - link.rate).abs() / link.rate;
        assert!(error < 0.15, "vazão medida {:.0}, real {:.0}", pacer.rate(), link.rate);

        // Daqui em diante nenhum quadro leva muito mais que o tempo de um quadro.
        let (_, worst) = saturate(&mut pacer, &mut link, now, 50);
        assert!(worst < FRAME_TIME * 2, "um quadro levou {worst:?} para atravessar");
    }

    #[test]
    fn follows_the_link_up_and_down() {
        let (mut pacer, start) = (Pacer::default(), Instant::now());
        let mut link = Link { rate: 1_250_000.0, free_at: start };
        let (now, _) = saturate(&mut pacer, &mut link, start, 30);
        let at_10 = pacer.budget();

        // A conexão melhora dez vezes: os quadros crescem junto.
        link.rate = 12_500_000.0;
        let (now, _) = saturate(&mut pacer, &mut link, now, 60);
        assert!(pacer.budget() > at_10 * 6, "alvo {} depois de {at_10}", pacer.budget());

        // E piora vinte vezes: o alvo encolhe em poucos quadros.
        link.rate = 625_000.0;
        let (now, _) = saturate(&mut pacer, &mut link, now, 12);
        assert!(pacer.budget() < at_10, "alvo {} com a conexão a 5 Mbit/s", pacer.budget());
        let (_, worst) = saturate(&mut pacer, &mut link, now, 40);
        assert!(worst < FRAME_TIME * 2, "um quadro levou {worst:?} para atravessar");
    }

    #[test]
    fn a_slow_viewer_is_not_mistaken_for_a_slow_link() {
        // Quem vê leva 25 ms para decodificar cada quadro, e só então confirma.
        let spent = Duration::from_millis(25);
        let (mut pacer, start) = (Pacer::default(), Instant::now());
        let mut link = Link { rate: 2_500_000.0, free_at: start };
        let mut now = start;
        for _ in 0..40 {
            let (id, bytes) = (pacer.begin(), pacer.budget());
            pacer.sent(id, bytes, now);
            now = link.send(now, bytes) + spent;
            pacer.seen(id, now, RTT, spent);
        }
        let error = (pacer.rate() - link.rate).abs() / link.rate;
        assert!(error < 0.15, "vazão medida {:.0}, real {:.0}", pacer.rate(), link.rate);
    }

    #[test]
    fn a_dropped_frame_frees_the_way() {
        let mut pacer = Pacer::default();
        let now = Instant::now();
        let id = pacer.begin();
        pacer.sent(id, 400_000, now);
        assert!(!pacer.ready(now));
        pacer.dropped(id);
        assert!(pacer.ready(now));
    }
}
