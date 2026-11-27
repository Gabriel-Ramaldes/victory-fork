# 13 — Auditoria complementar: Scarlett e AMD Megafork

Data: 2026-11-26. Nossa referência: `dev-belobug` `0deb4165` (Belobug `71652a2` + 59 commits nossos).
Só leitura: clones em `scratchpad/audit2/{scarlett,megafork}`; nada foi compilado nem executado, e nada no nosso repositório foi alterado.

## 0. O achado que muda a leitura de tudo: as linhagens

| Repositório | HEAD lido | Base real | Máquina dos números |
|---|---|---|---|
| Op-lite/victory-scarlett | `835ca60` (2026-11-27) | codec-d/cool_fox `f4b48ad` ← avatarmanz/VIctory-7900XTX `0a43566` (linha L3) | Ryzen 7 9800X3D, **RTX 4080 SUPER**, 32 GB |
| rorodev6/victory-megafork-amd | `8e0002e` (2026-11-26) | Belobug `cd8dfc0` + 10 commits (setup, Windows 10, perfis de launcher, porte do fxceless #1353) | sem medição própria |
| Belobug `cd8dfc0` | — | **avatarmanz `c9c932c` + "RX 9070 XT fixes & HUD ported onto upstream c9c932c" (`e942896`)**; `71652a2` (nossa base) **não** é ancestral | avatarmanz: i9-12900K + RX 7900 XTX |
| Belobug `main` | `4f62c2a5` (2026-11-25) | a linha antiga, a nossa | parada desde 25/11 |

- Licença: GPL-3.0 nos dois, compatível com o nosso fork.
- **Consequência:**
  - O próprio Belobug trocou de base para a linha do avatarmanz (L3), e o Megafork vai junto.
  - Nossa `dev-belobug` está na linha antiga do Belobug, que ele parou de seguir em 25/11.
  - O Scarlett também é L3.
- **Os três repositórios auditados compartilham uma arquitetura que não é a nossa:**
  - não há cópias esparsas;
  - há pool de texturas (`pool.rs`), níveis por tile (`tiles.rs`) e `MAPPING` RwLock;
  - os "renderer ports" do avatarmanz (`VICTORY_PORT_*`) corrigem o renderer do título em memória, com checagem de bytes.
- **Números alegados** (de outras máquinas, não extrapoláveis):
  - avatarmanz `c9c932c`: ~45 ms por quadro numa rua da cidade (Series S, 12900K + 7900 XTX).
  - Scarlett: 35–56 ms (9800X3D + RTX 4080 SUPER).
  - Nós: ~210–230 ms (3700X + 9070 XT).
  - A diferença de CPU (X3D/12900K contra Zen 2) explica parte, mas não um fator de 4–5×.

## 1. Scarlett: os oito mecanismos

| # | Mecanismo | Origem | O que faz (pelo código) | Sincronização | Equivalente nosso | Etapa nossa afetada |
|---|---|---|---|---|---|---|
| 1 | `VICTORY_TABLE_MEMO` (`=1` ou `verify`) | `c45b43e` (commit agregado, 26/11; marcado "Fox T", porte do cool_fox) | Por lista: antes de preparar uma entrada de tabela (`readable`), procura a chave (endereço do descritor, seus 32 bytes, tipo, se o shader grava, depth testado) em `entry_memo`. Se a lista já preparou a mesma entrada **e o epoch da lista não mudou** (toda mudança na lista incrementa o epoch, `HostList::changed_something`), pula. Texturas guardam a cópia host vista (`Seen`) e invalidam se ela mudou; views com alias de formato não são memorizadas. `view_memo` faz o mesmo para root views. `verify` executa o caminho inteiro e conta divergências (devem ser 0). | Estado por lista (`State`), sem lock compartilhado. Contadores thread-local descarregados a cada 4096. | **Parcial.** O nosso `readable` (`draw.rs:1899`) é por entrada e não tem memo. O refresh de buffer já é só uma nota (`defer_refresh`); o trabalho real fica no `bindless_plan`, que já é memorizado por heap com slots sujos (`descriptor/host.rs:1600`). | Gravação das listas (threads do título) e "bindless records". **Não** copies nem copy-back. |
| 2 | `VICTORY_PARTIAL_DELTA` | `411a745` (26/11), `texture/delta.rs` | Upload de textura escolhida por índice: por cópia host e subrecurso, guarda o digest dos bytes do título usados no último upload submetido. Copia só subrecursos cujo digest mudou; na submissão confere de novo e, se mudou entre gravação e submissão, esquece o registro e reenvia inteiro. Só 2D, fino, sem MSAA. | `Mutex<HashMap>` global por cópia; reconfirmação na submissão. | Não temos. Nossa etapa "textures" é de 2–8 ms por quadro. | textures (pequena aqui) |
| 3 | `VICTORY_BAKE_DELTA` | `7c8f019` (26/11), `texture.rs:807-823` | No bake de personagem: fonte já enviada, sem detile desde então e sem escrita vista (write-watch, guard, não "hot") desde o enchimento do staging → não copia nem relê; a checagem normal da submissão continua. | Atômicos no staging. | Não temos esse caminho de bake/staging (arquitetura de texturas diferente). | textures, só em cenas com bake |
| 4 | `VICTORY_LEVELS_ON_DEMAND=0` | `8dd30ab` (fxceless, 25/11), `tiles.rs:76-110` | **O padrão do código já é desligado.** `=1` cria texturas como recursos reservados, com tiles por nível (64 KiB, heaps de 64 MiB), e só os níveis que as views mostram recebem memória (exige tiled resources tier 2). O `=0` do Scarlett só desfaz o `=1` que o launcher do cool_fox ligava: com `=1`, toda textura acabava com a cadeia inteira no card; com `=0`, re-uploads ~0 e −4 GB de commit. | — | Não temos `tiles.rs`. Nada a portar: é a escolha do padrão contra uma configuração de launcher. | VRAM e re-uploads, não o nosso caminho crítico |
| 5 | `VICTORY_MAP_LOCK_FINE` | `7c8f019`, `watch.rs:3050+` (`mod fine`) | Troca o RwLock global `MAPPING` (leitores de memória do título contra mudanças de mapeamento) por intervalos: o leitor espera só escritor que se sobreponha. Mutex + Condvar, um escritor por vez. | Prova de invariante no comentário. | **Não se aplica:** não temos `MAPPING`. Nossa linha lê memória do título com leitura segura (`read_into`) e reage a remapeamentos por notificação (`title_memory_remapped`, `remapped()` das janelas). | — |
| 6 | `VICTORY_POOL_DISCARD` | `411a745`, `victory-hle/.../xmem.rs:2370+` | Páginas de pool devolvidas pelo título (vistas só pela view inteira da seção) ganham `MEM_RESET` + `VirtualUnlock` depois de 2–10 s, quando a RAM livre cai abaixo de 4 GB. Corta congelamentos de ~1 s (39 → 7 em 6 min) quando map/unmap fica 50× mais lento com pouca RAM. | Sob o lock das views. | Não temos. Depende de `VICTORY_SECTION_WINDOW` (views duradouras por seção), que não existe aqui. | Travadas por pouca RAM, não o quadro médio. **Relevante para nós** (commit 25,7–26,9 GB de 32), mas é HLE de memória. |
| 7 | `VICTORY_FILL_TITLE` | `9fea743` (24/11), `host.rs:3018-3045` | Um fill pequeno (≤ 64 KiB) de memória de upload que o título escreveu desde a tomada, ou onde nenhum shader escreveu nos últimos 600 quadros, não espera os "flights" (submissões em voo com copy-back). No caso deles: 1 fill de 480 B por quadro no heap de constantes de 174 MB, ~30 ms de espera num quadro de 130 ms. | Depende de `written_since_taken` e `landed_lately`. Risco declarado: ler constante velha que um shader em voo escreveria. | Não temos fills com espera por flight. Nossas esperas são estruturais: cada envio drena o anterior, e o heap de constantes é coberto por janela STRETCH ou cópia. | sincronização / "on the GPU" (drenagem), indireto |
| 8 | `VICTORY_POOL_CAP` | `c45b43e`, `pool.rs:1087` | Limita blocos novos do pool de texturas a 95% do orçamento do card; usa buracos em blocos existentes antes de criar. | Contadores atômicos. | Não temos pool de texturas. | VRAM, não o caminho crítico |

### 1.1 `TABLE_MEMO`: dá para reproduzir aqui?

- **O que o ganho deles mede:** 81% de ~54 mil entradas de tabela por quadro (na máquina deles) já preparadas antes na mesma lista, sem nada mudado. Com o memo, a lista não repete `readable` para elas. Isso economiza trabalho nas **threads de gravação do título** (a gravação deles custa ~22 µs por bind).
- **Conceitualmente reproduzível:** nosso `readable` também roda por entrada, em quatro chamadas no `draw.rs` (1139, 1251, 1273, 1317). Uma memo por `HostList`, com chave (handle, bytes do descritor, tipo, se grava) e invalidada por um epoch da lista, é viável.
- **O que não sabemos:** quanto o nosso `readable` custa. A parte cara dele (copiar buffer) já foi adiada (`defer_refresh`); o que sobra é decodificar a view, `placed_texture`, `ensure_state` e `texture_written`. Nossa linha de quadro não separa o tempo de gravação.
- **Pré-requisito:** a lista precisa de um epoch que mude a cada mudança relevante (estado de textura, upload, twin). Não temos isso. Seria código novo em `list.rs`/`host.rs`.
- **Conclusão:** reproduzível como ideia, ganho desconhecido. Primeiro medir: contador de chamadas de `readable` por quadro e quantas repetem a mesma chave na lista, mais o tempo total. Se der menos de ~5 ms, não vale.

### 1.2 `PARTIAL_DELTA` / `BAKE_DELTA` contra copies e copy-back

- **O que eles cortam:** são caminhos de **upload de textura**, não de buffers.
  - `PARTIAL_DELTA` corta reenvios inteiros de texturas indexadas que ganham uma página de 64 KiB por vez (125.312 uploads em run-113 deles).
  - `BAKE_DELTA` corta reenvios de fontes do bake de personagem que não mudaram.
- **Nas nossas etapas:**
  - Nenhum dos dois toca em **copies** (refresh de buffers do título, 35–80 ms), em **copy-back** (~45–50 ms) nem no eco da GPU.
  - Nossa etapa "textures" fica em 2–8 ms e 3–20 MB por quadro.
- **Prioridade:** baixa para o nosso perfil.

### 1.3 Um bônus fora da lista: `VICTORY_KNOWN_WRITES` (`knownwrites.rs`)

- **O que é:** um ledger próprio de páginas sujas usado como filtro na frente do rastreamento. Uma página sem marca desde a tomada não passa pelo write-watch, pelo guard nem pela comparação.
- **Por que não serve para o nosso eco:**
  - o próprio comentário diz que o volume de cópias continua o mesmo;
  - páginas vistas em outro endereço (aliases) **não** são poupadas.
- **O que ajudaria:** só o custo de perguntar (watch e guards), que nas nossas medições não está no caminho crítico (bitmap do watch: sem ganho no quadro).

## 2. Scarlett: experimentos rejeitados

| Switch | Origem | Resultado deles | Por quê | Para nós |
|---|---|---|---|---|
| `VICTORY_THIN` | `3a99ee6` (avatarmanz, 25/11), `list/thin.rs` | 7900 XTX: quadro 75 → 68 ms. Scarlett (4080): sem ganho e o dobro de quadros longos. | As chamadas de lista só anotam argumentos; 8 threads nossas traduzem ao lado. Tira 160–190 ms de CPU das threads de gravação, mas desloca a espera e colide com `BIND_FILTER`/`CHEAP_*`. Precisa da linha L3 (fila `queue/behind.rs`). | Não portável sem trocar de base. |
| `VICTORY_WINDOWS_BAR_MB` | `bd68f5e`, `shadow.rs:482-525` | Em replay, listas 29 → 26 ms e quadro 36 → 32. Em jogo, sem ganho e +1,4 GB de VRAM. | Janelas em heap `GPU_UPLOAD` (Resizable BAR, sem lado de CPU). Só ajuda se o card for o gargalo; com o card ocioso, nada muda. | Nosso card não é o gargalo. Não. |
| `VICTORY_CARD_AWAKE` | `5f86fa3`/`040aa27`, `host/awake.rs` | Adotado no cool_fox (7900 XTX: lacunas de 1,2–2,4 ms entre envios). Scarlett: card a 97–99% em menus, sem ganho. | Um loop de compute mantém o clock alto entre envios. Na 4080 não paga. | Depende do card. Na 9070 XT, com 71 ms de GPU de ~220, poderia reduzir lacunas, mas o quadro é CPU. Baixa prioridade. |
| `VICTORY_PAGE_MIGRATE` | `c45b43e`, `pool/pages/migrate.rs` | **Perda de device** em todas as rodadas (83–102 s), mesmo com bytes verificados. | `UpdateTileMappings` em textura viva que listas em voo ainda usam. Suspeita: caches de tradução da GPU não drenados. | **Não reproduzir** (perda de device). Mesma família do "tiles in place" que já descartamos. |
| `VICTORY_LIST_BATCH` | `c45b43e`, `descriptor/host.rs:2883, 6006` | Perda no quadro (README: "or a loss"). | Agrupa as cópias de descritores de uma tabela nova; o custo de montar o lote superou o de copiar. | Não. |

## 3. AMD Megafork: a fila de integração

Estado real, verificado no histórico (`git log`) e no código do `8e0002e`:

| Candidato | Estado no Megafork | Estado em nós | Observação |
|---|---|---|---|
| fxceless #1336 (vida das referências de textura em cópias GPU) | **Não integrado** (nenhum commit). O próprio AMD-TESTING diz que o patch não se aplica direto. | Não temos | Estabilidade; sem sintoma conhecido aqui. |
| Cool Fox + fxceless #1031/#1210 (sincronização de uploads de textura de personagem, mips residentes) | **Não integrado** | Não temos | Imagem (rostos, cabelo). A Belobug `5d6bd84` ("upload title textures in submission prelude to fix frame-order & flickering") está na base `cd8dfc0`; **não está na nossa**. |
| Animação facial em 16 bits (fxceless #927) | Não como PR. A base tem `9a6c829` ("d16 data of integer views keeps its bits") | **Não temos** `9a6c829` | O Scarlett tem `VICTORY_HALF_VIEWS`/`VICTORY_D16_BITS` ("corrige animação facial congelada"). Candidato de imagem, não de fps. |
| AvatarManz #55 (compilação de shaders em RDNA 4) | **Não integrado** | Não temos | Afeta a partida a frio, não o quadro. |
| fxceless #1226 (prebuild de pipelines) | Não integrado como PR; a base tem `b7ab9c4` (`victory warm`, `VICTORY_PIPELINE_BOOK`) | Temos o nosso `prebuild` (`57520af`) | Já coberto. |
| Geckomacabre `b9a97e9d` (menos esperas de GPU e cópias em lote) | **Não integrado** | Não temos | Seria sincronização. Precisa de leitura própria do vx-layer: não auditado aqui. |
| fxceless #1353 (job de pipeline concluído após panic) | **Integrado** (`af875f2`) | Não temos | Robustez do build de pipelines. |

**Conclusão do Megafork:**
- Como fork, ele traz só launcher, documentação, suporte a Windows 10 e um porte de robustez.
- O que interessa nele é a base: Belobug `cd8dfc0`, ou seja, avatarmanz + correções da 9070 XT.

## 4. Prioridade pelo nosso perfil

Escala: ●●● alto, ●● médio, ● baixo, — nenhum.

| Oportunidade | Preparo CPU | Copies | Copy-back | Sincronização | Submissão | GPU | Estabilidade | Esforço |
|---|---|---|---|---|---|---|---|---|
| **Medir a base `cd8dfc0` (ou o Megafork) nesta máquina** | ●●● (outra arquitetura) | ●●● | ●●● | ●● | ●● | ● | ? | baixo para medir; enorme para adotar |
| `TABLE_MEMO` (conceito) | ●● (se `readable` for caro) | — | — | — | — | — | — | médio (epoch por lista) |
| Eco da GPU: `gpu_wrote` só onde a GPU escreveu (nosso, doc 12) | ● | ●●● (~200–280 MB por quadro idênticos) | ● | — | — | — | — | médio |
| `FILL_TITLE` (conceito: não esperar copy-back para memória que só o título escreveu) | — | — | ● | ●● | ● | — | risco de dado velho | médio |
| `POOL_DISCARD` | — | — | — | — | — | — | ●● (travadas com pouca RAM) | alto (HLE de memória, exige views por seção) |
| `PARTIAL_DELTA` / `BAKE_DELTA` | ● (textures 2–8 ms) | — | — | — | — | — | — | alto (outro caminho de textura) |
| `MAP_LOCK_FINE`, `POOL_CAP`, `LEVELS_ON_DEMAND` | — | — | — | — | — | — | — | não se aplica |
| `9a6c829` / `HALF_VIEWS` (imagem) | — | — | — | — | — | — | imagem | baixo a médio |

## 5. Plano experimental (no máximo três; nenhum combinado)

### Experimento 1: medir a base nova do Belobug nesta máquina (pré-requisito, sem porte)

- **Commits e árvore:** Belobug `cd8dfc0` (ramo `belobug/rx9070xt` ou `a-publier`), já presente no remote `belobug` do nosso repositório. O Megafork `8e0002e` só acrescenta launcher.
- **Integração mínima:**
  - nenhuma: um worktree novo, só leitura do código, build `diag`;
  - cache e dados próprios em `C:\VictoryData\victory-cd8dfc0\`, com o save da sacada copiado para lá (dados do usuário, não arquivos do jogo).
- **Flags:**
  - as nossas de rodada (`VICTORY_HOST_PADS=0`, `VICTORY_KEYS=56:Enter`, `VICTORY_SKIP_MOVIES` se existir lá);
  - **os padrões deles, sem relay** (`VICTORY_RELAY` explícito em 0 se existir);
  - os `VICTORY_PORT_*` primeiro desligados (o padrão), depois só os que a 9070 XT do Belobug liga, cada um medido sozinho.
  - Não importar o launcher deles sem ler cada variável.
- **Equivalência:** mesmo save, mesmo profile (Series X), mesma duração (240 s), mesma contagem de draws; capturas finais comparadas a olho.
- **Instrumentação:** a linha de quadro deles (formato diferente: escrever um parser), `samples.txt` (commit e working set).
- **A/B:** 3 pares intercalados contra `dev-belobug` `0deb416`, save restaurado a cada rodada, nada mais rodando.
- **Sucesso:** mediana do quadro pelo menos 25% menor, sem perda de device, commit estável e imagem equivalente ou melhor. Se der certo, o assunto muda de "portar mecanismos" para "trocar de base" e isso volta para você decidir; não é um merge.
- **Rollback:** apagar o worktree. Nada no nosso ramo muda.

### Experimento 2: o eco da GPU marcado por página escrita (nosso, continuação do doc 12)

- **Só faz sentido se ficarmos na nossa base.**
- **Arquivos:** `shadow.rs` (`copy_back` de janela STRETCH, hoje "shortcut: the whole declared range") e `watch.rs` (`gpu_wrote`).
- **Integração mínima:**
  - em vez da faixa declarada inteira, marcar só as páginas cujo conteúdo mudou entre o snapshot anterior e o posterior à execução;
  - para janelas in place não há snapshot, então primeiro medir quantas páginas mudam de fato (diagnóstico).
- **Flag:** `VICTORY_GPU_WROTE_EXACT=1` (diagnóstico primeiro).
- **Equivalência:** `VICTORY_COPY_DIFFERS=1` deve mostrar o eco caindo sem aumentar "changed something" em outra origem; `VICTORY_CLEAN_VERIFY=1` sem divergências.
- **A/B:** sacada, 3 pares, junto com o A/B pendente das páginas de borda (em rodadas separadas).
- **Sucesso:** copies −10 ms ou mais e quadro menor nos 3 pares.
- **Rollback:** flag desligada.

### Experimento 3: medir antes de portar o `TABLE_MEMO`

- **Arquivos:** `draw.rs` (`readable` e os quatro pontos que o chamam).
- **Integração mínima, só diagnóstico, sem pular nada:**
  - contar por quadro as chamadas de `readable`, o tempo total e quantas repetem a mesma chave (handle, 32 bytes, tipo, grava) na mesma lista sem mudança desde a anterior;
  - "mudança": por enquanto, qualquer `ensure_state` ou upload na lista.
- **Flag:** `VICTORY_COPY_COUNTS=1` (o contador entra nas linhas de cópia).
- **Equivalência:** nada a verificar; nada é pulado.
- **A/B:** não há; uma rodada diagnóstica na sacada.
- **Sucesso para continuar:** `readable` acima de ~5 ms por quadro e repetições acima de ~50%. Aí sim a memo com epoch e `verify` (como no Scarlett) entra como experimento com A/B.
- **Rollback:** contador só no build de diagnóstico.

## Recomendação

Fazer primeiro o **experimento 1**:
- Todo o ecossistema que mostra números bons (avatarmanz, cool_fox, Scarlett e o próprio Belobug) está agora na linha L3, e nunca medimos essa linha nesta máquina.
- Os mecanismos do Scarlett que interessam (`TABLE_MEMO`, `FILL_TITLE`, `KNOWN_WRITES`) dependem dessa arquitetura.
- Portá-los para a nossa linha custa mais do que medir a deles.
- Se a base nova não ganhar aqui, seguimos com os experimentos 2 e 3 na nossa.
