# 11 — Lado da CPU: copies, copy-back e preparo

Data: 2026-11-26. Branch `dev-belobug`, HEAD `2be674c8`. Só investigação; nenhum código alterado.

## Metodologia

**Benchmark de desempenho** (só ele compara frame time):
- build diag, save `data-gf` restaurado de `data-gf-snap` antes de cada rodada, 270 s;
- `VICTORY_ASYNC_SUBMIT=1`, `VICTORY_SKIP_MOVIES=end`, `VICTORY_KEYS=56:Enter`, `VICTORY_HOST_PADS=0`;
- sem `VICTORY_GPU_TIMING`, `VICTORY_D3D12_DRED`, `VICTORY_COPY_COUNTS` nem `VICTORY_PROFILE`;
- medianas dos quadros com ≥ 4500 draws, em pares A/B intercalados.

**Diagnóstico** (só proporções, nunca frame time):

| Rodada | Variáveis | Efeito medido |
|---|---|---|
| `gputime-1` | `VICTORY_GPU_TIMING=1` | submissões síncronas: quadro 303 ms; "on the GPU" vira espera síncrona (107 ms) |
| `gputime-2` | + `VICTORY_D3D12_DRED=light` | timestamp e marker por comando: quadro 325 ms; ocupação real da GPU 71 ms |
| `cpudiag-1` | `VICTORY_COPY_COUNTS=1` + `VICTORY_PROFILE` | quadro 290 ms contra 218 ms no benchmark; plans 37 contra 22 ms, records 24 contra 6 ms |

- **"on the GPU" com async:** soma só as esperas de drain da CPU. Não é ocupação da GPU.
- **O perfil:** amostra a thread que submete e acumula desde o início da rodada, carregamento incluído. Por isso ele dá proporções, não ms por quadro.
- **Draws:** a cena variou de 8,9 a 9,7 mil draws por quadro nestas rodadas, contra 4,7 a 6,5 mil nas medições de 2026-11-25. Comparações só valem entre rodadas do mesmo dia e das mesmas condições.

## Referência (`pool-B3`, benchmark)

| | ms por quadro |
|---|---|
| quadro | 218 |
| preparo | 96: copies 31 (134 MB), textures 7, bindless plans 22, bindless records 6, ~30 o resto |
| copying back | 35 (10,6 MB escritos) |
| espera da CPU pela GPU (drain) | 10 |
| fora do preparo, da cópia de volta e do drain | ~77: trabalho do título e esperas da timeline (ver "Preparo") |

- 152 submissões por quadro.
- GPU ocupada ~71 ms por quadro (`gputime-2`).

## Copies

Valores por quadro, do log de `copystat` (`pool-B3`, sem contadores opcionais):

- **Volume:** 142 mil cópias de título (59 MB) e 84 mil refreshes de faixas, que somam 38 GB de faixas pedidas.
- **Watches:** 15,2 mil chamadas de `watch::writes`/`writes_guarded`, 294 ms somados em todas as threads. Checam 15,0 GB, dos quais 14,4 GB (96%) não mudaram.
- **Outros:** moves 73 ms somados (`host::copy_mapped`), 16 mil page faults, 932 guard faults, 1440 pins de ~29 µs.

**Funções:**
- `Shadow::update_lazily` e `update_free` (`shadow.rs:745-830`);
- `watch::writes_guarded` e `guard` (`watch.rs:177`, `:556`), mais `walk`;
- `Shadow::copy_stale`, `host::copy_mapped:3082` e `victory_safe_copy`;
- tudo isso chamado de `Batch::prepare_bindless`, dentro de `in_parallel`.

**O que sobra de trabalho:**
- As mesmas faixas são pedidas várias vezes por quadro, uma vez por submissão: faixas de 7 a 24 MB aparecem 5, 10 e 20 vezes por quadro no log.
- O atalho `clean_spans` (`shadow.rs:766`) só existe para memória com guard. Não há contador de quantas vezes ele acerta.
- Uma faixa (`0x1034590000+0xb585ac`, 11,8 MB) custa 10,8 ms por quadro em 5 refreshes. Outra de mesmo tamanho (`0x103d570000`) custa 2,1 ms. A primeira é recopiada de fato a cada vez.

**Locks:** 3,5% da thread em `watch::guard:556` (lock `GUARDS` disputado), e mais disputa em `try_open_guarded:1082` vinda de outras threads. Em 2026-11-25 o lock de guard não estava no caminho crítico (bitmap A/B sem ganho).

**Por que difere do antigo:** as copies estavam em ~42 ms e estão em 31–40. O pin com escopo (doc 09) reduziu os refreshes de eco. O resto do custo é o mesmo padrão: cada submissão pergunta de novo.

## Copy-back

Valores por quadro (`cpudiag-1`, `VICTORY_COPY_COUNTS=1`):

- **Compare:** 69 chamadas de `sparse_back`, 2632 tiles (2406 de 64 KiB), 171 MB comparados. 2435 tiles idênticos (92,5%) e 197 diferentes.
- **Tempo:** compare 14,1 ms; leitura, merge e escrita 14,9 ms; total 29,6 ms de thread.
- **Volume escrito:** 9,0 MB. Dos tiles diferentes, 143 escrevem entre 16 e 64 KiB.
- **Custo por tile diferente:** ~76 µs para um tile de 64 KiB. Copiar 64 KiB custa uns 5 µs.
- **Como é o merge:** `merge_gpu_writes_mut` (`shadow.rs:2321`, chamado em `:2237`) lê o tile do título com `host::read_into`, varre palavras de 8 bytes contra o "antes" e cai num laço por byte nas palavras que diferem. Ele atualiza três buffers e depois escreve de volta.
- **No perfil:** `copy_back:580` (memmove) e `:572`.
- **Snapshot:** o "antes" é tirado em `Shadow::writing:523` (memmove, ~5% da thread).

**Por que difere do antigo** (2026-11-25: ~104 chamadas, ~7,9 mil tiles, ~515 MB, 74–84 ms):
- O STRETCH passou a maior parte das faixas para "in place": 105 M em place contra 20 M sparse em `copy paths`.
- O compare caiu para um terço e o copy-back para ~35 ms.
- A paralelização do compare (doc 04 e memória de 2026-11-25) atacava 41–44 ms de compare. Hoje são 14 ms: não é mais o gargalo, e não deve ser repetida.

## Preparo e esperas

Perfil (`cpudiag-1`), proporção das amostras da thread que submete:

| O quê | Amostras | Onde |
|---|---|---|
| esperar a GPU | ~33% | `Host::wait_fence:1165`, chamado pela timeline: `finish_owed` e `wait_in_flight` antes de cada Signal/Work (`timeline.rs:184`), e pelo `queue::execute` do título |
| lock `submitted` disputado | ~10% | `execute_inner:1237`: a thread do título esperando o lock que `wait_fence` segura enquanto dorme no evento da GPU |
| drain no começo da submissão | ~2% | `execute_inner:1241`, `drain_in_flight_locked` |
| bindless plans | ~4% | `HostHeap::bindless_plan:1675-1828` |
| snapshots, copy-back e cópias | ~15% | memmove e `victory_safe_copy` |
| guards | ~4% | `watch::guard` e `walk` |
| `xmem::map_alias` | ~2% | `NtMapViewOfSectionEx` / `UnmapViewOfFile2` e o lock de `map_alias:824` |

**Leitura:**
- O async só sobrepõe a GPU ao trabalho do título entre duas submissões. Cada Signal do título espera a submissão em voo.
- Cada submissão começa drenando a anterior, porque os snapshots são por cópia de host, não por submissão.
- `wait_fence` segura `submitted` durante `WaitForSingleObject`. Enquanto isso, a próxima submissão do título fica parada no lock, pelo tempo em que a GPU roda.
- O mesmo defeito foi descrito no PR #258 do Belobug ("wait_fence sem o lock"), com 12% da thread no lock lá. Aqui são ~10%.
- **`JOIN_PARTS` do Belobug:** depende da arquitetura "line" e `ahead_of_writes`, que não temos. Lá rendeu 0,6 a 3 ms por quadro.
- **`THIN`:** move a tradução das listas para threads da runtime. É uma mudança grande e não está no nosso caminho crítico medido.

## Oportunidades, em ordem de prioridade

### 1. Copies: não perguntar de novo o que já se sabe limpo nesta janela

- **Onde:** `shadow.rs` (`update_lazily`, `update_free`, `clean_spans`), `watch.rs` (`writes_guarded`, `region_epoch`) e `host.rs` (`prepare_bindless`).
- **Hipótese:** a mesma faixa é checada a cada submissão (5 a 20 vezes por quadro), e 96% dos bytes checados não mudaram. Um registro "limpo até o epoch E" para todas as faixas, não só as com guard, dispensaria a checagem enquanto o epoch não muda. Também cabe deduplicar faixas sobrepostas dentro de uma submissão.
- **Antes, instrumentar:** contadores de acertos e erros de `clean_spans`; repetições da mesma faixa por quadro, com o tamanho; e o motivo do custo da faixa de 10,8 ms (bytes copiados por refresh).
- **Ganho potencial:** até ~10–15 ms de copies, se a maior parte das checagens repetidas puder ser pulada. Não medido.
- **Risco:** cópia velha se uma escrita escapar do epoch (kernel, alias, GPU). Os pins e os epochs atuais cobrem esses casos; os testes do `watch` precisam cobrir o atalho novo.

### 2. Copy-back: tornar o merge barato

- **Onde:** `shadow.rs`: `sparse_back:2146`, `merge_gpu_writes_mut:2321` e `merge_gpu_writes:2285`.
- **Hipótese:** 76 µs por tile diferente, contra ~5 µs para copiar 64 KiB. Os tiles diferentes têm muitas palavras alteradas (143 de 197 escrevem 16–64 KiB), e cada uma cai no laço por byte com dois desvios por byte. Um merge vetorizado com máscara (AVX2: `host != old`, depois blend em `title`) faria o mesmo resultado sem desvios por byte e sem threads novas.
- **Ganho potencial:** até ~10 dos 15 ms de leitura, merge e escrita.
- **Antes, instrumentar:** separar leitura, merge e escrita no contador `thread ms` e contar os bytes por tile que diferem de fato.
- **Risco:** um merge errado perde ou corrompe dados. Mitigação: teste diferencial contra o merge atual em tiles aleatórios e um modo de checagem no jogo, como `VICTORY_BACK_COMPARE_CHECK`.

### 3. Preparo: esperar a GPU sem segurar `submitted`

- **Onde:** `host.rs`: `wait_fence:1149-1170`, `wait_in_flight:1173` e `reap_completed`.
- **Hipótese:** com um evento por thread, a espera pela GPU sai do lock. O lock é tomado só para `reap_completed`, e não é tomado se outra thread já terminou a submissão. A ideia é a do PR #258 do Belobug, a ser escrita como código nosso. A submissão seguinte do título deixa de esperar o lock pelo tempo em que a GPU roda.
- **Ganho potencial:** parte dos ~10% de amostras em disputa. Na melhor hipótese ~10–20 ms por quadro, e só se a espera não for apenas deslocada para o drain do começo da submissão. Esse é o maior risco de o ganho sumir.
- **Antes, instrumentar:** tempo esperando `submitted` por quadro, por chamador (`execute_inner`, `wait_fence`).
- **Risco:** ordem de conclusão das submissões; quem volta de `wait_fence` precisa ver tudo até o valor terminado. Teste com WARP: uma thread espera e outra precisa conseguir tomar o lock.

## Critério de validade

Uma otimização vale se reduzir o frame time no benchmark, em pares A/B intercalados e nas mesmas condições, e se passar sem:
- divergência visual (capturas comparadas);
- perda de dados (testes e checagem diferencial);
- queda;
- perda de device;
- crescimento descontrolado de RAM ou VRAM (samples de commit).

## Resultado da oportunidade 3 (mesmo dia)

**Mudança** (`host.rs`):
- `Host::wait_fence` espera a GPU num evento auto-reset da própria thread (`own_event`: thread-local, fechado quando a thread termina), sem segurar `submitted`.
- Depois toma o lock só para `reap_completed`, e não toma se `FINISHED_FENCE` já chegou ao valor.
- Se não houver evento próprio, cai no caminho antigo: evento compartilhado, sob o lock.
- `execute_inner` e `drain_in_flight_locked` não mudam.
- A ideia é a do PR #258 do Belobug, escrita aqui como código nosso.

**Teste** `host::tests::waiting_for_the_card_leaves_submitted_free` (WARP, sem dados do jogo):
- a GPU só chega ao valor depois que a CPU abre um fence-portão;
- enquanto isso, outra thread precisa conseguir tomar `submitted`;
- passou 10 de 10; com o caminho antigo forçado, falha.
- Suíte `victory-gpu --lib`: 142 passaram, 0 falharam. Clippy limpo nas linhas novas.

**A/B no jogo:** três pares intercalados, A = `2be674c8`, B = A + mudança. Benchmark da metodologia acima, com capturas a cada 150 quadros do mundo. Medianas dos quadros com ≥ 4500 draws:

| Par | quadro A → B | preparo A → B | espera da CPU pela GPU A → B | copying back A → B |
|---|---|---|---|---|
| 1 | 237,4 → 225,1 ms | 105,0 → 101,8 | 11,8 → 22,1 | 36,5 → 37,2 |
| 2 | 238,9 → 216,1 ms | 93,3 → 94,1 | 11,3 → 21,2 | 34,0 → 36,5 |
| 3 | 246,5 → 241,6 ms | 95,3 → 101,9 | 12,4 → 27,6 | 37,1 → 36,6 |
| média | 240,9 → 227,6 ms (−13,3, −5,5%) | 97,8 → 99,3 | 11,8 → 23,6 | 35,9 → 36,8 |

- **Quadro:** B ficou menor nos 3 pares (−12,3, −22,8 e −4,9 ms).
- **Espera:** a espera pela GPU que sumiu do lock reaparece em parte no drain do começo da submissão ("on the GPU" dobra, +11,8 ms). O saldo no quadro é positivo: a thread do título não fica mais parada enquanto outra thread espera.
- **Draws:** ~6,4 mil por quadro neste dia, contra ~9 mil nas rodadas `pool-*` e `cpudiag-1`. Só os pares entre si são comparáveis.
- **Estabilidade:** nenhuma queda nem perda de device nas 6 rodadas. Commit no fim: 26,4–27,2 GB, igual nos dois lados.
- **Imagem:**
  - a diferença média entre builds diferentes (33,8, 34,1 e 3,8) ficou igual à diferença entre rodadas do mesmo build (34,8, 33,7 e 34,6);
  - as rodadas caem em momentos diferentes da cena, com o personagem e a câmera em outra posição;
  - a olho, nenhum artefato em B.
