# Fase 4: o problema atual, visto por inteiro

Estado em 25/11/2026, branch `dev-belobug`. Este documento junta o que
medimos, o que mudamos, o que deu errado e o que ainda não sabemos. A
intenção é decidir os próximos passos com menos tentativa e erro.

Convenção: **medido** = número tirado de uma rodada nesta máquina;
**lido no código** = conferido no fonte; **hipótese** = ainda sem prova.

## 1. Resumo

- O jogo chega ao mundo, roda 25 min sem cair e fica em **~2,4 fps
  (~410-420 ms por quadro)**. O mundo carrega em ~125-145 s.
- O gargalo **não é a GPU nem o jogo**. É o trabalho do VIctory na CPU
  para traduzir cada quadro do console para o PC:
  - ~170-236 ms preparando o quadro;
  - ~90 ms de GPU;
  - ~80 ms copiando de volta o que a GPU escreveu.
- Essas três partes acontecem **uma depois da outra**, então o quadro é
  aproximadamente a soma delas.
- A causa de fundo é arquitetural. No console, CPU e GPU enxergam a
  mesma memória (unificada). No PC, cada uma tem a sua. O VIctory precisa
  descobrir, a cada envio, o que o jogo mudou na memória, copiar para a
  GPU, e depois copiar de volta o que a GPU escreveu. Ver
  `02-xbox-series-vs-windows.md`.
- Os ganhos de outros forks (~180 ms/quadro alegados) **não se repetiram
  nesta máquina**. Cada fork é ajustado para o hardware do autor, e as
  linhagens de código são diferentes (portar ≠ fazer merge).

## 2. Onde estamos: números medidos

Máquina:
- Ryzen 7 3700X (Zen 2, mesma ISA do console);
- RX 9070 XT (RDNA4, driver 32.0.32029.2010);
- 32 GB DDR4-3600 (XMP ligado em 25/11);
- jogo no NVMe (`C:\Games`); dados em `C:\VictoryData`.

| Rodada (diag, 25 min salvo indicação) | Quadro | Preparo | GPU | Mundo em | Quedas |
|---|---|---|---|---|---|
| `dev` antigo (linhagem fxceless) | 590-615 ms | n/d | n/d | n/d | relay derrubava ~10 min |
| `dev-belobug`, RAM a 2133 | 439 ms | 232 | 95 | 273 s | 0 em 5 rodadas |
| `dev-belobug`, XMP 3600 | 413 ms | 228 | 86 | 125 s | 0 |
| `dev-belobug` + correção do pré-build mesh | 418 ms | 236 | 90 | 143 s | 0 |
| klbf-lab (LATE_WAIT=0) | 335 ms | n/d | n/d | 111-117 s | caiu 2/2 (culling) |
| vi-fx3 (myokex), relay off | ~490 ms | 137 | 153 | 131 s | caiu no driver (~1300 s) |
| `dev-belobug` com profiler (15 min) | 407 ms | 222 | 89 | 128 s | 0 |
| idem + bitmap no `watch` (save mais novo, ver §5) | 360 ms | 172 | 95 | 141 s | 0 |
| A/B 6 min, save congelado: base | 349 ms | 177 | 76 | 121 s | 0 |
| A/B 6 min, save congelado: bitmap no `watch` | 362 ms | 182 | 92 | 169 s | 0 |

A queda de 407 para 360 ms **não veio do bitmap**, e sim do save mais
novo (o jogo fez autosave, e a cena era outra). No A/B com o mesmo save
congelado, as duas versões ficam empatadas dentro do ruído.

## 3. Como um quadro passa pelo VIctory (resumo lido no código)

1. O jogo (código x86-64 nativo, rodando direto no Zen 2) grava listas
   de comandos D3D12.X e as envia. São ~100-120 envios por quadro no mundo.
2. Para cada envio, `victory-gpu`:
   - descobre quais intervalos da memória do jogo os comandos usam
     (buffers, texturas, descritores);
   - pergunta ao Windows quais páginas o jogo escreveu desde a última
     cópia. Usa `GetWriteWatch` ou, em memória de "views" físicas,
     páginas protegidas contra escrita com tratamento da exceção
     (`watch.rs`);
   - copia o que mudou para cópias no lado do host (`shadow.rs`);
   - monta os descritores bindless (`bindless_plan`);
   - tira um *snapshot* dos intervalos que os shaders podem escrever.
3. Envia para a GPU e **espera** terminar.
4. Compara o snapshot com o resultado e copia para a memória do jogo só o
   que a GPU escreveu (`copy_back`), porque o jogo lê esses dados na CPU.
5. Shaders: o binário nativo do console (ISA RDNA2) é traduzido para DXIL
   (`victory-shader`), e o driver AMD compila o pipeline. O pré-build e a
   biblioteca de pipelines evitam compilar durante o jogo.

## 4. Para onde vai o tempo da CPU (profiler, rodada devbel-prof-1)

Amostras da thread que envia (11.419 amostras em 15 min):

| Onde | Parte | Comentário |
|---|---|---|
| Snapshot antes do envio (`Shadow::writing`, memmove) | ~12% | cópia de intervalos que a GPU pode escrever |
| Comparação/cópia de volta (`Shadow::copy_back`) | ~12% | |
| Espera da GPU dentro do envio (`execute`) | ~12% | o quadro é serial |
| Planos bindless (`bindless_plan`) | ~10% | |
| Trava global no `watch::guard` | ~7,5% | fila num mutex, `VirtualProtect` com o lock preso |
| Cópias seguras da memória do jogo | ~6% | |

Contadores por quadro (`copystat`):
- ~103 envios por quadro;
- ~12 mil consultas de escrita, somando 664 ms (em várias threads);
- **11 GB de memória do jogo conferidos, 10 GB sem mudança**;
- buffers de 50-75 MB atualizados 12-13 vezes por quadro.

O modo "card buffers" do Belobug, que deixa buffers grandes na VRAM e
compara as páginas na GPU, foi feito para eliminar o snapshot e a
comparação. Nesta máquina ele **nunca ligava**. Investigado (commits
9f07c9d e 84e507c):

- **Causa:** `CardState::try_new` só é chamada pelos caminhos "buffer" e
  "loose" de `shadow::find_copy`. A cópia esparsa (blocos de 2 GiB, com
  `card: None` fixo) é tentada antes e atende tudo o que não é usado no
  lugar. Numa rodada de 4 min: no lugar 20,6 M, esparso 5,1 M,
  buffer/loose 0, recusas 0. Os ~24% de snapshot e comparação do
  profiler são `sparse_writing` e `sparse_back`.
- **A/B com `VICTORY_SPARSE=0`** (só diagnóstico; os card buffers passam
  a ser usados), mesmo save, 6 min, mediana dos últimos 25 registros:

  | | Quadro | Cópias | Planos | GPU | Cópia de volta | RAM |
  |---|---|---|---|---|---|---|
  | esparso ligado | 342 ms | 52,9 | 25,6 | 71,7 | 84,4 | 25,3 GB |
  | esparso desligado | 375 ms | 63,0 | 38,3 | 86,3 | 81,9 | 28,9 GB |

  - Vão para a VRAM 410 das 1.024 cópias; o resto é recusado por tamanho
    (< 4 MiB), orçamento de VRAM (75%) e teto de 2 GiB.
  - O snapshot esparso some, mas o custo reaparece em cópias, planos e
    GPU, e a cópia de volta não muda. A imagem é igual nas duas rodadas.
- **Conclusão:** ligar os card buffers como estão **não reduz** o tempo
  por quadro. O custo real não é o memmove do snapshot isolado, e sim
  quanto o jogo escreve pela GPU e precisa voltar para a CPU (~80 ms de
  cópia de volta nos dois casos).

## 4.1 Cópia de volta: o que volta da GPU é lido pela CPU? (backread)

Instrumentação só de diagnóstico (`crates/victory-gpu/src/backread.rs`). O
comportamento padrão não muda.
- **Sempre ligada nos builds diag:** a linha `back reads:` a cada 5 s, com
  bytes, escritas e blocos distintos de 64 KiB copiados de volta por
  origem: cópias do `Shadow` (`sparse_back`, `copy_back`, card),
  readbacks pedidos pelo jogo e texturas do `texback`.
- **`VICTORY_BACK_READS=<n>`:** um bloco em `n` (escolhido por hash do
  endereço, então são sempre os mesmos) recebe `PAGE_GUARD` logo depois da
  cópia de volta, em todos os endereços conhecidos da mesma página física
  (até 15).
  - O primeiro acesso de qualquer código da CPU dispara a exceção de
    guarda. Um VEH anota quem acessou (jogo, VIctory ou DLL de sistema,
    subindo a pilha), se foi leitura ou escrita e depois de quantos
    quadros e ms. Em seguida tira a guarda do resto do bloco e deixa o
    acesso seguir.
  - Bloco não tocado em 30 quadros conta como "untouched"; se a guarda
    sumiu antes disso, como "lost".
  - Acessos que não são do jogo saem com a pilha, como
    `victory.exe+offset`, nomeada offline com o script do apêndice A.

Rodadas no save congelado, 6 min, `VICTORY_HOST_PADS=0`, sparse ligado:

| Rodada | ms/quadro | cópia de volta | MB/quadro de volta |
|---|---|---|---|
| base `devbel-sparse-on` (sem amostragem) | 342 | 84 ms | 36 |
| `backreads-1` (n=64) | 339 | 80 ms | 34 |
| `backreads-2` (n=8) | 352 | 88 ms | 33 |

O que mediram:
- **Volume:** por janela de 5 s (~15 quadros), ~520-650 MB copiados de
  volta em ~12-14 mil escritas, sempre nos mesmos **~800 blocos de 64
  KiB (~52 MB)**. Nos 6 min foram 1.427 blocos distintos.
  - Readbacks do jogo: ~0 MB (4 blocos). Texturas `texback`: 0 nesta cena.
  - A etapa compara ~600 MB por quadro (`sparse writes since`: 9,2 GB
    por janela) para gravar ~34 MB. **O custo é a comparação, não a
    escrita.**
- **Quem lê primeiro (n=8):** 15.386 blocos acompanhados, 179 distintos
  (12,5% dos copiados de volta).

  | Primeiro acesso | Contagem |
  |---|---|
  | jogo (leitura ou escrita) | **0** |
  | VIctory (leitura) | 14.660 |
  | sem acesso em 30 quadros | 644 |
  | guarda perdida | 59 |

- **Pilha do acesso do VIctory, 100% dos casos:** `Shadow::update_free →
  copy_stale → Shadow::copy → host::copy_mapped → host::read →
  victory_safe_copy`. Em 96% vem do `update_free` direto e em 4% de um
  segundo `copy_stale`.
- **Quando:** 70% no mesmo quadro e 30% no seguinte. Por tempo: 4% em
  menos de 10 ms, 72% entre 10 e 100 ms, 24% entre 0,1 e 1 s.

**Por quê (lido no código):**
- `sparse_back` e `copy_back` chamam `watch::writing` antes de gravar.
- Essa função fixa o bloco como escrito por `PIN` = 2 s ("every check
  reports them written").
- No próximo `update_free` da mesma cópia, `writes_guarded` o devolve
  como escrito e `copy_stale` relê a **página inteira** da memória do
  jogo para a cópia na GPU, que acabou de ser a origem desses bytes.
- Como os ~800 blocos voltam a cada quadro, eles ficam fixados o tempo
  todo. **É um eco:** a escrita do VIctory volta para o VIctory antes que
  o jogo toque nela.

**Limites da medição:**
- Depois do eco a guarda já saiu, então leituras do jogo depois do eco
  (tipicamente 10-100 ms após a cópia) não são vistas.
- "0 leituras do jogo" vale para a janela entre a cópia de volta e o eco,
  e para os 644 blocos sem acesso em 30 quadros.
- Acesso pelo kernel a página guardada falha uma vez e conta como "lost".
- Leituras da GPU sobre memória usada no lugar não são leituras da CPU.
- A granularidade é de 64 KiB.
- Um endereço da página que o runtime não nos contou escapa.
- Com a guarda ativa, `run_of` (janelas) e `watch::writable` veem a
  proteção com o bit de guarda. É só com a variável ligada.
- Custo: n=64, 0,17 s em 6 min (nenhum efeito no quadro); n=8, 8,6 s
  (+3% no quadro).

**Tamanho do eco (rodada `backreads-3`, n=8, guarda rearmada 2 ms depois
de cada acesso nosso, contadores `taken again since` sempre ligados no
diag):**
- **Bytes:** por janela de 5 s (mediana dos últimos 25 relatórios), 459 MB
  de eco e 1.878 MB de outras releituras.
  - O eco é **~20% do que a etapa de cópias relê** (~40 de ~200 MB por
    quadro), algo como 9-10 ms dos ~46 ms dessa etapa, por proporção.
  - **Não explica os ~80 ms da cópia de volta,** que são a comparação de
    ~600 MB por quadro.
- **O jogo continua sem tocar:** 1.036 acompanhamentos (172 blocos
  distintos), 74 mil rearmes, **0 acessos do jogo em qualquer momento** e
  0 depois de um acesso nosso.
  - 935 acompanhamentos chegaram aos 30 quadros só com acessos nossos.
  - 16,6 mil escritas anunciadas (`watch::writing`: cópia de volta e I/O)
    tiraram a guarda antes de escrever.
- **O eco não é idêntico:** dos 1.915 MB de eco comparados (blocos
  amostrados, antes de reler), **6,9 MB diferem (0,36%)**.
  - Esses blocos não tiveram acesso do jogo pela CPU depois da cópia de
    volta. A causa mais provável, lida no código, são escritas do jogo
    **durante** o trabalho da GPU: o merge leva essas escritas para a
    memória do jogo, mas não para a cópia na GPU.
  - Uma alternativa a descartar: bytes da GPU fora dos intervalos
    pendentes.
- **Custo:** com rearme, a medição pesa (434 ms/quadro contra 342; 27 s
  em 6 min). As contagens valem, mas os tempos desta rodada não servem
  para comparar.

**Conclusão sobre o candidato:**
- "Não reler o que a própria cópia gravou" **não é seguro** como estava
  escrito: perderia os 0,36% acima.
- Além disso, enquanto o bloco está fixado por `watch::writing` (2 s,
  sem guarda), escritas do jogo nele não ficam registradas. O eco é o
  preço dessa fixação.
- Uma versão segura precisaria de duas mudanças juntas:
  1. gravar na cópia da GPU o resultado do merge na hora da cópia de
     volta;
  2. para as nossas escritas síncronas, recolocar a guarda logo depois,
     em vez de fixar por 2 s.
- Ganho esperado: da ordem de 10 ms por quadro, pequeno perto dos 80 ms
  da comparação.

**Próximo experimento:**
- Contar, dentro de `sparse_back`, quantos bytes do merge diferem da cópia
  na GPU logo após a cópia de volta.
- Se bater com os 0,36%, a explicação se confirma.
- Depois, medir onde a comparação de ~600 MB por quadro encontra
  diferença (quantos tiles comparados mudam de fato), que é o alvo maior.

## 4.2 A comparação da cópia de volta: quanto muda de fato

Instrumentação só de diagnóstico, sem variável:
- `backread::Tally`: soma por chamada de `sparse_back` e do `copy_back`
  fora do sparse, e soma uma vez no total ao fim da chamada.
- Aproveita o resultado da comparação existente (`host == old`, um
  `memcmp` que para na primeira diferença) e o intervalo sujo que o merge
  já devolve. Não faz segunda leitura.
- Linha `copy-back compare per frame:` a cada 5 s.

Rodada `compare-1` (save congelado, 6 min, `VICTORY_HOST_PADS=0`,
configuração normal), últimos relatórios, por quadro:

| Medida | Valor |
|---|---|
| chamadas / tiles comparados | ~104 / ~7.900 (quase todos de 64 KiB) |
| volume comparado | ~515-520 MB |
| tiles idênticos | 89-91% (~7.100 tiles, ~465 MB) |
| tiles diferentes | 9-11% (~700-840 tiles, 46-54 MB, **limite superior** dos bytes alterados) |
| gravado de volta na memória do jogo | 33-40 MB |
| gravado por tile diferente | 0 B: 0; ≤64 B: ~24; ≤1 KiB: ~39; ≤4 KiB: ~26; ≤16 KiB: ~45; <64 KiB: 490-590; 64 KiB: 85-110 |
| tempo somado das chamadas | 79-89 ms |
| comparar | 41-44 ms |
| ler + merge + gravar os diferentes | 37-43 ms |
| tempo de parede da etapa ("copying back") | 82 ms |

O caminho fora do sparse não apareceu (0 chamadas): na configuração
normal, toda a cópia de volta passa por `sparse_back`.

Leitura:
- **A etapa roda, na prática, numa thread só.**
  - O tempo somado das chamadas (79-89 ms) é igual ao tempo de parede
    (82 ms).
  - O `in_parallel` de 8 threads agrupa por `Shadow`, e todas as cópias
    sparse caem na mesma raiz por pedaço de 2 GiB: um grupo, uma thread.
- **90% da comparação não encontra nada.** ~465 MB por quadro são
  comparados para concluir "igual".
  - A comparação anda a ~12 GB/s, perto da banda de uma thread lendo dois
    fluxos.
  - Ou seja, o custo é volume, não implementação.
- **Os tiles diferentes custam caro por tile:** ~52 µs cada, ~1,2 GB/s.
  - Nesse tempo entram `read_into` de até 64 KiB, o merge palavra a
    palavra, `watch::writing` (trava global do watch) e `write_mapped`.
  - Ainda não está separado qual dessas partes pesa.
- **A granularidade não é a alavanca.**
  - Os tiles são quase sempre de 64 KiB inteiros. A comparação de um tile
    diferente para cedo; a de um tile igual não tem como parar cedo.
  - A gravação já se limita ao intervalo sujo: 33-40 MB gravados contra
    46-54 MB de tiles diferentes, e a maioria grava menos que o tile.
  - Tiles menores não reduziriam os bytes comparados.

Oportunidades (não implementadas), da mais barata para a mais cara:
1. **Paralelizar `sparse_back` por tile.**
   - Comparação e merge são independentes por tile; hoje uma thread faz
     tudo.
   - Mesmas comparações e mesmo resultado: 8 threads poderiam levar os
     ~80 ms para perto do limite de banda da memória.
   - Cuidados:
     - a trava `before`, mantida durante a chamada inteira;
     - o buffer `title`;
     - a ordem entre intervalos sobrepostos;
     - `watch::writing` (trava global).
2. **Separar o custo dos ~770 tiles diferentes** em `read_into`, merge,
   `watch::writing` e `write_mapped`, antes de mexer neles.
3. **Não comparar tiles que a GPU não pode ter escrito.**
   - Os intervalos declarados graváveis (UAVs) são muito maiores que o
     escrito: 90% iguais.
   - Saber quais tiles cada envio escreve, por recurso e faixa, cortaria a
     maior parte do volume.
   - É a mudança de maior ganho e a de maior desenho.

Overhead e limites desta medição:
- **Etapa da cópia de volta:** 82 contra 84 ms na base, dentro do ruído.
  Duas leituras de relógio por tile: ~8 mil tiles × ~25 ns ≈ 0,5 ms por
  quadro de tempo de thread.
- **O quadro subiu (383 contra 342 ms) por causa dos contadores sempre
  ligados das etapas anteriores**, não desta. A etapa de cópias foi de 53
  (base) para 58-62 ms com `copied()` (`f32d849`) e para 72 ms com
  `retaken()` (`dad44fc`, mutex por tile). Esses contadores deveriam
  passar a ser opcionais antes de novos A/B de tempo. Desde `6f7f4c1` eles
  só contam com `VICTORY_COPY_COUNTS=1` (só no diag, desligado por padrão).
- **A cena variou entre as rodadas** (3.800 draws contra 5.000 na base),
  então volumes absolutos de MB por quadro não são estritamente
  comparáveis entre elas.

## 5. Linha do tempo: melhorias e percalços

**Fase 3 (24/11):** mundo alcançado em ~350 s, 0,3-1,9 fps, 600-1.100
pipelines na fila do driver. O quadro já era dominado pela CPU.

**Início da fase 4 (24-25/11):**
- **Crash no compilador AMD** (`amdxc64.dll+0x861b9f`): dois padrões de
  reuso de valores na passagem de otimização DXIL. Corrigido (4b5ef6e).
  Um terceiro (TreeAnimVelocity) foi resolvido depois pela PR #452.
- **Base fxceless:** jogável no prólogo, mas 590-615 ms/quadro e
  interiores pretos. O "relay" (enviar adiantado) derrubava o
  dispositivo por volta dos 10 min, numa falha de página no dispatch de
  culling de meshlets. Desligamos o relay por padrão.
- **Troca de base para o Belobug** (mesma GPU que a sua, outra linhagem):
  439 ms, interiores iluminados, 0 quedas em 5 rodadas. Foi a melhor
  experiência até então.
- **Jogo no NVMe e XMP ligado:** o mundo passou de 273 s para 125 s;
  quadro de 439 para 413 ms. A RAM estava a 2133 MT/s sem o XMP.
- **klbf-lab (FSR, pré-build, água):** 335 ms, mas caiu 2/2 no mesmo
  dispatch de culling, com janelas roxas, paredes faltando e personagens
  quebrados. O pré-build dele dava segfault em dois shaders.
- **Pré-build que não carregava:** os pipelines mesh precisavam de
  `PRIMITIVE_TOPOLOGY` explícito na descrição para casar com a biblioteca
  D3D12 (7f51413). Depois disso, só 16 pipelines eram montados durante o
  jogo (antes ~1.000), e o mundo carregava mais rápido.
- **Pasta virtualizada do app Claude (MSIX):** processos iniciados pelo
  app viam o AppData redirecionado para dentro do pacote. Os dados foram
  movidos para `C:\VictoryData`.
- **vi-fx3 do myokex:** o `warm` dele dá segfault antes de gravar os
  pipelines; com relay o dispositivo caiu aos 148 s; sem relay, ~490 ms
  e queda no driver AMD aos ~22 min. A preparação dele é menor (137 ms),
  mas GPU e cópia de volta são maiores.
- **Profiler (§4):** primeira vez que medimos onde vai a CPU, em vez de
  adivinhar.
- **Bitmap no `watch` (branch `exp/watch-bitmap`, fora da
  `dev-belobug`):** troca dos conjuntos de blocos (`BTreeSet`) por bits
  por região de 64 MiB, mesma trava e mesma ordem de operações. 125
  testes passam (2 novos).
  - No A/B com o mesmo save, o tempo das consultas de escrita caiu
    ~15-20% (403-428 → 322-377 ms por quadro, somado entre threads).
  - O tempo por quadro não mudou (349 × 362 ms).
  - Lição: a fila no mutex do `watch` não estava no caminho crítico do
    quadro.
- **Entrada simulada parou de funcionar:** com um controle XInput
  conectado, o teclado deixa de funcionar como controle, por desenho, e
  o `VICTORY_KEYS` (Enter → A) não chega. Três rodadas de 15 min ficaram
  na tela inicial, inclusive com o executável antigo. Foi criada a chave
  de diagnóstico `VICTORY_HOST_PADS=0` (commit 809a24f).
- **Otimizações de shader pedidas:**
  - **alloca → SSA:** nenhum dos 26.503 shaders usa scratch, então não
    há `alloca` a promover;
  - **Wave32:** na 9070 XT nada roda dividido em metades de 32;
  - **leituras de constantes com endereço dinâmico:** saem uma por
    dword, então juntá-las é o único item com ganho possível.

## 6. Armadilhas de método que custaram tempo

1. **Números de outras máquinas não valem aqui.** RX 7800 XT, 9800X3D,
   NVIDIA: cada fork otimiza o próprio gargalo.
2. **O save muda entre rodadas.** O autosave leva a próxima rodada para
   outra cena e invalida a comparação. Solução: uma cópia congelada do
   save para cada A/B (`data-ab-snap`), restaurada antes de cada rodada.
3. **A entrada por script depende de não haver controle conectado.** Use
   `VICTORY_HOST_PADS=0` nas rodadas automáticas.
4. **diag ≠ release:** a build diag entra no mundo ~100 s mais tarde.
   Compare sempre diag com diag.
5. **Rodadas de 15-25 min por hipótese são caras.** O mundo só aparece
   depois de ~130 s, então cada medição custa no mínimo ~5 min de jogo.
6. **Comparar linhagens diferentes** (fxceless × Belobug) exige portar à
   mão. Merges geraram regressões grandes (590 ms).

## 7. Problemas em aberto, em ordem de impacto esperado

| # | Problema | Evidência | Próximo passo barato |
|---|---|---|---|
| 1 | Snapshot e comparação na CPU (~24% da thread) | profiler, A/B, backread | card buffers não ajudam (§4). A cópia de volta compara ~600 MB por quadro para gravar ~34 MB, e o que volta é relido pelo próprio VIctory (eco do `watch::writing`), sem leitura do jogo vista (§4.1). O eco é ~20% das releituras (~10 ms/quadro) e não é idêntico (0,36%). A comparação: 90% dos ~515 MB/quadro são tiles iguais, e a etapa roda numa thread só (§4.2). Próximo: paralelizar `sparse_back` por tile (mesmo resultado) |
| 2 | Quadro serial (preparo → GPU → cópia de volta) | profiler, fps line | só depois de 1: sobrepor o preparo do envio N+1 com a GPU do N exige resolver as quedas do relay |
| 3 | Consultas de escrita: 11 GB conferidos por quadro | copystat | o bitmap não mudou o quadro (A/B); só vale atacar se o problema 1 sair do caminho antes |
| 4 | `bindless_plan` (~10%) | profiler | ler as linhas quentes 1672-1816 |
| 5 | Leituras de constantes dinâmicas, uma por dword | contagem do pack | só se uma rodada com `VICTORY_GPU_TIMING` mostrar esses shaders entre os caros |
| 6 | Queda no culling com relay | klbf, fxceless | candidato: Belobug 78028ad (sem prova) |

## 8. Como trabalhar de forma mais inteligente daqui em diante

- **Medir antes de mexer, uma hipótese por rodada,** com o mesmo save
  congelado, `VICTORY_HOST_PADS=0` e a mesma duração.
- **Rodadas curtas:** 6 min (entra no mundo e mede ~4 min) bastam para
  comparar o tempo por quadro. Rodadas de 25 min só para estabilidade.
- **Um script de A/B único** que roda base e candidato e imprime uma
  tabela de 5 linhas (ms, preparo, GPU, consultas, quedas), para que
  cada comparação custe uma leitura curta.
- **Reproduzir fora do jogo sempre que possível:** testes de unidade e
  microbenchmarks para `watch`/`shadow`. A ideia de maior retorno é
  gravar os envios de um quadro e reexecutá-los sem o jogo, medindo o
  preparo em segundos em vez de minutos. Ainda não existe e exigiria
  desenho próprio.
- **Ler o profiler pelo topo:** as 10 primeiras linhas de cada seção
  bastam; o arquivo inteiro raramente muda a conclusão.

## Apêndice A. Nomear endereços de um log (offline)

Salvar como `simbolizar-pilha.py` fora do repositório (a política de
assets não aceita `.py` aqui) e rodar com `python -I`.

```python
"""Nomeia, offline, os endereços que o VIctory registra num log de diagnóstico.

Uso: python -I simbolizar-pilha.py <victory.exe> <endereço de victory_safe_copy+6> <endereços...>
Endereços no formato `victory.exe+0x...` (linha "first accesses not by the
title") são nomeados direto. Endereços absolutos precisam que o primeiro deles
esteja dentro de `victory_safe_copy`: ele fixa a base de carga da execução (a
base é alinhada a 64 KiB).
Usa o dbghelp.dll do Windows e o .pdb ao lado do .exe; precisa ser o mesmo
build que rodou.
"""
import ctypes, sys
from ctypes import wintypes as w
dh = ctypes.WinDLL(r"C:\Windows\System32\dbghelp.dll")
exe = sys.argv[1]; args = sys.argv[2:]
h = w.HANDLE(0x1234)
dh.SymSetOptions(0x2)  # UNDNAME; symbols loaded at once
dh.SymInitializeW.argtypes=[w.HANDLE, w.LPCWSTR, w.BOOL]
assert dh.SymInitializeW(h, None, False)
dh.SymLoadModuleExW.restype = ctypes.c_uint64
dh.SymLoadModuleExW.argtypes=[w.HANDLE, w.HANDLE, w.LPCWSTR, w.LPCWSTR, ctypes.c_uint64, w.DWORD, ctypes.c_void_p, w.DWORD]
B = 0x140000000
base = dh.SymLoadModuleExW(h, None, exe, None, B, 0, None, 0)
assert base, ctypes.GetLastError()
class SI(ctypes.Structure):
    _fields_=[("SizeOfStruct",w.ULONG),("TypeIndex",w.ULONG),("Reserved",ctypes.c_uint64*2),("Index",w.ULONG),("Size",w.ULONG),("ModBase",ctypes.c_uint64),("Flags",w.ULONG),("Value",ctypes.c_uint64),("Address",ctypes.c_uint64),("Register",w.ULONG),("Scope",w.ULONG),("Tag",w.ULONG),("NameLen",w.ULONG),("MaxNameLen",w.ULONG),("Name",ctypes.c_wchar*1024)]
def byname(n):
    s=SI(); s.SizeOfStruct=88; s.MaxNameLen=1024
    assert dh.SymFromNameW(h, n, ctypes.byref(s)), n
    return s.Address
def name(a):
    s=SI(); s.SizeOfStruct=88; s.MaxNameLen=1024; d=ctypes.c_uint64()
    if not dh.SymFromAddrW(h, ctypes.c_uint64(a), ctypes.byref(d), ctypes.byref(s)): return hex(a)
    return f"{s.Name}+{d.value:#x}"
offsets = [a for a in args if a.startswith("victory.exe+")]
for a in offsets: print(a, name(B + int(a.split("+")[1], 16)))
addrs = [int(a, 16) for a in args if not a.startswith("victory.exe+")]
if addrs:
    rva = byname("victory_safe_copy") - B
    runbase = (addrs[0] - rva) & ~0xFFFF
    print("run base", hex(runbase))
    for a in addrs: print(hex(a), name(a - runbase + B))
```
