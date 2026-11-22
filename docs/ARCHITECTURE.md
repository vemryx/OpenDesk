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
| `opendesk-platform` | Captura de tela, injeção de entrada, captura de teclado, e o serviço e o agente do controle total | **Sim** (só Windows hoje) |
| `opendesk-session` | Máquinas de estado do host e do visualizador | Não |
| `apps/opendesk` | Interface gráfica (egui/eframe sobre wgpu) | Pouco |
| `apps/opendesk-cli` | Diagnóstico e teste ponta a ponta sem interface | Não |
| `opendesk-relay` | Relay UDP auto-hospedado, para quando a conexão direta falha | Não |
| `rendezvous/` | Servidor de encontro (Cloudflare Worker + Durable Objects, TypeScript) | — |

Portar o OpenDesk para outro sistema é implementar `opendesk-platform` para ele.

## Aparência

A interface é neutra e compacta: cinzas levemente frios, branco suave em vez de branco puro e o azul da marca só na ação principal. As cores, a tipografia (Segoe UI do sistema, sem fontes embutidas) e os componentes ficam em `apps/opendesk/src/theme.rs`; os pares de texto e fundo dos dois temas passam de 4,5:1 de contraste.

A identidade oficial está em `assets/brand/`. O símbolo é desenhado em vetor pelo próprio app; o nome "OpenDesk" é uma máscara em tons de cinza embutida no executável e tingida com a cor do tema. `assets/brand/make_assets.py` regenera essa máscara e o ícone (`apps/opendesk/assets/`), que o `build.rs` embute no `.exe` no Windows.

Para conferir telas sem um segundo computador, builds de desenvolvimento aceitam `OPENDESK_PREVIEW` com uma lista de estados: `pedido`, `acesso`, `aviso`, `detalhes`, `recentes`, `historico`, `config`, `config-seguranca`, `config-rede`, `config-historico`, `config-sobre`.

## Configurações e dados locais

Tudo o que o app guarda fica na pasta de dados do usuário, em texto legível (`apps/opendesk/src/config.rs`):

| Arquivo | Conteúdo |
|---|---|
| `settings.json` | Configurações. Campos ausentes valem o padrão; arquivo corrompido não impede o app de abrir. |
| `recent.json` | Os oito computadores acessados por último. |
| `history.jsonl` | Registro de sessões, só acrescentado: uma linha JSON por sessão feita ou recebida, por pedido recusado (pela pessoa ou pela política), por senha errada e por arquivo trocado. |
| `unattended.json` | A senha de acesso não supervisionado, na forma derivada (ver abaixo). |
| `known_hosts` | Impressões digitais fixadas (ver abaixo). |
| `identity.*.der` | A chave e o certificado do dispositivo. |

As políticas de entrada são aplicadas no próprio app, antes de qualquer diálogo: com "aceitar pedidos" desligado o pedido é recusado e registrado; com "permitir controle" desligado só é possível autorizar a visualização. Servidor de encontro e relay trocam na hora (a presença é reiniciada); a porta vale ao reabrir. "Abrir com o Windows" grava a chave `Run` do usuário no registro, sem exigir administrador. Com o controle total instalado, ela aponta sempre para o OpenDesk instalado, o único que o agente atende: o app corrige o destino ao abrir, ao instalar e ao remover, sem nunca ligar ou desligar a opção por conta própria (salvo ao remover o próprio OpenDesk instalado, que deixaria a chave apontando para um arquivo apagado). Variáveis de ambiente têm prioridade sobre o arquivo, para implantações.

## Identidade e segurança

Na primeira execução o dispositivo gera um par de chaves e um certificado autoassinado. A **impressão digital** (SHA-256 do certificado) é a identidade do dispositivo. O **ID de 9 dígitos** é só um apelido derivado dela, para facilitar a conferência por voz; ele não autentica nada.

Toda conexão é QUIC com TLS 1.3 e autenticação mútua. Não há autoridade certificadora: o verificador aceita qualquer certificado, mas confere a assinatura do handshake, então cada lado tem a prova de que o outro possui a chave privada daquela impressão digital. Decidir se aquela identidade é confiável cabe à camada de cima:

- **No host**, uma pessoa vê nome, ID, impressão digital e endereço de quem pede, e escolhe entre recusar, permitir só ver ou permitir controle. O host nunca concede mais do que foi pedido, e confere a permissão a cada evento de entrada recebido (um visualizador modificado não ganha controle por insistir).
- **No visualizador**, a conexão por ID só é aceita se o host apresentar, no handshake, a impressão digital informada pelo servidor de encontro. E essa impressão é **fixada no primeiro contato** (`known_hosts`, como no SSH): se o mesmo ID aparecer depois com outra identidade, a conexão é recusada com um aviso. Um ID de 9 dígitos sozinho não identifica ninguém com segurança; a fixação é o que impede o servidor, ou alguém que fabrique um certificado com o mesmo ID, de se passar por um computador já conhecido.

Limitações atuais, declaradas:

- A chave privada fica em disco sem cifragem, protegida só pelas permissões do perfil do usuário.
- No primeiro contato com um ID ainda vale a confiança no servidor de encontro (e a conferência visual da impressão digital, que o aplicativo mostra). Não há interface para "esquecer" um host fixado; por ora é apagar a linha em `known_hosts`.
- Ainda não há lista de dispositivos autorizados no host: quem sabe a senha de acesso não supervisionado entra de qualquer dispositivo.
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

### Relay

Para quando a conexão direta não passa. Como a sessão é QUIC com TLS de ponta a ponta entre os dois dispositivos, o relay é um encaminhador de datagramas UDP burro: recebe pacotes de um par e os repassa ao outro, sem chave nenhuma, sem conseguir ler nem alterar nada. Ele enxerga endereços e volume de tráfego, e só.

- **`opendesk-relay`** (`crates/opendesk-relay`) é um binário pequeno que qualquer pessoa roda numa máquina com IP público e uma porta UDP aberta (47800 por padrão). Compila para Windows e Linux.
- **Envelope.** Cada pacote vai como `0x02 | token de 16 bytes | pacote QUIC`. O relay junta os dois primeiros endereços que aparecerem com o mesmo token e repassa entre eles.
- **Combinação.** Basta **um** dos lados ter um relay configurado (`OPENDESK_RELAY=host:porta`). O visualizador sugere o seu no pedido; o host usa o próprio se tiver, senão aceita o sugerido, e sorteia o token. A escolha volta na resposta do encontro.
- **Transparente para o QUIC.** No socket do nó, um par alcançado por relay ganha um endereço virtual (faixa reservada 240.0.0.0/4). Pacotes para esse endereço são embrulhados e mandados ao relay; os que chegam do relay são desembrulhados e entregues como vindos do endereço virtual. O resto do código não muda.
- **Direto primeiro.** O visualizador tenta os candidatos diretos por 4 s e só então usa o relay. A barra da sessão mostra "Via relay" quando é o caso.
- **Limites contra abuso** no relay: sessões simultâneas (256), banda por sessão (20 Mbit/s), sessões novas por IP por minuto (30) e expiração por inatividade (30 s). Tudo ajustável por parâmetro.

O que ainda não existe são os **relays comunitários**: uma lista pública de relays voluntários que o aplicativo use sozinho. Para isso faltam tokens emitidos pelo servidor de encontro (hoje qualquer par que conheça o endereço de um relay pode usá-lo, dentro dos limites), medição de saúde para escolher um relay bom, e um jeito de publicar e retirar relays da lista. Enquanto isso não existir, o relay é "traga o seu", e o OpenDesk não promete conexão em qualquer rede sem ele.

## Transferência de arquivos

Cada arquivo viaja num stream QUIC bidirecional próprio, aberto por quem envia: não trava o controle nem o vídeo e herda a cifragem de ponta a ponta da sessão.

```
quem envia                      quem recebe
  Offer { nome, tamanho }  ──▶
                           ◀──  Accept | Refuse
  bytes… (fim do stream)   ──▶
                           ◀──  Saved | Failed
```

- **Permissão própria** (`files`), que só existe em acessos com controle e pode ser desligada nas configurações. Sem ela, nada é enviado nem aceito, nos dois sentidos.
- **Quem decide.** O host aceita sem perguntar o que vem de quem já o controla (é o que permite usar com acesso não supervisionado). No sentido contrário, quem acessa confirma arquivo a arquivo: o computador remoto não grava nada no disco de quem o está vendo sem um "Receber".
- **O nome vem da rede e não é confiável.** Quem recebe tira qualquer caminho, troca caracteres proibidos, neutraliza nomes reservados do Windows e limita o tamanho. O arquivo é gravado só na pasta de recebidos (`Downloads\OpenDesk`, ou `OPENDESK_FILES_DIR`), com um sufixo temporário, e só ganha o nome final depois de chegar inteiro; um nome repetido vira `nome (1).ext` em vez de sobrescrever.
- **Limites.** 16 GB por arquivo; o tamanho anunciado é conferido nos dois lados, e o que chega a mais ou a menos é descartado.

O código está em `crates/opendesk-session/src/files.rs`.

## Acesso não supervisionado

Vem desligado. Ligado, quem souber a senha de acesso entra sem que alguém aprove; o pedido aparece na tela do mesmo jeito, e a faixa de "alguém está acessando" diz que a entrada foi pela senha.

- **A senha nunca trafega.** Os dois lados rodam SPAKE2 (um PAKE) sobre a senha esticada com PBKDF2-HMAC-SHA256 (600 mil iterações, sal por host). Quem não sabe a senha ganha um palpite por tentativa, e quem observa ou intermedeia a troca não leva nada que permita testar senhas por conta própria.
- **Amarrada às identidades.** As impressões digitais TLS dos dois dispositivos entram como identidades do SPAKE2. Um intermediário com outro certificado faz as chaves divergirem, mesmo repassando as mensagens intactas. Há confirmação nos dois sentidos: o host só responde depois de conferir a prova do visualizador, e o visualizador confere que o host também conhece a senha.
- **Tentativas limitadas.** Três erros por conexão; cinco erros em dez minutos, somando todos os dispositivos, e o host para de aceitar senha por dez minutos. Cada erro vai para o histórico.
- **O que fica no disco.** O sal e a senha esticada, em `unattended.json`, separado das configurações. Não revela a senha digitada, mas é equivalente a ela para este host: quem copia esse arquivo consegue entrar. Protegê-lo com DPAPI está no roteiro, junto com a chave do dispositivo.
- **A senha não passa por cima das políticas.** Com "aceitar pedidos" desligado a senha não vale; com "permitir controle" desligado ela só libera a visualização.

O código está em `crates/opendesk-session/src/auth.rs`, com testes para senha certa, errada, troca repassada por um intermediário e o bloqueio por tentativas.

## Controle total

O Windows nega duas coisas a um programa comum, por mais permissão que a pessoa tenha dado dentro do OpenDesk:

- **Clicar e digitar em janelas de administrador.** O Gerenciador de Tarefas, um instalador, um terminal elevado: a entrada injetada por um processo de integridade menor é descartada em silêncio (UIPI). A tela aparece, o clique não chega.
- **Ver e tocar a área de trabalho segura.** O aviso "Deseja permitir que este aplicativo faça alterações?" (UAC) e a tela de bloqueio ficam em outra área de trabalho, que só o SYSTEM abre. Quem acessa vê a imagem congelar.

É o motivo mais comum de um programa de acesso remoto "não clicar". A saída é a mesma em todos eles: um processo SYSTEM dentro da sessão do usuário. No OpenDesk ele é opcional, vem desligado e é instalado em Configurações, Segurança, **Controle total**.

```
 serviço OpenDesk (SYSTEM, sessão 0)
   │  mantém um agente na sessão de quem está no computador
   ▼
 agente (SYSTEM, sessão do usuário)          aplicativo (usuário, sem privilégio)
   captura de tela ── memória compartilhada ──▶ codec ──▶ rede
   injeção de entrada ◀──── pipe nomeado ────── eventos do visualizador
```

- **O serviço não faz mais nada.** Não abre porta, não fala com a rede e não guarda dados: a cada segundo confere quem está no console e mantém um agente nessa sessão, trocando-o quando muda o usuário. Sem ninguém logado, não há agente.
- **O agente só captura e injeta.** Rede, chaves, autorização, codec e interface continuam no aplicativo, sem privilégio. O agente segue a área de trabalho ativa: quando o Windows troca para a do UAC ou a de bloqueio, a thread se muda para lá e refaz a duplicação da tela.
- **Quadros por memória compartilhada.** Um quadro 4K tem 33 MB. O agente copia da GPU direto para a memória que o aplicativo lê; pelo pipe só passam os pedidos e o tamanho do quadro. Captura e entrada usam conexões separadas, para um clique nunca esperar um quadro.
- **Sem agente, nada muda.** O aplicativo tenta o agente ao começar uma sessão e, se não houver (ou se ele cair no meio), captura e injeta por conta própria, com os limites acima. O host informa isso a quem acessa, e a barra da sessão mostra "Controle limitado" em vez de deixar a pessoa clicar no vazio.

Um processo SYSTEM que aceita pedidos de um programa comum é uma porta de elevação de privilégio se for mal fechada. O que a fecha:

- **Pasta protegida.** Instalar copia o executável para `Arquivos de Programas\OpenDesk`, onde só administradores escrevem. O serviço e o agente rodam de lá, e o agente só atende um processo cujo executável seja **esse mesmo arquivo**. O OpenDesk aberto de outra pasta continua funcionando, sem o controle total.
- **Prova dentro do processo.** O número de processo informado por um pipe não é prova suficiente. O agente cria um segredo, entrega um handle dele **dentro** do processo que conferiu e exige o segredo de volta. Os quadros seguem o mesmo caminho: a memória é duplicada para o processo conferido, não enviada pela conexão.
- **Pipe restrito.** Só o SYSTEM e o usuário daquela sessão abrem o pipe, que recusa clientes remotos. O aplicativo, por sua vez, só confia num pipe cujo dono seja o SYSTEM: outro programa não consegue se passar pelo agente para ler as teclas da sessão.
- **Registro em lugar seguro.** O serviço e o agente escrevem o log ao lado do executável instalado, e não numa pasta em que um usuário comum pudesse plantar um atalho.

O que isso não impede, declarado: um programa rodando como o mesmo usuário que consiga injetar código no próprio OpenDesk instalado herda o acesso ao agente, e com ele o poder de clicar num aviso de permissão. É o limite de todo programa de acesso remoto cuja interface roda como o usuário, e a razão de o controle total ser uma escolha de quem instala.

O código está em `crates/opendesk-platform/src/windows/agent.rs` e `service.rs`. Os testes do agente (`tests/agent.rs`) exercitam o protocolo, a prova de identidade, a memória compartilhada, a captura e a entrada sem privilégio.

O que roda como SYSTEM só se confere com o serviço instalado. O teste é uma sessão local contra o OpenDesk instalado, com `opendesk-cli view <endereço> --control` e as opções `--click X,Y`, `--press` e `--after`. Conferido assim, num Windows 11: o clique numa janela de administrador (sem o serviço o cursor nem se move); o aviso do UAC que surge no meio da sessão, respondido com o mouse e com o teclado; e a tela de bloqueio, tanto ao bloquear durante a sessão quanto ao começar a sessão com o computador já bloqueado. No Windows 11 a cortina de bloqueio fica na área de trabalho comum e só o pedido de PIN ou senha vai para a área segura; o agente acompanha as duas trocas. O serviço e o agente registram o que fazem em `opendesk-service.log`, ao lado do executável instalado.

## Protocolo de sessão

Uma conexão QUIC, dois canais:

1. **Controle** (stream bidirecional, aberto pelo visualizador). Mensagens `postcard` prefixadas por tamanho.
   `Hello` → `Welcome` → *(a pessoa decide, ou o visualizador prova a senha: `Auth` ⇄ `Auth`, `AuthConfirm` → `AuthOk`)* → `Decision` → sessão. A decisão leva as permissões concedidas, o monitor mostrado e se o host tem o controle total.
   Durante a sessão: `Input`, `RequestKeyframe`, `Clipboard` (nos dois sentidos, só texto, até 100 kB), `SelectDisplay` → `Display`, `Lock`, `Seen` (cada quadro confirmado por quem vê; ver Atraso), `Bye`.
2. **Vídeo** (stream unidirecional, aberto pelo host após autorizar). Pacotes binários com retângulos a aplicar sobre o quadro anterior.

As permissões são conferidas no host: entrada e bloqueio remoto exigem a permissão de controle, e a área de transferência só é lida ou gravada com a permissão correspondente (só o que for copiado depois de a sessão começar atravessa). Trocar de monitor reinicia a captura; os quadros da captura anterior que ainda estiverem a caminho são descartados por um número de geração.

Posições do mouse viajam normalizadas (`0..=65535` sobre o monitor compartilhado) e teclas viajam como scancode físico, de modo que resolução, escala e layout de teclado de quem vê não interferem.

## Codec de tela ("tiles v1")

A tela é dividida em blocos de 64×64. A cada quadro o host compara com o último quadro **enviado** e transmite só o que mudou, em células de até 256×256.

- Cada célula tenta primeiro **RGB + zstd, sem perdas**. Texto e interface comprimem muito bem e chegam perfeitos.
- Se não comprimir (foto, vídeo), a célula vai como **JPEG** e fica marcada.
- Quando a tela para, o host **refina**: reenvia sem perdas o que foi como JPEG.

- Quando a tela muda mais do que a conexão escoa, o quadro recebe um **alvo de bytes** e as células descem uma escada: JPEG mais forte, depois metade, um quarto e um oitavo da resolução (quem vê amplia de volta). O espaço que sobrar devolve a versão boa às células em que ela custa menos, então é a foto que borra antes do texto. O refinamento, em passos do mesmo tamanho, devolve a nitidez assim que a tela para.

Este codec é o ponto de partida, não o destino. Ele é excelente para trabalho de escritório e, com o alvo de bytes, continua respondendo rápido em qualquer conexão; o que ele não faz é vídeo em tela cheia com nitidez em pouca banda. Para isso o caminho é:

1. H.264/AV1 codificado na GPU (Media Foundation no Windows), sem tirar o quadro da GPU, atrás da mesma interface de codec;
2. redução de resolução quando a janela de quem vê é menor que a tela remota;
3. o "tiles" mantido como modo sem perdas para texto.

A barra da sessão mostra codec, resolução, quadros por segundo e bitrate reais, para que o desempenho seja visível em vez de prometido.

## Atraso

O atraso de um acesso remoto quase nunca está na captura ou no codec: está em **filas**. Um host que codifica mais rápido do que a conexão escoa empilha quadros prontos, e cada um já está velho quando chega. Foi o que se mediu aqui antes de corrigir: com a tela inteira mudando, 1,1 s entre o clique e a resposta numa conexão de 20 Mbit/s, e 8,9 s numa de 5 Mbit/s com 1% de perda. Havia até 2 MB esperando na fila do QUIC, mais dois ou três quadros já codificados atrás dela, e nada que ajustasse o tamanho do quadro à banda.

O OpenDesk não tem essa fila. Quatro peças trabalham juntas (`crates/opendesk-session/src/pacing.rs`):

- **Quadro sob demanda.** A thread de captura só captura e codifica quando a conexão tem caminho livre. Enquanto espera, as mudanças se acumulam na captura e saem todas no quadro seguinte; todo quadro enviado mostra a tela de instantes antes.
- **Confirmação de quem vê.** O visualizador responde a cada quadro com `Seen` assim que ele está na tela, informando quanto tempo gastou decodificando. É por aí que o host sabe o que ainda está a caminho, e quanto a conexão escoa (bytes confirmados sobre o tempo que levaram, descontado o tempo do computador de quem vê).
- **Alvo de bytes.** A vazão medida vira o tamanho do próximo quadro: o que atravessa a conexão em 33 ms. O codec desce a qualidade para caber (ver acima). Quadros pequenos, como um botão mudando de cor, seguem sem esperar ninguém.
- **Controle de congestionamento BBR**, no lugar do Cubic padrão do QUIC, e confirmações em no máximo 5 ms. O Cubic trata toda perda como rede cheia; numa conexão com Wi-Fi, que perde pacote por ruído, ele derruba a banda útil a uma fração da real.

O resultado é uma troca deliberada: numa conexão ruim a imagem borra por um instante, em vez de chegar nítida com segundos de atraso.

### Medidas

Tela 1920×1080, medianas, com a conexão simulada por `opendesk-cli netsim`. "Rolagem" é uma página de texto rolando sem parar: a tela inteira muda a cada quadro, o pior caso.

| Conexão | Conteúdo | Clique até a tela, antes | Depois | Quadros/s, depois |
|---|---|---|---|---|
| 10 Mbit/s, 40 ms de ida e volta | página parada | 50 ms | 48 ms | — |
| 50 Mbit/s, 20 ms | rolagem | 423 ms | 95 ms | 25 |
| 20 Mbit/s, 30 ms | rolagem | 1 095 ms | 91 ms | 26 |
| 20 Mbit/s, 30 ms | vídeo | 720 ms | 81 ms | 28 |
| 5 Mbit/s, 60 ms, 1% de perda | rolagem | 8 907 ms | 135 ms | 17 |
| 5 Mbit/s, 60 ms, 1% de perda | vídeo | 6 882 ms | 119 ms | 18 |
| 2 Mbit/s, 80 ms, 2% de perda | rolagem | não medido | 167 ms | 14 |

Em 3840×2160 com rolagem: 124 ms a 20 Mbit/s e 194 ms a 5 Mbit/s com 1% de perda. Com a tela parada o atraso é o da conexão mais alguns milissegundos, antes e depois: o problema era o movimento.

Para repetir, em três terminais (`--synthetic` troca a tela de verdade por uma de mentira, que carimba a hora em cada quadro e tem um quadrado que muda de cor a cada clique):

```bash
opendesk-cli host --port 47961 --auto-accept-loopback control --synthetic rolagem
opendesk-cli netsim --listen 47962 --to 127.0.0.1:47961 --mbit 5 --delay-ms 30 --loss 1
opendesk-cli view 127.0.0.1:47962 --control --measure 15 --seconds 10
```

`OPENDESK_TRACE_VIDEO=1` no host registra, uma vez por segundo, para onde vai o tempo de cada quadro (espera pela conexão, captura, codec) e a banda medida.

O que as medidas não cobrem: são de uma conexão simulada, com host e visualizador na mesma máquina. O comportamento entre duas casas de verdade ainda não foi medido.

## O que aprendemos observando o AnyDesk

O AnyDesk foi usado como **referência de arquitetura, não de código**. A análise se limitou ao que qualquer usuário vê na própria máquina: os nomes de módulos no log do serviço, as chaves dos arquivos de configuração e os metadados do executável. O executável é um carregador com o programa real cifrado dentro; ele **não foi descompactado nem descompilado**, para manter o OpenDesk um projeto independente ("clean room") e livre para usar a licença Apache-2.0.

| No AnyDesk (observado) | A ideia | No OpenDesk |
|---|---|---|
| `anynet.punch_connector`: tenta vários endereços candidatos em paralelo | Perfuração de NAT com candidatos | **Feito**: `Node::punch` + `Node::connect_any` sobre o socket QUIC único |
| `anynet.relay_connector`, `main_relay_conn`: TLS até um relay da empresa | Relay quando o P2P falha | **Feito**: `opendesk-relay`, auto-hospedado, sem acesso ao conteúdo. Relays comunitários no roteiro |
| `anynet.connection_mgr`: "Found N direct connection candidates" | Trocar candidatos locais pelo servidor | **Feito**: servidor de encontro (Cloudflare Workers + Durable Objects) |
| `anynet.tcp_acceptor` + `os_win.upnp_config` | Escuta direta e mapeamento UPnP | Escuta direta **feita** (UDP 47700); UPnP no roteiro |
| `app.discovery`: grupo multicast | Achar computadores na rede local | **Roteiro**: descoberta por multicast |
| `ad.anynet.pkey` / `cert` / `fpr` em `service.conf` | Identidade = par de chaves por dispositivo | **Feito**: `opendesk-net::identity` |
| `handshake`: "Using protocol version N" | Versão negociada no início | **Feito**: `PROTOCOL_VERSION` no `Hello` |
| `app.session`: "Session features: deskrt, clipboard, chat, audio, input, tcp_tunnel" | Sessão = conjunto de canais opcionais | Vídeo, entrada, área de transferência (texto) e arquivos **feitos**; áudio e túnel no roteiro, um stream QUIC por recurso |
| `anynet.client_packetizer`: "Adjusted send buffer" | Ajustar o envio à rede | **Feito**: quadro sob demanda, vazão medida e qualidade ajustada à banda (ver Atraso) |
| Processo `--service` + `fiber.ipc_namespace`, "frontend" | Serviço SYSTEM separado da interface | **Feito**: serviço que mantém um agente só de captura e entrada (ver Controle total). A tela de logon, antes de alguém entrar, segue no roteiro |
| `ad.security.permission_profiles.*` | Perfis de permissão | Permissões por sessão **feitas**; perfis no roteiro |
| `win_app.firewall_mgr` | Regra de firewall na instalação | **Feito** na instalação do controle total (UDP de entrada, redes privadas e de domínio). Sem instalar, o Windows pergunta na primeira execução |
| `ad.roster.*`, `thumbnails/` | Catálogo de endereços com miniaturas | **Roteiro** |
| `app.auto_update`, `ad.telemetry.*`, `ad.license.*` | Atualização, telemetria, licença | Atualização no roteiro; **telemetria e licenciamento não existirão** |

Onde o OpenDesk diverge de propósito: QUIC em vez de protocolo próprio sobre TCP/UDP (cifragem, multiplexação e controle de congestionamento prontos e auditados), e nenhum servidor da "empresa" no caminho dos dados.

## Roteiro

1. **Relays comunitários**: lista pública, tokens emitidos pelo servidor de encontro, escolha automática.
2. **Desempenho**: codec de vídeo na GPU; resolução reduzida quando a janela de quem vê é menor que a tela remota; mouse por datagrama, para o movimento não esperar a retransmissão de um pacote perdido.
3. **Vários visualizadores** no mesmo host, compartilhando uma única captura de tela.
4. **Experiência**: navegador de arquivos remoto e envio de pastas, área de transferência com imagens e arquivos, cursor remoto, conversa por texto, gravação da sessão, apelidos e favoritos nos computadores recentes.
5. **Segurança**: gerenciar hosts fixados pela interface, chave e senha de acesso protegidas por DPAPI, lista de dispositivos autorizados, segundo fator.
6. **Antes do login**: hoje o serviço só mantém o agente; a rede e a sessão vivem no aplicativo, que só existe depois que alguém entra no Windows. Atender na tela de logon exige levar o host para o serviço. Também falta enviar Ctrl+Alt+Del.
7. **Distribuição**: releases automáticos, assinatura de código (SignPath para projetos abertos), Linux e macOS, domínio próprio para o servidor de encontro.

## Limitações conhecidas

- Só Windows.
- O servidor de encontro padrão é uma instância no plano gratuito da Cloudflare, num endereço `workers.dev`. Serve para começar; um projeto com muitos usuários precisa de domínio próprio e de acompanhar os limites do plano.
- Não há relay público. Se os roteadores dos dois lados não permitirem perfuração (NAT simétrico, UDP bloqueado), a conexão pela internet só acontece com um relay próprio configurado.
- A perfuração entre duas redes diferentes ainda não foi testada em campo. Foram testados o encontro, a conexão direta e a conexão por relay na mesma máquina, e a descoberta do endereço público.
- O host atende um visualizador por vez (o Windows só permite uma captura de tela por processo); um segundo pedido recebe "ocupado".
- IPv6 não é usado.
- Sem o controle total instalado, janelas de administrador não recebem entrada e o aviso do UAC e a tela de bloqueio não são capturados. Quem acessa vê "Controle limitado" na barra.
- O controle total só vale para o OpenDesk aberto a partir de `Arquivos de Programas`, e só atende a sessão do console (não uma sessão de Área de Trabalho Remota).
- Ctrl+Alt+Del não é enviado ao computador remoto.
- Monitores rotacionados não são corrigidos.
- O acesso não supervisionado depende de o OpenDesk estar aberto numa sessão do Windows. Com o controle total ele alcança o computador bloqueado; antes do login, ainda não.
- A área de transferência sincroniza só texto.
- A transferência de arquivos envia arquivos soltos, não pastas, e não retoma um envio interrompido.
- O ponteiro do mouse remoto não é desenhado; o cursor local faz esse papel.
- Reduzir uma tela 4K numa janela pequena usa filtro bilinear simples, sem mipmaps.
