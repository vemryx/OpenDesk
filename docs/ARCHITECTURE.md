# Arquitetura do OpenDesk

O OpenDesk é um aplicativo de acesso remoto ponto a ponto. Este documento explica como ele é organizado, por que foi feito assim e o que ainda falta.

## Princípios

1. **Ninguém entra sem ser convidado.** Toda sessão passa por uma decisão explícita de quem está no computador. Não existe modo oculto, e a sessão ativa fica sempre anunciada na tela.
2. **Ponto a ponto por padrão.** Imagem, mouse e teclado trafegam direto entre os dois computadores, cifrados de ponta a ponta. Servidores só ajudam os pares a se encontrar e nunca veem o conteúdo.
3. **Custo zero para operar.** Nada no caminho crítico depende de infraestrutura paga pelo projeto. Relay é opcional e hospedado por quem precisa.
4. **Um executável pequeno.** Tudo em Rust, sem runtime nem instalador obrigatório.

## Visão geral

```
            Visualizador                                   Host
┌──────────────────────────────┐          ┌──────────────────────────────────┐
│ apps/opendesk (interface)    │          │ apps/opendesk (interface)        │
│   janela, entrada, textura   │          │   pedido de acesso, faixa ativa  │
├──────────────────────────────┤          ├──────────────────────────────────┤
│ opendesk-session::viewer     │          │ opendesk-session::host           │
│   framebuffer, comandos      │          │   autorização, captura, entrada  │
├──────────────────────────────┤          ├──────────────────────────────────┤
│ opendesk-codec (decodifica)  │          │ opendesk-codec (codifica)        │
├──────────────────────────────┤  QUIC    ├──────────────────────────────────┤
│ opendesk-net  ───────────────┼──────────┼─────────────── opendesk-net      │
│   TLS 1.3 mútuo, identidade  │  UDP     │   TLS 1.3 mútuo, identidade      │
├──────────────────────────────┤          ├──────────────────────────────────┤
│ opendesk-platform::keyboard  │          │ opendesk-platform::capture/input │
└──────────────────────────────┘          └──────────────────────────────────┘
```

| Crate | Responsabilidade | Depende do SO? |
|---|---|---|
| `opendesk-proto` | Bytes no fio: mensagens de controle, pacotes de vídeo, enquadramento | Não |
| `opendesk-codec` | Codec de tela por blocos | Não |
| `opendesk-net` | Identidade do dispositivo e transporte QUIC | Não |
| `opendesk-platform` | Captura de tela, injeção de entrada, captura de teclado | **Sim** (só Windows hoje) |
| `opendesk-session` | Máquinas de estado do host e do visualizador | Não |
| `apps/opendesk` | Interface gráfica (egui/eframe sobre wgpu) | Pouco |
| `apps/opendesk-cli` | Diagnóstico e teste ponta a ponta sem interface | Não |

Portar o OpenDesk para outro sistema é implementar `opendesk-platform` para ele.

## Identidade e segurança

Na primeira execução o dispositivo gera um par de chaves e um certificado autoassinado. A **impressão digital** (SHA-256 do certificado) é a identidade do dispositivo. O **ID de 9 dígitos** é só um apelido derivado dela, para facilitar a conferência por voz; ele não autentica nada.

Toda conexão é QUIC com TLS 1.3 e autenticação mútua. Não há autoridade certificadora: o verificador aceita qualquer certificado, mas confere a assinatura do handshake, então cada lado tem a prova de que o outro possui a chave privada daquela impressão digital. Decidir se aquela identidade é confiável cabe à camada de cima:

- **No host**, uma pessoa vê nome, ID, impressão digital e endereço de quem pede, e escolhe entre recusar, permitir só ver ou permitir controle. O host nunca concede mais do que foi pedido, e confere a permissão a cada evento de entrada recebido (um visualizador modificado não ganha controle por insistir).
- **No visualizador**, `Node::connect` aceita uma impressão digital esperada e recusa qualquer outra. Isso será usado quando a conexão for por ID.

Limitações atuais, declaradas:

- A chave privada fica em disco sem cifragem, protegida só pelas permissões do perfil do usuário.
- Ainda não há lista de dispositivos confiáveis nem acesso não supervisionado. Quando houver, será por senha com PAKE mais fixação de impressão digital, nunca por "aceitar tudo".
- A CLI tem `--auto-accept-loopback` para testes. Ela força a escuta em `127.0.0.1`, então não abre o computador para a rede.

## Protocolo de sessão

Uma conexão QUIC, dois canais:

1. **Controle** (stream bidirecional, aberto pelo visualizador). Mensagens `postcard` prefixadas por tamanho.
   `Hello` → `Welcome` → *(pessoa decide)* → `Decision` → `Input*` / `RequestKeyframe` / `Bye`.
2. **Vídeo** (stream unidirecional, aberto pelo host após autorizar). Pacotes binários com retângulos a aplicar sobre o quadro anterior.

Posições do mouse viajam normalizadas (`0..=65535` sobre o monitor compartilhado) e teclas viajam como scancode físico, de modo que resolução, escala e layout de teclado de quem vê não interferem.

## Codec de tela ("tiles v1")

A tela é dividida em blocos de 64×64. A cada quadro o host compara com o último quadro **enviado** e transmite só o que mudou, em células de até 256×256.

- Cada célula tenta primeiro **RGB + zstd, sem perdas**. Texto e interface comprimem muito bem e chegam perfeitos.
- Se não comprimir (foto, vídeo), a célula vai como **JPEG** e fica marcada.
- Quando a tela para, o host **refina**: reenvia sem perdas o que foi como JPEG.

Controle de latência: o caminho captura → codec → rede tem contrapressão de ponta a ponta (canal de capacidade 1 e janela de envio QUIC pequena). Se a rede não escoa, a thread de captura espera e as mudanças se acumulam num único quadro seguinte. A taxa de quadros cai; o atraso não cresce.

Este codec é o ponto de partida, não o destino. Ele é excelente para trabalho de escritório e fraco para vídeo em tela cheia. O próximo passo é H.264/AV1 por hardware (Media Foundation no Windows) atrás da mesma interface, mantendo o "tiles" como alternativa sem perdas.

## O que aprendemos observando o AnyDesk

O AnyDesk foi usado como **referência de arquitetura, não de código**. A análise se limitou ao que qualquer usuário vê na própria máquina: os nomes de módulos no log do serviço, as chaves dos arquivos de configuração e os metadados do executável. O executável é um carregador com o programa real cifrado dentro; ele **não foi descompactado nem descompilado**, para manter o OpenDesk um projeto independente ("clean room") e livre para usar a licença Apache-2.0.

| No AnyDesk (observado) | A ideia | No OpenDesk |
|---|---|---|
| `anynet.punch_connector`: tenta vários endereços candidatos em paralelo | Perfuração de NAT com candidatos | **Roteiro**: mesma ideia sobre o socket QUIC único de `opendesk-net` |
| `anynet.relay_connector`, `main_relay_conn`: TLS até um relay da empresa | Relay quando o P2P falha | **Roteiro**: relay opcional e auto-hospedado; o tráfego continua cifrado ponta a ponta |
| `anynet.connection_mgr`: "Found N direct connection candidates" | Trocar candidatos locais pelo servidor | **Roteiro**: servidor de encontro (Cloudflare Workers + Durable Objects) |
| `anynet.tcp_acceptor` + `os_win.upnp_config` | Escuta direta e mapeamento UPnP | Escuta direta **feita** (UDP 47700); UPnP no roteiro |
| `app.discovery`: grupo multicast | Achar computadores na rede local | **Roteiro**: descoberta por multicast |
| `ad.anynet.pkey` / `cert` / `fpr` em `service.conf` | Identidade = par de chaves por dispositivo | **Feito**: `opendesk-net::identity` |
| `handshake`: "Using protocol version N" | Versão negociada no início | **Feito**: `PROTOCOL_VERSION` no `Hello` |
| `app.session`: "Session features: deskrt, clipboard, chat, audio, input, tcp_tunnel" | Sessão = conjunto de canais opcionais | Vídeo e entrada **feitos**; área de transferência, arquivos, áudio e túnel no roteiro, um stream QUIC por recurso |
| `anynet.client_packetizer`: "Adjusted send buffer" | Ajustar o envio à rede | **Feito** de forma simples (contrapressão); adaptação de qualidade no roteiro |
| Processo `--service` + `fiber.ipc_namespace`, "frontend" | Serviço SYSTEM separado da interface | **Roteiro**: necessário para UAC, tela de logon e acesso não supervisionado |
| `ad.security.permission_profiles.*` | Perfis de permissão | Permissões por sessão **feitas**; perfis no roteiro |
| `win_app.firewall_mgr` | Regra de firewall na instalação | **Roteiro** (hoje o Windows pergunta na primeira execução) |
| `ad.roster.*`, `thumbnails/` | Catálogo de endereços com miniaturas | **Roteiro** |
| `app.auto_update`, `ad.telemetry.*`, `ad.license.*` | Atualização, telemetria, licença | Atualização no roteiro; **telemetria e licenciamento não existirão** |

Onde o OpenDesk diverge de propósito: QUIC em vez de protocolo próprio sobre TCP/UDP (cifragem, multiplexação e controle de congestionamento prontos e auditados), e nenhum servidor da "empresa" no caminho dos dados.

## Roteiro

1. **Conexão pela internet**: servidor de encontro por ID, troca de candidatos, perfuração de NAT, relay auto-hospedado.
2. **Experiência**: área de transferência, transferência de arquivos, múltiplos monitores, cursor remoto, codec de vídeo por hardware, qualidade adaptativa.
3. **Segurança**: dispositivos confiáveis, chave protegida por DPAPI, registro local de sessões.
4. **Serviço Windows**: UAC e tela de logon, acesso não supervisionado com senha (PAKE).
5. **Distribuição**: builds automáticos, assinatura de código (SignPath para projetos abertos), Linux e macOS.

## Limitações conhecidas

- Só Windows, e só conexão direta por endereço (mesma rede ou porta encaminhada).
- Janelas elevadas e o prompt do UAC não recebem entrada nem são capturados (exige o serviço).
- Monitores rotacionados não são corrigidos; só o monitor principal é compartilhado pela interface.
- O ponteiro do mouse remoto não é desenhado; o cursor local faz esse papel.
- Reduzir uma tela 4K numa janela pequena usa filtro bilinear simples, sem mipmaps.
