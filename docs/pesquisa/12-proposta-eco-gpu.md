# 12 — Proposta: o eco das escritas da GPU in place

Data: 2026-11-26. Branch `dev-belobug`, HEAD `7c51326e`. Só proposta; nenhum código de produção alterado. Os contadores de diagnóstico usados aqui (origem por janela viva) estão na árvore de trabalho, sem commit.

## O problema medido

Rodadas `refresh-8` e `echo-1`, com `VICTORY_COPY_COUNTS=1` e `VICTORY_COPY_DIFFERS=1`:

- **Volume:** páginas marcadas "GPU in place" geram 174–231 MB de cópias de refresh por quadro, e 84–85% delas não mudam nenhum byte.
- **Causa:** `Shadow::copy_back` de uma janela STRETCH chama `watch::gpu_wrote` sobre toda a faixa declarada (`shadow.rs`, "shortcut: the whole declared range"). `gpu_wrote` marca no journal as páginas dessa faixa e das outras vistas das mesmas páginas físicas (`Views::shown`). Toda cópia que cobre uma dessas vistas copia a página de novo.
- **Onde:** só 32–43 MB por quadro (~18%) dessas cópias caem dentro de uma janela viva. O resto está em aliases: vistas de 64 KiB das mesmas páginas, em outros endereços, servidas por cópias esparsas.
- **Por que esses aliases não são janelas:** o log mostra "title memory not usable in place" para faixas dentro de vistas isoladas de 64 KiB (cada vista é uma alocação) ou em runs de vistas contíguas menores que `STRETCH_LEAST` (4 MiB). Exemplo: `0xcac3a9000+1 MiB`, 16 vistas seguidas, recusada.

## Por que não comparar antes de copiar

- A cópia do host fica em heaps `CUSTOM / WRITE_BACK / L0`, memória cacheável da CPU.
- Comparar 64 KiB lê duas vezes o que copiar lê e escreve uma vez. No 3700X os dois custam quase o mesmo.
- O que se ganharia é pequeno. O corte real é não precisar da cópia.

## Alternativas

### A. Tiles esparsos apontando para a memória do jogo ("tiles in place") — descartada

- **Ideia:** mapear o tile esparso do alias num heap aberto sobre a página física do jogo.
- **Por que não:** o Belobug mediu (`crates/victory-gpu/src/inplace.rs` do a-publier, com o adaptador de software) que "tile mappings into such a heap are refused and REMOVE THE DEVICE". Não testado aqui; o risco é perda de device, e não vale tentar.

### B. Janelas STRETCH menores (recomendada para o primeiro experimento)

- **Onde:** `shadow.rs`: `through_another_view`, `STRETCH_LEAST`, `direct` e `remapped`.
- **Ideia:**
  - Hoje uma run de vistas só vira janela a partir de 4 MiB.
  - Com um limite menor (por exemplo 64 KiB a 1 MiB), a faixa recusada vira janela: a seção é mapeada de novo num endereço nosso, e um heap é aberto ali com `OpenExistingHeapFromAddress1`, como já acontece para as runs longas.
  - A GPU lê e escreve as páginas físicas; não há cópia de refresh nem copy-back para elas.
  - O eco some para esses aliases, porque passam a ser vistas in place.
- **Por que 4 MiB hoje:** "each window is a host heap", e "a view of ours is never unmapped" (o comentário chama isso de "address space only"), assim como as janelas mortas, que ficam guardadas.
- **Riscos:**
  - Com limite baixo, o número de heaps e de vistas nossas cresce com o streaming e nunca diminui.
  - Custo de residência e de criação de heap (`OpenExistingHeapFromAddress1` trava as páginas).
  - Possível crescimento de commit e de espaço de endereçamento.
  - Mais janelas morrendo em `remapped` quando o jogo remapeia vistas curtas (o streaming faz isso).
- **Experimento proposto:**
  1. Uma chave só de diagnóstico, `VICTORY_STRETCH_LEAST_KB=<n>`, com padrão 4096 (sem mudança).
  2. Contadores: janelas vivas e mortas, heaps abertos, bytes de vistas nossas, tempo em `OpenExistingHeapFromAddress1`, e o "GPU in place" e "inside a live window" de hoje.
  3. Rodadas diagnósticas com 1024 e 256 KiB no save da sacada, olhando commit e working set (`samples.txt`), janelas e eco.
  4. Se o eco cair sem crescimento descontrolado de memória: A/B de benchmark no save da sacada, em pares intercalados, junto com o A/B pendente das páginas de borda.
- **Critério de sucesso:** frame time menor no A/B, copies e eco menores, commit estável ao longo da rodada, nenhuma queda ou perda de device, capturas equivalentes.
- **Rollback:** a chave volta para 4096; o padrão não muda até o A/B provar o ganho.

### C. Heaps por seção (`OpenExistingHeapFromFileMapping`) — maior, para depois

- **Ideia:**
  - Um heap por seção da runtime, aberto sobre a seção e não sobre uma vista. O Belobug mediu que isso não trava páginas e sobrevive ao unmap das vistas.
  - Qualquer vista de qualquer seção viraria um buffer posicionado nesse heap, sem limite de tamanho e sem vistas nossas.
- **Custo:** exige que `xmem` crie as seções com `SEC_COMMIT` em vez de `SEC_RESERVE` (o Belobug mediu que `SEC_RESERVE` é recusado). Isso muda a carga de commit do processo inteiro: memória que hoje fica só reservada passaria a contar.
- **Riscos:** mudança no HLE de memória, com impacto direto no commit (já em 26–28 GB de 32). Os "ports" do Belobug vão nessa direção; nas nossas medições de 2026-11-26 o `VICTORY_PORT_*` cortou copies (34 → 20 ms) mas não o quadro.
- **Decisão:** só depois de B, se B provar que tirar os aliases das cópias dá ganho no quadro.

### D. Cópia pela GPU — não recomendada agora

- **Ideia:** copiar a página da janela para o tile esparso com `CopyBufferRegion` em vez de pela CPU.
- **Por que não:** os dois lados estão na RAM do PC, então são ~200 MB por quadro pelo PCIe nos dois sentidos, no tempo da GPU, que já está no caminho crítico (doc 11). Não reduz o trabalho, só o muda de lugar.

## Recomendação

Começar por B: uma chave de diagnóstico e contadores, rodadas de diagnóstico, e só então o A/B no save da sacada, junto com o A/B pendente das páginas de borda. A não muda nada no padrão nem fora do build de diagnóstico até o A/B provar o ganho.

## Resultado da alternativa B (2026-11-26)

Implementado só no build de diagnóstico (árvore de trabalho, sem commit):

- `stretch_least()` em `shadow.rs`: lê `VICTORY_STRETCH_LEAST_KB` (mínimo 64); no release continua `STRETCH_LEAST` = 4 MiB.
- Contadores `STRETCHED` / `stretch_text()`, escritos com as linhas do `copystat` quando `VICTORY_COPY_COUNTS=1`: janelas por vistas nossas e seus bytes, µs em `OpenExistingHeapFromAddress1`, runs recusadas por curtas, janelas vivas e mortas.

Rodadas diagnósticas (não benchmark): save da sacada restaurado a cada rodada, 240 s, `VICTORY_COPY_COUNTS=1`, `VICTORY_COPY_DIFFERS=1`, capturas a cada 150 quadros. Rodadas `stretch-4096`, `stretch-1024`, `stretch-256`, `stretch-64`.

| Limite | Janelas vivas | Mortas | Runs curtas por linha | "GPU in place" por quadro | Commit no fim | Working set no fim |
|---|---|---|---|---|---|---|
| 4096 KiB | 55 (4,3 GB) | 56 | ~1000–4000 | 204–292 MB | 25,9 GB | 21,3 GB |
| 1024 KiB | 196 (4,6 GB) | 100 | ~400–4100 | 229–274 MB | 26,8 GB | 21,9 GB |
| 256 KiB | 1266 (5,1 GB) | 166 | ~700–2000 | 215–282 MB | 25,7 GB | 21,4 GB |
| 64 KiB | 4303 (5,3 GB) | 1183 | 0 | 215–282 MB | 26,9 GB | 23,0 GB |

- **O eco não caiu.** Mesmo a 64 KiB, onde nenhuma run é recusada por tamanho e há 78 vezes mais janelas, as cópias "GPU in place" seguem em ~215–282 MB por quadro (~83% sem mudar um byte), e "inside a live window" segue em 0–50 MB.
- **Conclusão:** a hipótese da seção B estava errada. As páginas do eco não chegam a `direct()` como faixas recusadas por tamanho. São cópias já existentes (esparsas) cujas faixas incluem aliases das páginas que a GPU escreveu numa janela. Uma janela nova sobre o alias não tira essas cópias do caminho.
- **Custo:** janelas e vistas nossas crescem com o limite (+1 GB de commit e +1,6 GB de working set a 64 KiB), e o tempo para abrir heaps soma 12–34 ms por linha de log.
- **Quadro e copies:** não comparáveis entre as rodadas. A rodada de 64 KiB terminou com outra câmera (8,2k draws contra 6,4k), então o save da sacada nem sempre fixa a vista.
- **Imagem:** sem quedas, perda de device ou imagem corrompida nas capturas finais.
- **Decisão:** sem A/B de benchmark para B, porque o critério de entrada (o eco cair) não foi atingido. O padrão não muda. A chave e os contadores ficam só no diagnóstico.
- **Próximo passo, se o eco continuar alvo:** descobrir quais cópias carregam os aliases (por endereço e tipo de recurso) e fazer `gpu_wrote` marcar só as páginas que a GPU de fato escreveu, em vez da faixa declarada inteira e de todas as vistas. Isso é investigação, não código ainda.
