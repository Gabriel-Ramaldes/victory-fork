# 09 — Pins de escrita: o caminho rápido de `guard()` e a duração de cada pin

Data: 2026-11-26. Branch `dev-belobug`, sobre `07fddf73`.

## Defeito

- **O que é um pin:** `watch::writing` deixava um bloco de memória do título gravável por 2 s, para escritas que não passam pelo nosso handler: o kernel, as nossas cópias e a GPU.
- **Onde falhava:** só o caminho lento de `guard()` respeitava os pins. No caminho rápido (o intervalo começa num bloco protegido), um bloco fixado no meio do intervalo era protegido de novo e marcado limpo. Uma escrita do kernel ainda pendente nesse bloco falhava.
- **Expiração:** os pins só expiravam no caminho lento.

## Correção

- **Laço único:** `guard()` passa a usar `walk()` nos dois caminhos. Sem pins, o comportamento é idêntico ao anterior.
- **Pins com dono:**
  - `watch::writing_held(at, len) -> Writing`: o pin dura enquanto o handle existir e é solto no `Drop`, inclusive em retorno antecipado ou panic.
  - `Writing::until_done(overlapped)`: o pin dura até a leitura assíncrona concluir, ou seja, até o status do `OVERLAPPED` deixar de ser `STATUS_PENDING`. O status é lido com leitura segura, e memória ilegível conta como concluída, porque o título só libera o `OVERLAPPED` depois da conclusão.
- **Ao soltar o pin:** o bloco fica aberto e não limpo. A próxima checagem o protege e o reporta como escrito.
- **Sem troca de epoch ao soltar:** uma faixa só fica limpa (`clean_spans`) depois de uma checagem que protegeu todos os seus blocos. Abrir o bloco já troca o epoch, e uma checagem durante o pin troca de novo. Medido: a troca extra custava ~10 ms em copies.

| Callsite | Pin |
|---|---|
| copy-back do card, `copy_back`, `sparse_back`, read-backs, `copy_mapped` | durante a cópia |
| janelas da GPU (`OpenExistingHeapFromAddress1`, duas) | durante a criação do heap |
| HLE: `dstorage` (`fill`), `fread`, `ReadFile` síncrono | durante a chamada |
| HLE: `ReadFile` com `ERROR_IO_PENDING` e `OVERLAPPED` | até a conclusão |

- **`ReadFile`:** guarda o `GetLastError` logo após a chamada e o restaura no fim.
- **Janelas da GPU:** antes tinham um pin de 2 s que o caminho rápido ignorava. Na prática, a memória voltava a ser protegida na checagem seguinte, e o STRETCH funcionava assim. Com o pin de 2 s respeitado, havia até 5001 pins vivos reportando faixas de vários MB a cada checagem.
- **`writing()` de 2 s:** continua só para os testes.

## Testes

- **`watch::` (7 testes novos):**
  - bloco fixado dentro de um intervalo protegido;
  - página fixada vista por um alias;
  - pin vencido;
  - pin de escopo;
  - retorno antecipado e panic;
  - leitura assíncrona pendente e concluída;
  - `OVERLAPPED` liberado.
- **Contra o código antigo:** os três primeiros falham.
- **Suíte `victory-gpu`, 35 rodadas:**
  - `lists_are_cut_at_waits_that_follow_work` falhou em todas, como antes da mudança.
  - Houve falhas intermitentes em 3 rodadas, contra 3 de 20 no HEAD. São testes do `watch` e do `query` que dividem estado global e a GPU.
- **`victory-hle`:** as 5 falhas do `xmem`, iguais no HEAD.

## Resultado

Build diag, `data-gf`, async e STRETCH ligados, quadros com ≥ 4500 draws:

| | HEAD | Com a mudança (2 rodadas) |
|---|---|---|
| copies | 40,4 ms | 30,8 / 32,8 ms |
| preparo | 105,5 ms | 89,5 / 97,9 ms |
| quadro | 218,9 ms | 201,9 / 238,8 ms |
| guard faults | ~659 | 722 / 872 |
| mundo carregado | 99 s | 86 / 96 s |

- **Pins:** ~1300–1800 por quadro, de 19–44 µs cada. Entre os logs, nenhum fica vivo.
- **Quadro:** a diferença fica dentro do ruído.
- **Guard faults:** sobem, porque os blocos voltam a ser protegidos logo depois da escrita.
- **Estabilidade:** nenhuma queda nem perda de device. A imagem não foi comparada.

## Verificações (mesmo dia, depois do commit `283665df`)

**Integridade:**
- Dois testes do `watch` usam uma leitura assíncrona real: um pipe com `FILE_FLAG_OVERLAPPED`, cuja leitura fica pendente até o outro lado escrever. O teste segue a mesma sequência do `ReadFile` do HLE.
- **Com o pin:** o bloco passa por três checagens pelo caminho rápido sem perder o pin, os bytes do kernel chegam certos e o bloco volta a ser protegido depois da conclusão.
- **Sem o pin:** a mesma leitura falha ao concluir.
- Os dois testes passaram 30 vezes em 30.
- Um teste do HLE confirma que o `ReadFile` devolve `ERROR_IO_PENDING` intacto e que a leitura conclui.
- **No jogo:** não houve nenhuma leitura assíncrona nesta cena.

**Repetibilidade:** três pares intercalados, A = `07fddf73` e B = `283665df`, build diag, `data-gf`, ≥ 4500 draws, médias:

| | A | B |
|---|---|---|
| quadro | 227,9 ms | 217,3 ms (B menor nos 3 pares) |
| preparo | 108,5 ms | 102,6 ms |
| copies | 43,4 ms | 37,8 ms (B menor em 2 de 3 pares) |
| refreshes | 78 mil | 69 mil |
| guard faults | 649 | 797 |
| mundo | 112 s | 98 s |

**Imagem:**
- Capturas a cada 150 quadros do mundo, comparadas no mesmo momento da cena.
- A diferença entre builds diferentes (média 1,7 e 3,0) ficou igual ou menor que entre duas rodadas do mesmo build (4,9).
- A olho, nenhuma diferença.

**Estabilidade:** nenhuma queda ou perda de device nas seis rodadas.
