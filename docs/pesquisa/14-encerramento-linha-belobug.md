# 14 — Encerramento da linha `dev-belobug` e o que levar para a base nova

Data: 2026-11-26. Último commit da linha: `0deb4165` (no remoto `Gabriel-Ramaldes/victory-fork`, branch padrão).

## Por que encerrar

- **Mudança de linhagem:**
  - O Belobug saiu da própria linha (`71652a2` → `belobug/main` `4f62c2a5`, parada desde 25/11).
  - A linha nova é `cd8dfc0`: avatarmanz/VIctory-7900XTX `c9c932c` mais as correções da RX 9070 XT (`e942896`).
  - Nossa `dev-belobug` ficou sobre uma linha que ninguém mais mantém (auditoria em [13](13-auditoria-scarlett-megafork.md)).
- **Imagem nos forks dessa linha nova, na gravação do usuário nesta máquina:**
  - forks: rorodev6/victory-megafork-amd (sobre `cd8dfc0`) e Op-lite/victory-scarlett (sobre o cool_fox);
  - texturas completas e melhores, cabelo do protagonista, reflexos e luzes;
  - fps parecido (no máximo 5) com contagem de draws próxima (~6.000).
- **Decisão do usuário (2026-11-26):** escolher uma das duas como base pelo desempenho medido nesta máquina e guardar aqui o que esta linha ensinou.

## O que esta linha mediu (3700X + RX 9070 XT, Windows 11)

### Onde vai o quadro

- O quadro é preso pela CPU: ~210–230 ms, com a GPU ocupada ~71 ms (medido com `VICTORY_GPU_TIMING=1` + `VICTORY_D3D12_DRED=light`, que tornam os envios síncronos, então só como proporção; doc 11).
- O preparo (copies, bindless plans e copy-back) é a maior parte.
- A GPU entra no caminho crítico pelas esperas entre envios: cada envio drena o anterior.
- A trava do watch (`watch::guard`) não estava no caminho crítico: os bitmaps cortaram 15–20% do tempo de watch e o quadro não mudou (doc 04).

### Desperdício nas cópias

- ~83% das cópias de refresh não mudam nenhum byte (`VICTORY_COPY_DIFFERS=1`, doc 11).
- **Maior fonte, o eco das escritas da GPU (~200–280 MB por quadro):**
  - o copy-back de uma janela STRETCH marca no journal a faixa declarada inteira;
  - marca também todas as vistas das mesmas páginas físicas;
  - toda cópia que cobre um alias copia a página de novo.
- **Janelas STRETCH menores não resolvem** (doc 12): a 64 KiB houve 4.303 janelas e nenhuma recusa, mas o eco não mudou e o commit subiu 1 GB.
- **Por que comparar antes de copiar não paga:** em heaps `CUSTOM / WRITE_BACK / L0` comparar custa o mesmo que copiar.

### Ganhos medidos (cada um no seu cenário, não somam)

| Mudança | Efeito |
|---|---|
| STRETCH: vistas de 64 KiB usadas in place (`bc85525`) | release: 350 → 258 ms por quadro, com ~9% menos draws |
| Async submit (`19656a0`) | 226 → 203 ms (release) |
| Pins só enquanto a escrita dura (`283665d`) | 228 → 217 ms, menor em 3 de 3 pares |
| `wait_fence` fora do lock (`3124244`) | 241 → 228 ms, menor em 3 de 3 pares |
| Bindless plans pulando janelas mortas (`e09a57e`) | plans 36 → 25 ms; quadro dentro do ruído |
| Threads mantidas no `in_parallel` (`2be674c`) | 298 → 1,3 µs por chamada; quadro dentro do ruído |
| Páginas de borda lembradas (`7c51326`) | copies −6 ms; quadro ainda não provado |

### O que não deu ganho (não repetir sem motivo novo)

- card buffers: as cópias esparsas tomam o lugar deles; ligados, sem ganho;
- bitmaps de write-watch;
- comparação do copy-back em duas threads: idêntica bit a bit, ~1 ms;
- epochs de limpeza mais finos (simulado);
- janelas STRETCH menores;
- `VICTORY_SPARSE=0` como configuração de jogo;
- tiles in place: o Belobug mediu que removem o device; não tentado.

## Método que vale para qualquer base

- **Save fixo para A/B:** o Manual Save da sacada do usuário (`data-sacada-snap`), restaurado antes de cada rodada. A câmera ainda pode começar em outro lugar, então confira a contagem de draws antes de comparar.
- **Pares intercalados:** no mínimo 3 pares, comparando medianas por etapa. O ruído entre rodadas é de ~10–15%.
- **Diagnóstico não é benchmark:** rodadas com contadores (`VICTORY_COPY_COUNTS=1`, `VICTORY_COPY_DIFFERS=1`, `VICTORY_GPU_TIMING=1`) servem para entender, não para medir quadro.
- **Uma coisa por vez:** só um jogo ou build pesado rodando; outra sessão de IA pode usar a máquina.
- **Medir antes de adotar:** números de outras máquinas (9800X3D, 12900K, 7900 XTX, RTX 4080) não valem para esta.

## Ideias que talvez sirvam na base nova

Cada uma só vale depois de medir na base nova se o problema também existe lá:

1. **Marcar como escrito pela GPU só o que mudou** (doc 12, "próximo passo"), se a base nova também tiver o eco de aliases. O `KNOWN_WRITES` do Scarlett não poupa aliases.
2. **`wait_fence` fora do lock:** esperar o fence com um evento por thread, fora do mutex de envios. Verificar se a base nova segura um lock durante essa espera.
3. **Threads mantidas** em vez de criar threads a cada chamada paralela.
4. **Páginas de borda:** cópia parcial de uma página lembrada com sua faixa e carimbo, se a base nova copiar páginas parciais de novo a cada refresh.
5. **Ferramentas:** `VICTORY_CAPTURE_WORLD`, `VICTORY_SKIP_MOVIES`, `VICTORY_HOST_PADS=0` e a janela do título escolhida pelo tamanho (`733d938`), se faltarem na base nova.

## Onde estão os detalhes

- Docs 01–12 desta pasta (pt-BR).
- README da branch, seção "Performance work on this branch".
- Auditoria das linhagens: doc 13 desta pasta.

## Medição da escolha da base (2026-11-26, nesta máquina)

- **Método:**
  - cada fork com as variáveis exatas do próprio launcher, relay incluído (`VICTORY_RELAY=2`, decisão do usuário);
  - build `diag`, shaders traduzidos antes, cache próprio por fork;
  - save da sacada restaurado a cada rodada, 300 s;
  - uma rodada de aquecimento de cada, depois 2 medidas intercaladas (MS SC MP MP SC MS);
  - mediana das linhas de quadro com 2.000 draws ou mais, descartando o primeiro quarto.
- **Configurações:**
  - MS: Megafork `8e0002e`, `play-amd.ps1 -Profile Stable` (Series S);
  - MP: o mesmo com `-Profile Performance` (Series S, `VICTORY_THIN=1`);
  - SC: Scarlett `835ca60`, `play-scarlett.ps1` padrão (Series X, `VICTORY_PROBE_RAYS=1`, sem hooks).

| Rodada | Quadro | fps | Preparo | Copies | Copy-back | Draws | Resultado |
|---|---|---|---|---|---|---|---|
| MS aquec. / 1 / 2 | 104,3 / 104,8 / 111,6 ms | 9,0–9,6 | 75–79 ms | 30 ms | 24 ms | 6,2 mil | 3 de 3 sem falha |
| MP aquec. / 1 / 2 | 100,0 / 96,5 / 93,3 ms | 10,0–10,7 | 77–81 ms | 36–38 ms | 23–25 ms | 6,3–7,9 mil | 3 de 3 sem falha |
| SC aquec. / 1 / 2 | 95,9 / 116,5 / — ms | 8,6–10,4 | 60 ms | 29 ms | 21 ms | 6,5–7,2 mil | **2 de 3 caíram** (263 s e 110 s) |
| `dev-belobug` (rodadas anteriores, sem relay) | ~205 ms | ~4,9 | ~84 ms | ~25 ms | ~37 ms | ~6,3 mil | — |

- **Scarlett:** as duas quedas são `0xc0000005` em `amdxc64.dll+0x861b9f`, dentro de `D3D12Core` criando pipeline. É o mesmo crash do compilador da AMD que esta linha corrigiu em `4b5ef6e` (o passo de reuso do otimizador misturava tipos de handle e tamanhos de recurso). O Scarlett é ajustado numa RTX 4080 e não tem essa correção.
- **Megafork:** 6 de 6 rodadas sem queda nem perda de device. O Performance é o mais rápido (~10,5 fps), mesmo chegando mais longe na cutscene, com mais draws.
- **Comparação:**
  - os dois forks fazem ~2× a taxa de quadros da nossa linha;
  - o Scarlett, quando roda, é comparável em Series X, um perfil mais pesado que o Series S do Megafork;
  - não houve rodada em Series X do Megafork para igualar os perfis.
- **Memória:** commit no fim entre 23,4 e 28,9 GB nos três, dentro dos 32 GB.

## Comparação dos cinco forks (2026-11-27, nesta máquina)

- **Método:**
  - cada fork como o README da versão mais nova recomenda;
  - save da sacada restaurado a cada rodada, 300 s;
  - aquecimento seguido de 2 rodadas medidas em ordem espelhada (BB AV DX SC MS MP, depois o inverso);
  - mediana das linhas de quadro com 2.000 draws ou mais, descartando o primeiro quarto.
- **Mandyhub (AK4K-Coding):** estudado e não usado. Ele compila em `--release` e roda `victory run` só com cache, dados e Series S opcional: não aplica as variáveis dos launchers, não traduz shaders antes e não mede nada.

| Fork (commit) | Como rodou | Perfil | Rodadas medidas | Quadro | Estabilidade | Commit no fim |
|---|---|---|---|---|---|---|
| Belobug `a-publier` (`4eb6cfd`) | `scripts\play.ps1` padrão (relay, THIN) | Series X | 107,0 / 174,5 ms | 5,7–9,3 fps | sem queda; a rodada 2 teve travadas de 1–3 s e esperas de GPU | 31,5–32,7 GB |
| AvatarManz `main` (`014d52d`) | `scripts\play.ps1` padrão | Series X | 113,8 / 115,1 ms | 8,7–8,8 fps | sem queda | 32,6–32,8 GB |
| DexNom `main` (`8114ff7`) | atalho padrão `victory-play` (release) | Series X | — | — | **caiu em 3 de 3** (aos 214 s, 30 s e 8 s) | — |
| Scarlett (`bc264a4`) | `play-scarlett.ps1` padrão | Series X | — | — | **caiu em 3 de 3** (aos 6 s) | — |
| Megafork Stable (`8e0002e`) | `play-amd.ps1 -Profile Stable` | Series S | 109,4 / 113,7 ms | 8,8–9,1 fps | sem queda | 23,3–23,9 GB |
| Megafork Performance | `play-amd.ps1 -Profile Performance` | Series S | 97,2 / 100,8 ms | 9,9–10,3 fps | sem queda | 26,4–27,1 GB |

- **DexNom e Scarlett:** todas as quedas são `0xc0000005` em `amdxc64.dll+0x861b9f`, no compilador de shaders do driver da AMD, montando pipelines. O Belobug corrigiu isso no PR #324 ("a size query is not reused"), que o AvatarManz e o Megafork herdam ou dispensam. Os dois forks que caem foram testados em 7900 XTX e RTX 4080, não em RDNA4.
- **Imagem:**
  - no quadro 3000, Belobug e AvatarManz estão na sacada com imagem praticamente igual e detalhe de Series X;
  - os perfis Series S do Megafork já estão na cutscene, com imagem mais pobre;
  - pelo usuário, o Performance é "visualmente muito feio".
- **Ranking do usuário:** fidelidade ao Series X vem primeiro. Os perfis Series S ficam só como referência.
- **Memória:** os forks em Series X terminam com 31–33 GB de commit numa máquina de 32 GB, e o README deles recomenda 64 GB. A rodada ruim do Belobug tem a assinatura de paginação. Medir isso direito é uma contribuição útil para quem tem 32 GB.
- **Conclusão:**
  - Belobug e AvatarManz empatam em Series X (~9 fps, imagem igual), porque o Belobug é o AvatarManz mais as correções da 9070 XT;
  - o Belobug tem a correção da RDNA4, mira esta placa e tem a maior comunidade (3,7 mil estrelas, 159 PRs abertos);
  - escolha proposta: **Belobug `a-publier`**.
