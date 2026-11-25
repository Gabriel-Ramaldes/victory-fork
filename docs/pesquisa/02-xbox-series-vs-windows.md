# Xbox Series X|S × Windows: hardware e software

O que sabemos da plataforma de origem (o console) e da de destino (um PC
com Windows 11). Também explica por que cada diferença custa trabalho ao
VIctory.

Fontes, marcadas em cada item:
- **[pública]**: documentação ou apresentação da Microsoft, ou imprensa
  técnica (links no fim);
- **[código]**: lido no VIctory, que implementa a interface que o jogo
  usa;
- **[inferido]**: dedução nossa, ainda sem confirmação.

Nada aqui vem de SDKs confidenciais. Para o VIctory, o "contrato" do
console é o que o jogo chama e o que os binários contêm.

## 1. Hardware

| | Series X | Series S | Nosso PC de teste |
|---|---|---|---|
| CPU | 8 núcleos Zen 2, 3,8 GHz (3,66 com SMT) [pública] | 8 núcleos Zen 2, 3,6 GHz (3,4 com SMT) [pública] | Ryzen 7 3700X, Zen 2, 8 núcleos |
| GPU | RDNA 2 custom, 52 CUs a 1,825 GHz, 12,15 TFLOPS (chip de 56 CUs) [pública] | RDNA 2, 20 CUs a 1,565 GHz, 4 TFLOPS [pública] | RX 9070 XT, RDNA 4, 16 GB GDDR6 próprios |
| Memória | 16 GB GDDR6 **compartilhada** CPU+GPU: 10 GB a 560 GB/s + 6 GB a 336 GB/s, barramento de 320 bits [pública] | 10 GB GDDR6: 8 GB a 224 GB/s + 2 GB a 56 GB/s [pública] | 32 GB DDR4-3600 (CPU) + 16 GB VRAM (GPU), ligados por PCIe |
| Armazenamento | NVMe custom 1 TB, 2,4 GB/s bruto, ~4,8 GB/s com descompressão em hardware [pública] | NVMe 512 GB ou 1 TB [pública] | NVMe (o jogo foi copiado para `C:\Games`) |
| Processo | TSMC N7 "enhanced" [pública] | n/d | n/d |

O que importa para a emulação:

- **A CPU é a mesma família (Zen 2).** O código x86-64 do jogo roda
  nativo, sem tradução de instruções, inclusive instruções específicas da
  AMD como `clzero`, `rdpru` e `mwaitx` [código/medido na fase 2]. Não é
  essa a parte lenta.
- **A GPU é de outra geração.** RDNA 2 no console, RDNA 4 aqui. A ISA
  muda, e o PC nem expõe ISA nativa: tudo passa por DXIL e pelo
  compilador do driver.
- **A diferença decisiva é a memória.**
  - No console é uma só memória física; CPU e GPU usam os mesmos
    endereços, e o jogo foi escrito assumindo isso. Comentário do
    `xmem.rs`: "the GPU side works from CPU addresses (unified memory)"
    [código].
  - No PC, a VRAM é separada e a CPU só a alcança pelo PCIe.
  - Tudo o que o jogo escreve na CPU e a GPU lê (e o contrário) precisa
    ser **descoberto e copiado** pelo VIctory. Essa é a origem do custo
    descrito em `01-fase4-problema-atual.md`.

## 2. Software do console

### 2.1 Sistema operacional

- **[pública, fontes secundárias]** Desde o Xbox One, o console roda um
  hipervisor derivado do Hyper-V com partições:
  - um *host OS* que controla o hardware;
  - um sistema compartilhado para o painel e os apps;
  - o **Game OS** (partição exclusiva), onde o jogo roda com quase todos
    os núcleos, a GPU e a maior parte da RAM.
- O Game OS é um núcleo Windows reduzido. Cada jogo pode trazer a sua
  própria imagem: o dump tem `gameos.xvd` [código: listado no dump].
- **[código]** O jogo é um executável PE32+ AMD64
  (`game_scarlett_master.exe`; "Scarlett" é o codinome da série) com DLLs
  próprias:
  - `XboxBvhBuilder` (construção de BVH para ray tracing);
  - FidelityFX (loader e upscaler);
  - `Bink2Scarlett` (vídeo);
  - `XCurl` e `libHttpClient.GDK` (rede).

### 2.2 APIs que o jogo usa (GDK) e como o VIctory as atende

O jogo importa funções de DLLs do sistema do console. O VIctory
implementa cada uma em `victory-hle::modules` [código]:

| Área do console | O que faz no console | No VIctory |
|---|---|---|
| `xmem` | memória física, páginas de 2 MiB, "views" de seções físicas (a mesma página em vários endereços) | memória do host, mesmas restrições de alinhamento; views como mapeamentos de arquivo |
| `d3d12x` (Direct3D 12.X) | D3D12 do console: listas de comandos e memória de GPU sob controle do jogo | `victory-gpu` traduz objetos e comandos para o D3D12 do PC |
| `dstorage` | DirectStorage com descompressão em hardware | leituras de arquivo e descompressão na CPU; avisa o `watch` do que escreveu (`title_memory_written`) |
| `gameinput` | controles e teclado | XInput, DualSense, teclado como controle quando nenhum controle está conectado |
| `xgamesave`, `xuser`, `xstore`, `xnetworking` etc. | saves, usuário, loja, rede | saves em `VICTORY_DATA_DIR`; rede respondendo como console offline |
| `xg` | biblioteca de layout de texturas (tiling) | `victory-formats` (detiling) |
| `ffx` | FidelityFX do console | ponte para o FidelityFX SDK do PC (`VICTORY_FFX_DIR`) |

Nota de limite: o `xstore` responde a consultas de licença como um
console offline com licença ativa. Isso já está no upstream e foi
apontado na auditoria como zona cinzenta. Não deve ser estendido
(regra do projeto: nada de contornar licenciamento).

### 2.3 Direct3D 12.X: o que muda em relação ao D3D12 do PC

| Assunto | Console | PC (Windows) | Consequência no VIctory |
|---|---|---|---|
| **Shaders** | Já compilados para a ISA da GPU (RDNA 2), dentro de contêineres no executável e no `shaders.rpf` [código: o tradutor decodifica `gfx10`] | DXIL, compilado pelo driver na criação do pipeline | tradução ISA → DXIL (`victory-shader`), cache de shaders, pré-build e biblioteca de pipelines |
| **Driver** | Fixo: um hardware, um driver, sem compilar na hora [pública: a Microsoft contrasta "fixed driver and GPU environment" com o PC] | Muitos drivers e GPUs, compilação na primeira execução | ~28 mil pipelines; compilar durante o jogo trava; o pré-build leva ~1 h na primeira vez |
| **Memória de GPU** | O jogo escolhe endereços virtuais de GPU e põe buffers, texturas e descritores onde quiser na memória comum [código] | Recursos opacos, em heaps; a CPU não vê a VRAM | cópias "shadow", consulta de escritas, cópia de volta |
| **Descritores** | Registros de bytes na memória, que o jogo copia e escreve diretamente (ex.: cópias byte a byte de registros de descritor) [código: commits do fxceless] | Heaps de descritores opacos | espelhar bytes e reconstruir descritores do host (`bindless_plan`, "descriptor sync") |
| **Parâmetros de raiz** | Chegam nos registradores escalares (user SGPRs) do shader; o que não cabe vai para uma tabela em memória [código: `unspill.rs`] | Root signature | o tradutor reescreve essas leituras como parâmetros de raiz |
| **Tamanho de wave** | Shaders de computação usam Wave64 [código] | RDNA 4 aceita 32 e 64; NVIDIA só 32 | na AMD, `WaveSize(64)` quando o programa lê outras lanes; na NVIDIA, dividir em metades (`docs/wave64-on-wave32.md`) |
| **Sincronização CPU↔GPU** | Memória coerente: o jogo lê na CPU o que a GPU escreveu, às vezes sem esperar uma cerca explícita [inferido do comentário de `copy_back`] | Precisa de cerca e cópia explícitas | o envio espera a GPU e copia de volta: o quadro fica serial |
| **Ray tracing** | BVH construída com `XboxBvhBuilder.dll` em formato próprio do console | DXR com estruturas opacas do driver | a luz indireta por RT não é traçada; o Belobug usa uma luz ambiente constante no lugar |
| **Métodos sem equivalente** | `device` slots 0x1c8 e 0x218 (criação de pipeline com descrição desconhecida) [código: aviso nos logs] | | não implementados em nenhuma árvore; o jogo segue sem eles |

### 2.4 Armazenamento: Velocity Architecture × PC

- **[pública]** Quatro peças: SSD NVMe, blocos de descompressão em
  hardware (zlib e **BCPack**, um formato próprio para texturas),
  DirectStorage e Sampler Feedback Streaming.
- **[código]** O jogo pede ao DirectStorage leituras de arquivos dentro
  de pacotes (`G:\pack\...\*.rpf`, arquivos `.ttpf` de "ttdb") direto
  para a memória física. No PC, o VIctory lê e descomprime na CPU. A
  descompressão BCPack está listada como "planned" em `victory-formats`.
- **[medido]** Mover o jogo do HDD para o NVMe ajudou o carregamento; o
  quadro no mundo quase não mudou.

## 3. Windows: o que o PC tem que o console não tem, e o contrário

- **Paginação e proteção de memória:** permitem ao VIctory saber o que o
  jogo escreveu, com `GetWriteWatch` e páginas protegidas contra escrita
  tratadas por exceção. É exatamente o custo medido de ~11 GB conferidos
  por quadro.
- **VRAM separada e PCIe:** o que o console nunca copia, o PC copia.
  - Opções conhecidas: manter buffers grandes na VRAM e comparar páginas
    na GPU (os "card buffers" do Belobug, hoje inativos aqui), ou usar a
    memória do jogo diretamente pela GPU quando possível ("used in
    place").
  - O log mostra `title memory used in place by the host GPU on=true`
    para parte da memória.
- **Pilha de drivers genérica:** compilação de pipelines, validação e
  barreiras de estado. A biblioteca de pipelines e o pré-build compensam
  a compilação; o resto é custo fixo.
- **Agendamento de threads genérico:** no console, o jogo tem núcleos
  exclusivos. No PC, as threads do jogo dividem os 8 núcleos com as do
  VIctory (envio, cópias, compilação de pipelines).

## 4. Implicações práticas para a fase 4

1. O custo por quadro escala com **quanta memória o jogo usa na GPU por
   envio**, não com a complexidade gráfica. Por isso a GPU fica ociosa
   (~35% de uso que você observou) enquanto a CPU prepara.
2. O caminho de maior ganho é **copiar menos** (saber exatamente o que
   mudou) e **copiar na GPU** (card buffers), não acelerar shaders.
3. Sobrepor CPU e GPU (relay) é o segundo passo. Ele exige resolver a
   falha de página no dispatch de culling, provavelmente uma cópia que
   não terminou a tempo.

## Fontes

- Microsoft, Hot Chips 2020, "Xbox Series X SoC" (Jeff Andrews):
  https://hc32.hotchips.org/assets/program/conference/day1/HotChips2020_GPU_Microsoft_Jeff_Andrews_v2.pdf
- Xbox Wire, especificações do Series X:
  https://news.xbox.com/en-us/2020/03/16/xbox-series-x-tech/
- AnandTech, review do Series X (memória com chips de densidades
  diferentes): https://www.anandtech.com/show/16217/the-xbox-series-x-review-ushering-in-next-gen/2
- Xbox.com, Series S: https://www.xbox.com/en-za/consoles/xbox-series-s
- Tom's Hardware, Series S: https://www.tomshardware.com/uk/news/xbox-series-s-specs-reveal
- Xbox Wire, Velocity Architecture:
  https://news.xbox.com/en-us/2020/07/14/a-closer-look-at-xbox-velocity-architecture/
- Microsoft, Advanced Shader Delivery (contraste entre o driver fixo do
  console e o PC): https://devblogs.microsoft.com/directx/introducing-advanced-shader-delivery/
- Sistema operacional (fontes secundárias, tratar com cautela):
  https://betawiki.net/wiki/Xbox_OS e
  https://xboxoneresearch.github.io/wiki/faq
