# 06 — STRETCH: memória do título em views de 64 KiB usada no lugar

Data: 2026-11-26. Branch `dev-belobug`, a partir de `ae6143b`. Máquina: Ryzen 7 3700X + RX 9070 XT.
Implementação própria. A ideia vem do Belobug (`0434b3ac`), mas nada foi trazido por cherry-pick.

## Problema

O GTA VI mapeia os heaps de streaming em views de 64 KiB, cada uma uma alocação separada. O host não consegue abrir uma faixa dessas como um único heap (`run_of` recusa, porque são várias alocações). Por isso essas faixas viravam cópias esparsas, com dois custos:

- **Recópia** quando a CPU do título escreve (`update_lazily`, `update`).
- **Comparação e copy-back** depois de cada submissão em que um shader declara a faixa como UAV (`sparse_back`). As duas maiores faixas UAV (50 MiB e 25 MiB em `0xfb2600000`) somam 157 dos 505 MB comparados por quadro.

O diagnóstico em que isso se baseia está na seção "Diagnóstico" abaixo.

## Mecanismo

1. `watch::run_of_views(at, end, low, high)` encontra, entre as views que o runtime informou (`title_memory_view`), a sequência que contém a faixa. As views precisam ser vizinhas no endereço **e** em ordem dentro da mesma seção, dentro do bloco de 1 GiB (`WINDOW_REACH`).
2. `shadow::through_another_view`, chamada por `direct()` quando `run_of` falha:
   - recusa sequências com menos de 4 MiB (`STRETCH_LEAST`);
   - mapeia a sequência de novo, inteira, num endereço nosso (`MapViewOfFile` na seção) e abre o heap do host ali (`OpenExistingHeapFromAddress1`);
   - a janela (`Direct` com `stretch: true`) usa as mesmas páginas, então nada é copiado em nenhuma direção.
3. **Escritas da GPU.** As páginas também aparecem em outros endereços do título (3 a 5 views por página medidas). Cópias feitas a partir desses endereços precisam saber que a GPU escreveu.
   - Para a janela STRETCH, `gpu_writes` continua registrando o passo `GpuWrite`.
   - Depois da fence, `copy_back` chama `watch::gpu_wrote(address, len)`. Ela tira do `clean` os blocos da faixa e de suas outras views, anota no journal e invalida as épocas de região.
   - Nada disso faz chamada ao kernel: um bloco protegido fora do `clean` é reportado como escrito na próxima checagem.
   - Atalho: avisa a faixa declarada inteira, não só o que foi escrito.
   - Na primeira versão, que usava `watch::writing` (abre as guardas com `VirtualProtect` bloco a bloco), as cópias subiam de ~85 para ~150 ms por quadro.
4. **Remapeamento.** Quando o título remapeia memória da janela, ela morre como qualquer janela (`remapped`). Quando aparece uma sequência mais longa com o mesmo início, ela toma o lugar da anterior, que vai para `host::retire`.
5. **Atalho:** a view nossa nunca é desmapeada. Isso custa só espaço de endereço (as páginas são da seção). Nas rodadas houve 32 janelas (~600 MiB mapeados) e 5 remapeamentos.
6. `VICTORY_STRETCH=0` volta ao comportamento anterior. Sem `VICTORY_VIEW_GUARD`, as views não são conhecidas e não há janelas.

## Resultados (release, cena da Lucile, `VICTORY_HOST_PADS=0`, `data-ab-snap` restaurado a cada rodada)

Rodadas em ordem ABBA, 8 min cada: OFF (caiu aos 34 s, ver abaixo), ON, ON, OFF, mais uma OFF extra. Medianas das linhas de fps na cena pesada (≥ 4500 draws por quadro):

| | OFF (2 rodadas, n=70) | ON (2 rodadas, n=63) |
|---|---|---|
| fps | 2,90 | **3,90** |
| ms/quadro | 349,6 | **258,1** |
| preparo | 185,0 | 130,8 |
| cópias | 50,0 ms / 190 MB | 43,3 ms / 126 MB |
| bindless plans | 38,9 | **55,0** (pior) |
| GPU | 87,5 | 86,9 |
| copy-back | 79,2 ms / 33,8 MB | **29,1 ms / 8,1 MB** |
| draws (mediana) | 6590 | 5991 |

- Linhas de quadro acima de 1 s: 3–4 por rodada nos dois casos, sem diferença.
- A mediana de draws das rodadas ON é ~9% menor. Parte da diferença de quadro pode vir da cena.
- O copy-back, as cópias e o preparo são medidos por componente e mudam de forma consistente.
- **Os bindless plans pioraram em ~16 ms.** A causa não foi investigada.

### Imagem

Capturas diag (`VICTORY_CAPTURE`): o quadro 8192 com STRETCH ligado é a montagem de abertura, sem lixo nem blocos errados. A referência OFF é a cmp-base-c. Isso é uma checagem de sanidade, não comparação pixel a pixel, e não cobre a cena da Lucile.

### Fora do escopo do resultado

- **Queda isolada do jogo** aos 34 s na rodada `abba-1-off` (`VICTORY_STRETCH=0`): `victory-image-…+0x2215c35`, escrita em 0. Não tinha aparecido antes e não se repetiu nas outras 4 rodadas.
- **Testes de `victory-gpu`**: falhas intermitentes, uma por rodada em módulos diferentes (`watch`, `query`, `queue`, `resource`, `pipeline::library`), com e sem diagnóstico. Os testes novos passam. A comparação com o HEAD limpo não foi feita (o Windows travou durante ela).

## Diagnóstico que levou a isso (`VICTORY_STRETCH_AUDIT`, `VICTORY_WRITE_AUDIT`)

Instrumentação só no build diag (`writeaudit.rs`), desligada por padrão:
- `VICTORY_STRETCH_AUDIT=<pasta>`: as 20 faixas recopiadas mais caras, com layout das views, aliases, elegibilidade, probe de map+heap e um CSV por faixa.
- `VICTORY_WRITE_AUDIT=1`: o que o copy-back compara, por faixa e por declarante (pipeline, slot), com tiles iguais e diferentes e a análise de stores/atômicas do código.

Achados (2 rodadas):
- 7 das 20 faixas mais caras eram elegíveis, entre elas as três maiores. O que separa as inelegíveis é a ordem dos offsets na seção, não o número de seções (todas são a mesma).
- 195 dos 505 MB comparados por quadro estavam dentro de faixas elegíveis.
- Para declarar como gravável só o que o shader escreve (ideia do DexNom, `bc3cd4b0`): 0 MB confirmados como só-leitura; dos 111 MB/quadro que nunca diferiram, 95% vêm de pipelines com atômicas.
