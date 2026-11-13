# Arquitetura do OpenDesk

O OpenDesk é um aplicativo de acesso remoto ponto a ponto. Este documento explica como ele é organizado, por que foi feito assim e o que ainda falta.

## Princípios

1. **Ninguém entra sem ser convidado.** Toda sessão passa por uma decisão explícita de quem está no computador. Não existe modo oculto, e a sessão ativa fica sempre anunciada na tela.
2. **Ponto a ponto por padrão.** Imagem, mouse e teclado trafegam direto entre os dois computadores, cifrados de ponta a ponta. Servidores só ajudam os pares a se encontrar e nunca veem o conteúdo.
3. **Custo zero para operar.** Nada no caminho crítico depende de infraestrutura paga pelo projeto. Relay é opcional e hospedado por quem precisa.
4. **Um executável pequeno.** Tudo em Rust, sem runtime nem instalador obrigatório.
5. **Transparência.** O aplicativo mostra por onde a sessão passa, a latência, o codec e as permissões concedidas. Nada de configuração de rede na tela principal, mas nada escondido.

O OpenDesk não é "o RustDesk, só que P2P": o RustDesk também conecta ponto a ponto e usa servidores só para encontro e relay. A aposta aqui é outra: ser o mais simples de usar, deixar claro o que está acontecendo com a conexão, e tratar a infraestrutura (encontro e relay) como algo aberto, que qualquer pessoa pode hospedar.

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
| `opendesk-net` | Identidade, transporte QUIC, STUN, perfuração de NAT, cliente do servidor de encontro, hosts conhecidos | Não |
| `opendesk-platform` | Captura de tela, injeção de entrada, captura de teclado | **Sim** (só Windows hoje) |
| `opendesk-session` | Máquinas de estado do host e do visualizador | Não |
| `apps/opendesk` | Interface gráfica (egui/eframe sobre wgpu) | Pouco |
| `apps/opendesk-cli` | Diagnóstico e teste ponta a ponta sem interface | Não |
| `rendezvous/` | Servidor de encontro (Cloudflare Worker + Durable Objects, TypeScript) | — |

Portar o OpenDesk para outro sistema é implementar `opendesk-platform` para ele.

## Identidade e segurança

Na primeira execução o dispositivo gera um par de chaves e um certificado autoassinado. A **impressão digital** (SHA-256 do certificado) é a identidade do dispositivo. O **ID de 9 dígitos** é só um apelido derivado dela, para facilitar a conferência por voz; ele não autentica nada.

Toda conexão é QUIC com TLS 1.3 e autenticação mútua. Não há autoridade certificadora: o verificador aceita qualquer certificado, mas confere a assinatura do handshake, então cada lado tem a prova de que o outro possui a chave privada daquela impressão digital. Decidir se aquela identidade é confiável cabe à camada de cima:

- **No host**, uma pessoa vê nome, ID, impressão digital e endereço de quem pede, e escolhe entre recusar, permitir só ver ou permitir controle. O host nunca concede mais do que foi pedido, e confere a permissão a cada evento de entrada recebido (um visualizador modificado não ganha controle por insistir).
- **No visualizador**, a conexão por ID só é aceita se o host apresentar, no handshake, a impressão digital informada pelo servidor de encontro. E essa impressão é **fixada no primeiro contato** (`known_hosts`, como no SSH): se o mesmo ID aparecer depois com outra identidade, a conexão é recusada com um aviso. Um ID de 9 dígitos sozinho não identifica ninguém com segurança; a fixação é o que impede o servidor, ou alguém que fabrique um certificado com o mesmo ID, de se passar por um computador já conhecido.

Limitações atuais, declaradas:

- A chave privada fica em disco sem cifragem, protegida só pelas permissões do perfil do usuário.
- No primeiro contato com um ID ainda vale a confiança no servidor de encontro (e a conferência visual da impressão digital, que o aplicativo mostra). Não há interface para "esquecer" um host fixado; por ora é apagar a linha em `known_hosts`.
- Ainda não há lista de dispositivos autorizados no host nem acesso não supervisionado. Quando houver, será por senha com PAKE mais fixação de impressão digital, nunca por "aceitar tudo".
- A CLI tem `--auto-accept-loopback` para testes. Ela força a escuta em `127.0.0.1`, então não abre o computador para a rede.

## Conexão por ID

Conectar por endereço exige estar na mesma rede. Para conectar digitando só um ID, três peças trabalham juntas.

**1. Servidor de encontro** (`rendezvous/`). Um Durable Object por ID. O dispositivo dono mantém um WebSocket aberto em `/v1/listen/<id>`; quem quer conectar abre um em `/v1/dial/<id>` e entrega os seus endereços. O servidor repassa o pedido e devolve os endereços do dono. É só isso: nenhum byte da sessão passa por ele.

- **Registro com prova de posse.** O dispositivo envia o certificado e assina um desafio do servidor com a chave privada. O servidor confere que o certificado dá aquele ID e que a assinatura é válida. O primeiro dispositivo a registrar um ID fica dono dele (a impressão digital é gravada); outro certificado que dê o mesmo ID é recusado e o dispositivo parte para um ID alternativo, derivado da mesma impressão digital.
- **Custo.** Cabe no plano gratuito da Cloudflare. Manter um dispositivo online usa WebSocket com hibernação (os *pings* são respondidos pela plataforma sem acordar o objeto), então o consumo é proporcional às conexões feitas, não ao tempo online.
- **Limites contra abuso.** Mensagens de até 4 KB, até 8 endereços por pedido, 20 pedidos por minuto por ID, e pedidos sem resposta expiram em 15 s.
- **Sem confiança.** O servidor pode mentir sobre endereços (a conexão simplesmente falha) e sobre a impressão digital (o handshake e a fixação pegam). Ele fica sabendo quais IDs estão online, de que IP e quem procurou quem; não fica sabendo o que acontece na sessão.

**2. Endereço público por STUN.** O nó pergunta a dois servidores STUN públicos como o seu socket aparece na internet. A pergunta sai **do mesmo socket UDP do QUIC** (um invólucro sobre o socket do quinn separa STUN e pacotes de perfuração do tráfego QUIC), porque é o mapeamento desse socket no roteador que importa. Duas respostas diferentes indicam NAT simétrico, que o aplicativo avisa na tela.

**3. Perfuração de NAT.** Cada lado troca com o outro uma lista de candidatos: o endereço na rede local e o público. O host começa a enviar pacotes mínimos para os candidatos do visualizador (abrindo o caminho de volta no seu roteador) e o visualizador tenta todos os candidatos do host ao mesmo tempo; o primeiro handshake que completar vence. Dois computadores na mesma rede se acham pelo endereço local; em redes diferentes, pelo público.

```
Visualizador                 Servidor de encontro                    Host
     │  STUN → endereço público                                        │ (registrado, WebSocket aberto)
     │── dial <id> + candidatos ──────────▶│── offer + candidatos ────▶│
     │                                     │                           │ STUN; começa a perfurar
     │◀── answer: impressão + candidatos ──│◀── answer + candidatos ───│
     │ confere a impressão fixada                                      │
     │══ QUIC para todos os candidatos, em paralelo ══════════════════▶│
     │◀═════════════ sessão direta, cifrada de ponta a ponta ═════════▶│ (pessoa aprova)
```

O que isso **não** resolve: NAT simétrico dos dois lados, redes que bloqueiam UDP e alguns CGNAT. Para esses casos é preciso relay.

### Relay (projetado, ainda não implementado)

Como a sessão é QUIC com TLS de ponta a ponta entre os dois dispositivos, o relay pode ser um encaminhador de datagramas UDP burro: ele recebe pacotes de um par e os repassa ao outro, sem conseguir ler nem alterar nada. Para o QUIC, o endereço do relay simplesmente passa a ser "o endereço do outro lado". O plano:

- um binário pequeno (`opendesk-relay`) que qualquer pessoa roda numa máquina com IP público;
- o host indica um relay na resposta do encontro, com um token de sessão; os dois lados se apresentam ao relay com o token e ele passa a encaminhar entre os dois endereços;
- o visualizador tenta os candidatos diretos primeiro e só cai no relay se nenhum responder; a barra da sessão mostra "Relay" em vez de "Direta";
- **relays comunitários**: uma lista pública de relays voluntários que o aplicativo pode usar. Isso exige o que ainda não existe: limite de banda e de sessões por relay, tokens emitidos pelo servidor de encontro para que só sessões legítimas usem o relay, e medição de saúde para escolher um relay bom. Sem esses controles, um relay aberto vira banda grátis para qualquer coisa.

Enquanto o relay não existir, o OpenDesk não promete conexão em qualquer rede.

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

Este codec é o ponto de partida, não o destino. Ele é excelente para trabalho de escritório e **ruim para vídeo e jogos em tela cheia**: medido com um jogo rodando em 4K, ficou em ~26 quadros por segundo consumindo ~260 Mbit/s, o que só serve em rede local. Para competir em fluidez o caminho é:

1. H.264/AV1 codificado na GPU (Media Foundation no Windows), sem tirar o quadro da GPU, atrás da mesma interface de codec;
2. qualidade e taxa de quadros adaptadas à banda medida, com teto de bitrate;
3. redução de resolução quando a janela de quem vê é menor que a tela remota;
4. o "tiles" mantido como modo sem perdas para texto.

Até lá, a barra da sessão mostra codec, resolução, quadros por segundo e bitrate reais, para que o desempenho seja visível em vez de prometido.

## O que aprendemos observando o AnyDesk

O AnyDesk foi usado como **referência de arquitetura, não de código**. A análise se limitou ao que qualquer usuário vê na própria máquina: os nomes de módulos no log do serviço, as chaves dos arquivos de configuração e os metadados do executável. O executável é um carregador com o programa real cifrado dentro; ele **não foi descompactado nem descompilado**, para manter o OpenDesk um projeto independente ("clean room") e livre para usar a licença Apache-2.0.

| No AnyDesk (observado) | A ideia | No OpenDesk |
|---|---|---|
| `anynet.punch_connector`: tenta vários endereços candidatos em paralelo | Perfuração de NAT com candidatos | **Feito**: `Node::punch` + `Node::connect_any` sobre o socket QUIC único |
| `anynet.relay_connector`, `main_relay_conn`: TLS até um relay da empresa | Relay quando o P2P falha | **Projetado**: relay UDP auto-hospedado e comunitário; o tráfego continua cifrado ponta a ponta |
| `anynet.connection_mgr`: "Found N direct connection candidates" | Trocar candidatos locais pelo servidor | **Feito**: servidor de encontro (Cloudflare Workers + Durable Objects) |
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

1. **Publicar o servidor de encontro** e fixar o endereço padrão no aplicativo. Hoje é preciso apontar para um servidor próprio com `OPENDESK_RENDEZVOUS`.
2. **Relay**: `opendesk-relay` auto-hospedado, depois relays comunitários com controle de abuso.
3. **Desempenho**: codec de vídeo na GPU, qualidade adaptativa, teto de bitrate.
4. **Experiência**: área de transferência, transferência de arquivos, múltiplos monitores, cursor remoto, catálogo de computadores recentes.
5. **Segurança**: gerenciar hosts fixados pela interface, chave protegida por DPAPI, registro local de sessões.
6. **Serviço Windows**: UAC e tela de logon, acesso não supervisionado com senha (PAKE).
7. **Distribuição**: builds automáticos, assinatura de código (SignPath para projetos abertos), Linux e macOS.

## Limitações conhecidas

- Só Windows.
- Conexão por ID exige um servidor de encontro configurado, e ainda não há um público do projeto.
- Sem relay: se os roteadores dos dois lados não permitirem perfuração (NAT simétrico, UDP bloqueado), a conexão pela internet falha. A perfuração entre duas redes diferentes ainda não foi testada em campo, só o encontro e a conexão na mesma máquina e a descoberta do endereço público.
- IPv6 não é usado.
- Janelas elevadas e o prompt do UAC não recebem entrada nem são capturados (exige o serviço).
- Monitores rotacionados não são corrigidos; só o monitor principal é compartilhado pela interface.
- O ponteiro do mouse remoto não é desenhado; o cursor local faz esse papel.
- Reduzir uma tela 4K numa janela pequena usa filtro bilinear simples, sem mipmaps.
