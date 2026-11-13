// Servidor de encontro do OpenDesk.
//
// Faz uma coisa só: quando alguém digita um ID, entrega ao dono daquele ID os
// endereços de quem quer conectar, e devolve os endereços do dono. A partir
// daí os dois se falam direto, por QUIC cifrado de ponta a ponta. Imagem,
// teclado e mouse nunca passam por aqui.
//
// Cada ID tem o seu Durable Object. O dispositivo dono mantém um WebSocket
// aberto em /v1/listen/<id>; quem quer conectar abre um em /v1/dial/<id>.
//
// O servidor não é confiável por construção: mesmo que minta sobre endereços
// ou impressões digitais, o handshake TLS entre os pares é que autentica, e a
// pessoa no computador de destino ainda precisa aprovar.

import { DurableObject } from "cloudflare:workers";
import { hex, unhex, verifyRegistration } from "./identity";

export interface Env {
  DEVICE: DurableObjectNamespace<Device>;
}

const ROUTE = /^\/v1\/(listen|dial)\/([1-9]\d{8})$/;
const MAX_MESSAGE = 4096;
const MAX_CANDIDATES = 8;
const CANDIDATE = /^(\d{1,3}\.){3}\d{1,3}:\d{1,5}$/;
/** Quanto um pedido espera pela resposta do dono do ID. */
const DIAL_TIMEOUT_MS = 15_000;
const MAX_WAITING = 8;
/** Pedidos de conexão aceitos por minuto para um mesmo ID. */
const OFFERS_PER_MINUTE = 20;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const match = ROUTE.exec(url.pathname);
    if (!match) {
      if (url.pathname === "/") return new Response("OpenDesk rendezvous v1\n");
      return new Response("não encontrado\n", { status: 404 });
    }
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("esperava uma conexão WebSocket\n", { status: 426 });
    }
    const id = match[2]!;
    return env.DEVICE.get(env.DEVICE.idFromName(id)).fetch(request);
  },
} satisfies ExportedHandler<Env>;

type Attachment =
  | { role: "pending"; id: string; nonce: string; at: number }
  | { role: "host"; fingerprint: string }
  | { role: "dial"; session: string; at: number; offered: boolean };

function candidates(value: unknown): string[] | null {
  if (!Array.isArray(value) || value.length === 0 || value.length > MAX_CANDIDATES) return null;
  return value.every((c) => typeof c === "string" && CANDIDATE.test(c)) ? (value as string[]) : null;
}

export class Device extends DurableObject<Env> {
  private offers: number[] = [];

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // Respondido pela plataforma sem acordar o objeto: manter um dispositivo
    // online não consome tempo de execução.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  async fetch(request: Request): Promise<Response> {
    const match = ROUTE.exec(new URL(request.url).pathname);
    if (!match) return new Response("não encontrado\n", { status: 404 });
    const [, role, id] = match as unknown as [string, "listen" | "dial", string];

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    const sockets = this.ctx.getWebSockets();

    if (role === "listen") {
      if (sockets.filter((ws) => this.attachment(ws)?.role === "pending").length >= MAX_WAITING) {
        return new Response("muitas tentativas de registro\n", { status: 429 });
      }
      const nonce = hex(crypto.getRandomValues(new Uint8Array(32)));
      this.ctx.acceptWebSocket(server);
      server.serializeAttachment({ role: "pending", id, nonce, at: Date.now() } satisfies Attachment);
      server.send(JSON.stringify({ t: "challenge", nonce }));
    } else {
      this.ctx.acceptWebSocket(server);
      if (!this.host()) {
        this.fail(server, "offline");
      } else if (sockets.filter((ws) => this.attachment(ws)?.role === "dial").length >= MAX_WAITING) {
        this.fail(server, "busy");
      } else {
        const session = hex(crypto.getRandomValues(new Uint8Array(12)));
        server.serializeAttachment({ role: "dial", session, at: Date.now(), offered: false } satisfies Attachment);
      }
    }
    await this.scheduleCleanup();
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, raw: string | ArrayBuffer): Promise<void> {
    const state = this.attachment(ws);
    if (!state || typeof raw !== "string" || raw.length > MAX_MESSAGE) return this.fail(ws, "bad_request");
    let message: Record<string, unknown>;
    try {
      message = JSON.parse(raw);
    } catch {
      return this.fail(ws, "bad_request");
    }
    if (typeof message !== "object" || message === null) return this.fail(ws, "bad_request");

    if (state.role === "pending" && message.t === "register") return this.register(ws, state, message);
    if (state.role === "dial" && message.t === "offer") return this.offer(ws, state, message);
    if (state.role === "host" && message.t === "answer") return this.answer(state, message);
    this.fail(ws, "bad_request");
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    try {
      ws.close(1000);
    } catch {
      // já fechado
    }
  }

  /** Fecha pedidos e registros que ficaram pelo caminho. */
  async alarm(): Promise<void> {
    const now = Date.now();
    for (const ws of this.ctx.getWebSockets()) {
      const state = this.attachment(ws);
      if (state && state.role !== "host" && now - state.at >= DIAL_TIMEOUT_MS) this.fail(ws, "timeout");
    }
    await this.scheduleCleanup();
  }

  private async register(
    ws: WebSocket,
    state: Extract<Attachment, { role: "pending" }>,
    message: Record<string, unknown>,
  ): Promise<void> {
    const verified = await verifyRegistration(state.id, state.nonce, message);
    if (!verified.ok) return this.fail(ws, verified.code);

    // O primeiro dispositivo a provar posse de uma chave que dá este ID fica
    // dono dele. Sem isso, um ID de 9 dígitos poderia ser tomado por quem
    // fabricasse outro certificado com o mesmo ID.
    const owner = await this.ctx.storage.get<string>("owner");
    if (owner && owner !== verified.fingerprint) return this.fail(ws, "id_taken");
    if (!owner) await this.ctx.storage.put("owner", verified.fingerprint);

    // Uma conexão nova do mesmo dispositivo substitui a anterior.
    for (const other of this.ctx.getWebSockets()) {
      if (other !== ws && this.attachment(other)?.role === "host") this.close(other, "replaced");
    }
    ws.serializeAttachment({ role: "host", fingerprint: verified.fingerprint } satisfies Attachment);
    ws.send(JSON.stringify({ t: "registered", id: state.id }));
  }

  private offer(ws: WebSocket, state: Extract<Attachment, { role: "dial" }>, message: Record<string, unknown>): void {
    const host = this.host();
    const list = candidates(message.candidates);
    const fingerprint = unhex(message.fingerprint, 32);
    if (state.offered || !list || fingerprint?.length !== 32) return this.fail(ws, "bad_request");
    if (!host) return this.fail(ws, "offline");

    const now = Date.now();
    this.offers = this.offers.filter((at) => now - at < 60_000);
    if (this.offers.length >= OFFERS_PER_MINUTE) return this.fail(ws, "busy");
    this.offers.push(now);

    ws.serializeAttachment({ ...state, offered: true } satisfies Attachment);
    host.send(JSON.stringify({ t: "offer", session: state.session, fingerprint: message.fingerprint, candidates: list }));
  }

  private answer(state: Extract<Attachment, { role: "host" }>, message: Record<string, unknown>): void {
    const list = candidates(message.candidates);
    if (typeof message.session !== "string" || !list) return;
    for (const ws of this.ctx.getWebSockets()) {
      const dial = this.attachment(ws);
      if (dial?.role === "dial" && dial.offered && dial.session === message.session) {
        // A impressão digital vem do registro verificado, não do que o host
        // disser agora.
        ws.send(JSON.stringify({ t: "answer", fingerprint: state.fingerprint, candidates: list }));
        this.close(ws, "done");
      }
    }
  }

  private host(): WebSocket | undefined {
    return this.ctx.getWebSockets().find((ws) => this.attachment(ws)?.role === "host");
  }

  private attachment(ws: WebSocket): Attachment | null {
    return ws.deserializeAttachment() as Attachment | null;
  }

  private fail(ws: WebSocket, code: string): void {
    try {
      ws.send(JSON.stringify({ t: "error", code }));
    } catch {
      // já fechado
    }
    this.close(ws, code);
  }

  private close(ws: WebSocket, reason: string): void {
    try {
      ws.close(1000, reason);
    } catch {
      // já fechado
    }
  }

  private async scheduleCleanup(): Promise<void> {
    const waiting = this.ctx.getWebSockets().some((ws) => this.attachment(ws)?.role !== "host");
    if (waiting && (await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(Date.now() + DIAL_TIMEOUT_MS);
    }
  }
}
