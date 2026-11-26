# 10 — `in_parallel` em threads mantidas

Data: 2026-11-26. Branch `dev-belobug`, sobre `31a5d17e`. A ideia vem do PR #253 do Belobug ("in_parallel runs on kept threads"). O código foi escrito aqui, sem cherry-pick.

## Defeito

- **Threads novas a cada chamada:** `host::in_parallel` abria até 5 threads auxiliares com `std::thread::scope` em toda chamada.
- **Quem chama:** a thread de submissão, várias vezes por submissão. Os três usos são os grupos de cópia (`host.rs`), as leituras de volta e as entradas do card (`card.rs`).
- **Custo:** no Windows, abrir e juntar threads custa centenas de µs por chamada. No nosso perfil (`devbelobug-cpu-profile`), `ZwCreateThreadEx` aparece nessa thread.

## Correção

- **Módulo novo `parallel.rs`:** 5 threads mantidas (`victory-parallel-0..4`), criadas no primeiro uso.
- **O que não muda em `in_parallel`:** o corte em pedaços e a ordem em que eles são pegos. Muda só onde os pedaços rodam.
- **Como uma chamada funciona:**
  - A chamada oferece o trabalho a até N threads livres e também faz a sua parte.
  - Quando a parte de quem chamou termina, a oferta é retirada.
  - Quem chamou espera apenas as threads que pegaram a oferta. Nunca espera uma thread ocupada com outra coisa, inclusive uma chamada aninhada. Por isso não há deadlock, e uma chamada que nenhuma thread ajudou roda como rodaria sozinha.
- **Ordem dos locks:** a lista de ofertas é sempre travada antes do estado de uma chamada, nunca depois.
- **Panics:**
  - Um panic numa auxiliar é capturado nela e relançado em quem chamou, como fazia o `thread::scope`. A auxiliar continua viva.
  - Um panic na parte de quem chamou também retira a oferta e espera as auxiliares antes de desenrolar, pelo `Drop` de `Withdraw`. Assim, o trabalho que elas executam continua vivo enquanto rodam.
- **Se nenhuma thread puder ser criada:** a chamada roda em quem chamou.
- **`unsafe`:** só apaga o tempo de vida da closure. A garantia é que `run` só volta, ou só desenrola, depois de todas as auxiliares que a pegaram terminarem.

## Testes

Em `parallel::tests`, sem dados do jogo:

- cada item feito exatamente uma vez, em 20 chamadas;
- no máximo 5 threads auxiliares em 50 chamadas;
- chamada aninhada;
- 8 threads chamando ao mesmo tempo;
- panic numa auxiliar chega a quem chamou, e o pool continua funcionando;
- panic em quem chamou espera as auxiliares;
- zero auxiliares roda em quem chamou.

Resultados:

- os 7 passaram em 20 de 20 rodadas;
- suíte `victory-gpu --lib`: 141 passaram, 0 falharam;
- clippy: nada em `parallel.rs`.

## Resultado

**Custo por chamada** (teste ignorado `cost_of_a_call`, release, 3700X, 2000 chamadas com 5 auxiliares e trabalho vazio):

- threads criadas a cada chamada: 298 µs;
- threads mantidas: 1,3 µs.

**No jogo:** três pares intercalados, A = `31a5d17e` e B = A + pool. Build diag, `data-gf`, async ligado, 270 s, quadros com ≥ 4500 draws, medianas:

| Par | quadro A → B | preparo A → B | copies A → B |
|---|---|---|---|
| 1 | 327,7 → 282,2 ms | 131,8 → 122,8 | 41,5 → 38,0 |
| 2 | 265,9 → 277,1 ms | 106,0 → 113,4 | 39,3 → 37,2 |
| 3 | 226,5 → 218,1 ms | 100,3 → 95,7 | 34,8 → 31,3 |
| média | 273,4 → 259,1 ms | 112,7 → 110,6 | 38,5 → 35,5 |

- **Quadro:** B ficou menor em 2 dos 3 pares. A rodada A1 é atípica: o mundo carregou em 174 s e ela teve só 13 quadros medidos. Sem ela, a diferença de quadro fica dentro do ruído.
- **Copies:** B ficou menor nos 3 pares, por 2 a 3,5 ms. É o trecho que chama `in_parallel` (os grupos de cópia).
- **Cena:** nestas rodadas a cena teve ~9000 draws por quadro, contra ~6450 nas medições anteriores (doc 09). Os números não são comparáveis com os de lá.
- **Estabilidade:** nenhuma queda nem perda de device nas seis rodadas.
