# Próxima otimização: linha de base, auditoria do fork 7900XTX e recomendação

25/11/2026, `dev-belobug` em `b023488` (código em `6f7f4c1`). Nada de
código mudou para este documento.

## 1. Linha de base

Configuração da rodada `baseline-6f7f4c1`:
- build diag;
- `VICTORY_COPY_COUNTS` e `VICTORY_BACK_READS` desligados (o log diz
  "counters off");
- `VICTORY_HOST_PADS=0`, save congelado, 6 min;
- medianas dos últimos 25 relatórios (cada um ~5 s), com p25-p75.

| | linha de base nova | base antiga `devbel-sparse-on` (84e507c) |
|---|---|---|
| quadro | **388 ms** (p25-p75 316-430; pico 1.667) | 342 ms (331-422) |
| preparo na CPU | 206 ms (157-241) | 176 ms (168-224) |
| cópias / planos bindless | 66 / 42 ms | 53 / 26 ms |
| GPU | 67 ms (65-87) | 72 ms (66-73) |
| cópia de volta | 74 ms (71-83), 24,6 MB | 84 ms (82-88), 35,8 MB |
| draws | 4.757 (4.332-5.032) | 5.021 (3.990-5.049) |
| draws descartados sem pipeline | ~550-3.500 | ~350 |
| memória: conjunto de trabalho / commit | 19,7 / 25,3 GB | 19,2 / 25,3 GB |

**A janela desta rodada não ficou estável:**
- Os draws oscilaram de 4.300 a 7.000 e os descartados chegaram a 3.500.
  Na base antiga ficaram em 5.048 e ~350.
- O save congelado não levou à mesma cena. A diferença de 388 para 342
  ms não vem dos contadores, que estavam desligados.
- Conclusão de método: entre rodadas há ±15% de ruído. Um A/B precisa de
  duas rodadas de cada lado, alternadas, e de comparar as etapas
  ("copying back"), não só o quadro.

## 2. Auditoria do fork avatarmanz/VIctory-7900XTX

**Ancestralidade:**
- O fork é da linhagem fxceless, montado sobre a nossa `main` (`ffe310e`,
  24/11): 408 commits à frente dela.
- A base comum com a `dev-belobug` é `8e79c80` (24/11): 420 commits do
  lado deles, 536 do nosso. Em `victory-gpu` há ~80 mil linhas a mais;
  `watch.rs` tem +4.500 e `shadow.rs` 5.588 linhas, contra 2.658 nossas.
- **O fork não tem o caminho sparse** (`sparse()`, `CHUNK`,
  `sparse_back`): as cópias dele são contíguas, com um snapshot `Vec`
  único por cópia.
- Nada portável como código: só ideias.

**Máquina:**
- Deles: i9-12900K (8P+8E), 64 GB, e o próprio README recomenda 64 GB
  (40-46 GB privados).
- Nossa: Ryzen 7 3700X (8C/16T, Zen 2) e 32 GB de DDR4-3600 em dois
  canais.

| Implementação no fork | Arquivos / funções | Correspondente na `dev-belobug` | O que resolve |
|---|---|---|---|
| **Comparação larga** (`VICTORY_WIDE_BACK`, ligada por padrão, **sem relay**) | `shadow.rs`: `wide_back`, `find_all`, `find_pieces`, `wide`, `workers`, `take_snapshots`; `host.rs`: `hand_back` | `Shadow::sparse_back` compara tile a tile numa thread, segurando a trava `before` da raiz | comparação (e snapshots) presos a uma thread "pela velocidade de memória de um núcleo" (palavras deles: 375 MB, 35 ms) |
| Comparação **"AVX2"** | `safecopy.rs`: `differ`, `merge` (asm `victory_safe_merge`), `put_changed`; `shadow.rs`: `word_runs_avx2` | `merge_gpu_writes_mut` (8 bytes por vez, byte a byte onde difere) + `read_into` + `write_mapped` | merge e gravação dos pedaços **diferentes**: lê o jogo só onde mudou e grava só os dwords que mudaram |
| Divisão do trabalho | `land.rs`: `Pool`, `jobs` (~256 KiB, cortes em bloco de 64 KiB), `each`; `wide` (~1 MiB, threads = núcleos/4, de 2 a 6, com espera ativa) | `host::in_parallel`: cria threads (`thread::scope`) a cada chamada, 6 threads, por grupo de `Shadow` | trabalho em threads mantidas; ordem garantida por bloco de 64 KiB |
| Travas | `land_back`: `merging` (RwLock de leitura) + `before` só para pegar o ponteiro; `find_all` pega o ponteiro sob a trava e compara fora dela | `before` mantida a chamada inteira; `watch::writing` com a trava global `GUARDS` e `VirtualProtect` por bloco | paralelismo sem trava durante a comparação |
| **LAND / LAND_BESIDE / LAND_ECHO** | `land.rs`, `inplace.rs` (`beside`), `watch.rs` (`landed_beside`, `block_published`), `Shadow::landed` | `watch::writing` fixa o bloco por 2 s; `copy_stale` relê (o eco, §4.1 do doc 01) | grava pela própria visão das páginas físicas, sem mexer nas guardas; marca a cópia como atual e não relê o que ela mesma gravou |
| **RELAY** | `host.rs`: `relay`, `RelayBack`, `relay_land`; cópias na memória da placa | não temos (relay ligado aqui: perda de dispositivo aos 148 s na rodada vi-fx3) | traz o que a GPU escreveu numa thread própria, atrás do quadro |

**A comparação "AVX2" não é a comparação dos tiles:**
- Em `find_pieces`, cada pedaço de 64 KiB usa o mesmo `now != old` (um
  memcmp) que o nosso `sparse_back`. O ganho deles ali vem das threads.
- O AVX2 está no merge e na gravação (`safecopy::merge`, que compara o
  que a placa tem agora com o que tinha, 32 bytes por vez, e grava
  dwords) e num hash (`torn.rs`).
- Os dados são do mesmo tipo dos nossos: bytes da cópia na CPU contra o
  snapshot, e a memória do jogo. Mas o merge deles grava sem
  `watch::writing`, pela visão própria (LAND_BESIDE), e mantém `held`
  atualizado. Portar o AVX2 sozinho não remove o que pesa no nosso
  caminho dos tiles diferentes (trava do watch, proteções, faltas de
  página).

## 3. As três alternativas, com os nossos números

Do doc 01, §4.2, por quadro: ~515 MB comparados em ~7.900 tiles; 90%
iguais; comparação 41-44 ms, a ~24 GB/s de leitura (dois fluxos);
tiles diferentes 37-43 ms (~770 tiles, ~52 µs cada); tudo numa thread.

| | 1. Paralelizar só a comparação | 2. Paralelizar comparação, merge e gravação | 3. Rastrear o que a GPU pode escrever |
|---|---|---|---|
| Arquivos / funções | `shadow.rs`: `Shadow::sparse_back`, `each_tile`, `tile`; um pool de threads mantidas novo (o `in_parallel` cria threads a cada chamada) | o mesmo + `merge_gpu_writes_mut`, `watch::writing`, `host::write_mapped`, `watch` (VEH de escrita) | `host.rs` (intervalos declarados graváveis), `shadow.rs` (`sparse_writing`, `sparse_back`), tradutor de shaders ou hashes na GPU |
| Potencial no nosso perfil | comparação de ~42 para ~23-28 ms (piso de banda: ~1 GB lido por quadro a 40-45 GB/s práticos de DDR4-3600): **~15-19 ms** (4-5% do quadro) | até ~40 ms a mais, se a trava do watch e o `VirtualProtect` não serializarem (eles serializam: trava global e trava do espaço de endereços no kernel) | o maior: 90% dos ~515 MB comparados e boa parte dos ~600 MB de snapshot deixariam de existir |
| Trava `before` | segue segurada pela chamada; as threads só leem (os snapshots não mudam até o fim do `sparse_back`, e a GPU já terminou) | precisa ser quebrada por tile ou por bloco | redesenho |
| Intervalos sobrepostos | sem efeito: a comparação é só leitura e o merge segue em série, na ordem atual | precisa da regra do fork: pedaços do mesmo bloco de 64 KiB num só job, em ordem | a definir |
| `watch::writing` | intocado | é o gargalo provável e não é paralelizável como está (o fork o contornou com LAND_BESIDE, +4.500 linhas no watch) | intocado |
| Risco de regressão | baixo: mesmas comparações, mesmo resultado, mesma ordem de efeitos; risco de disputar núcleos e banda com as threads do jogo | médio-alto: ordem de escrita, escritas concorrentes do jogo, o 0,36% do eco | alto: um tile não comparado que a GPU escreveu é dado perdido |
| Complexidade | baixa-média (~1 função + pool + teste) | alta | muito alta |
| Experimento mínimo | abaixo | antes, separar o tempo dos tiles diferentes em leitura / merge / watch / gravação (só instrumentação) | — |

## 4. Recomendação única: paralelizar só a comparação em `sparse_back`

**Por quê:**
- A comparação é o maior bloco isolado da cópia de volta (41-44 de ~80
  ms).
- Ela roda numa thread a ~24 GB/s, abaixo da banda da máquina.
- É só leitura, e o resultado é o mesmo bit a bit.
- O fork mostra a mesma situação e a mesma saída (`VICTORY_WIDE_BACK`,
  sem relay e sem card buffers). Mas o ganho aqui é limitado pela DDR4
  do 3700X e não pode ser presumido a partir do i9-12900K.
- As alternativas 2 e 3 dependem de mudanças no watch ou no tradutor que
  ainda não medimos.

**Desenho para o experimento** (atrás de uma chave só de diag, desligada
por padrão, por exemplo `VICTORY_BACK_FIND_THREADS=<n>`):
1. Em `Shadow::sparse_back`, sob a trava `before` que já é tomada,
   juntar os tiles da chamada, com endereço, ponteiro da cópia, ponteiro
   do snapshot e tamanho.
2. Comparar esses tiles em `n` threads mantidas (com espera ativa curta,
   como o `wide` do fork), em pedaços de ~1 MiB, produzindo o conjunto
   dos tiles diferentes.
3. O laço atual roda em seguida, na mesma ordem, só sobre os
   diferentes, com o mesmo `read_into`, o mesmo merge, o mesmo
   `watch::writing` e o mesmo `write_mapped`. Tiles iguais já saem cedo
   hoje: o efeito é idêntico.

**Riscos a vigiar:**
- Threads extras disputando núcleos e banda com as do jogo: preparo e
  quadro não podem subir.
- Custo de acordar as threads em ~104 chamadas por quadro: pool
  mantido, nunca `thread::scope` por chamada.
- Ponteiros de tiles válidos só enquanto a trava está segura: a trava
  continua segura durante a comparação.

**Validação mínima:**
1. **Teste de unidade:** o conjunto de diferentes em paralelo é igual ao
   da comparação em série, com tiles aleatórios, tiles parciais nas
   pontas e tamanhos que não são múltiplos de 1 MiB.
2. **No jogo:** A/B no save congelado, duas rodadas de cada lado,
   alternadas, 6 min.
   - Com o controle conectado (pedido do usuário), o usuário só confirma
     o início e não move a câmera.
   - Medir "copying back" (mediana e p25-p75), preparo e quadro. Numa
     rodada extra de cada lado com `VICTORY_COPY_COUNTS=1`, conferir que
     a fração de tiles iguais/diferentes se mantém e que o tempo de
     comparação cai.
3. **Sucesso:** "copying back" cai 10 ms ou mais; o preparo não sobe;
   imagem igual; sem crash em 6 min. Se a banda limitar abaixo disso,
   registrar e partir para a instrumentação da alternativa 2.

## 5. Resultado do experimento com 2 workers (25/11)

Código: `09c16bb` (`VICTORY_BACK_COMPARE=<n>`, só diag, desligado por
padrão), mais o modo de checagem `VICTORY_BACK_COMPARE_CHECK=1` (commit
seguinte). Só a comparação `now != old` saiu da thread. A trava
`before`, o merge, o `watch::writing`, o `VirtualProtect` e as gravações
seguem em série, na mesma ordem. Os tiles da chamada são cortados em
pedaços de ~1 MiB; a thread que chama faz pedaços também. Chamadas com
menos de 2 MiB comparam na própria thread.

**Método:**
- build diag, save congelado, 6 min, `VICTORY_HOST_PADS=0`;
- rodadas alternadas base / 2 workers;
- medianas dos últimos 25 relatórios, com p25-p75.

| Rodada | Quadro (ms) | Preparo | Cópia de volta | Draws |
|---|---|---|---|---|
| cmp-base-a | 350 (323-587) | 185 | 81 | 3.688 |
| cmp-w2-a | 349 (336-445) | 187 | 71 | 4.795 |
| cmp-base-b | 361 (340-416) | 191 | 74 | 6.390 |
| cmp-w2-b | 344 (336-377) | 182 | 73 | 2.998 |

As cenas não ficaram iguais: os draws variam de 3.000 a 6.400. Por isso
a cópia de volta também foi normalizada pelo volume comparado:
- base: 137 e 138 ms por GB;
- 2 workers: 134 e 129 ms por GB;
- diferença de −3 a −6%.

**Par com `VICTORY_COPY_COUNTS=1`** (cmp-base-c / cmp-w2-c), tempos por
quadro:

| | base | 2 workers |
|---|---|---|
| comparação | 38,3 ms (511 MB, 13,4 GB/s) | 31,3 ms (524 MB, 16,7 GB/s) |
| ler, fazer o merge e gravar os diferentes | 42,2 ms | 46,5 ms |
| soma das chamadas | 83,2 ms | 79,4 ms |
| cópia de volta (relógio) | 81 ms | 80 ms |
| quadro | 366 ms | 354 ms |

**Linha do pool** (2 workers, por quadro):
- ~37-40 chamadas no pool, 27-29 ms, das quais ~1,0-1,2 ms esperando
  pedaços dos workers;
- ~65-73 chamadas pequenas na própria thread;
- pedaços: ~170 pela thread que chama, ~330 pelos workers;
- panicked 0.

**Uso de CPU:** 2,6 a 3,1 núcleos (de 16) dos dois lados. Conjunto de
trabalho de 19-21 GB e commit de 24-26 GB nos dois.

**Estabilidade:** 8 rodadas de 6 min (3 base, 5 com workers, 2 delas de
checagem), sem crash; `crashes.log` sem nada novo.

**Equivalência no jogo:**
- `VICTORY_BACK_COMPARE_CHECK=1` compara cada tile de novo na vez dele,
  usa essa resposta e conta onde a dos workers diferiu.
- Rodada cmp-w2-check2: **5.011.338 tiles rechecados, 0 discordâncias**.
- A rodada anterior (cmp-w2-check) tinha o contador invertido (contava
  concordâncias): marcou 3.326.546 de 3.326.546. É a mesma conclusão,
  corrigida no código antes da check2.
- O teste de unidade `tilecmp::the_answers_are_the_serial_ones` cobre
  tiles aleatórios, parciais e diferenças no primeiro, no último e num
  byte do meio.

**Imagem:**
- As capturas de quadros potência de 2 (telas de carregamento) estão
  corretas nas duas configurações.
- As capturas do mundo (`VICTORY_CAPTURE_WORLD=200`) mostram as mesmas
  cenas corretas.
- **Ponto em aberto:** em cmp-w2-c, quadros 11496/11497 (a cena da cela
  junto à janela), há uma mancha azul em blocos num objeto escuro no
  canto inferior direito.
  - Em cmp-base-c (quadros 11972/11973) e nas 18 capturas dessa cena de
    rodadas seriais antigas (`devbel-sparse-on`/`-off`), o objeto está
    preto.
  - Os quadros foram capturados em momentos diferentes da cena, e as
    rodadas de checagem chegaram a outras cenas.
  - Como as respostas dos workers são as seriais (0 discordâncias), a
    mancha não vem de tiles tratados de outro jeito. Sobra uma diferença
    de tempo (as threads mudam o ritmo do quadro) ou algo da própria
    cena. Não foi reproduzida nem descartada.

**Conclusão:**
- A comparação caiu ~7 ms (38 para 31 ms). A cópia de volta caiu de 3 a
  6% por GB comparado, e só ~1 ms no relógio do par -c.
- O critério do §4 (cair 10 ms ou mais) **não foi atingido**.
- Três threads dão só 1,3× na comparação (13,4 para 16,7 GB/s, dois
  fluxos de leitura): o limite é a banda de memória do 3700X com
  DDR4-3600, não o número de threads.
- O preparo e o quadro não subiram, dentro do ruído de ±15%.
- 4 workers ainda não testado: aguarda decisão.
