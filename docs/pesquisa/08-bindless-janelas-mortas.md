# 08 — Bindless plans: só procurar janelas mortas quando alguma morreu

Data: 2026-11-26. Branch `dev-belobug`, sobre o async (`19656a05`).

## Problema

Comparei dois perfis da thread de envio, no build diag com async ligado, no save `data-gf`, com rodadas de 300 s: uma com STRETCH ligado e outra desligado.

- No `HostHeap::bindless_plan`, as linhas do laço sobre `direct_slots` sobem de 2,2% para 4,3% das amostras quando o STRETCH está ligado.
- Esse laço faz uma busca no `HashMap` e chama `is_dead()` para cada slot.
- Ele roda a cada mudança de vista ou de memória, ou seja, quase todo envio.
- Com o STRETCH, há muito mais slots `direct`.

## Mudança

- **`shadow::DEAD_WINDOWS`:** contador que `shadow::remapped` incrementa depois de marcar janelas `direct` como mortas. Janelas só morrem ali.
- **`dead_seen`:** cada heap bindless guarda o valor do contador da última vez que percorreu `direct_slots`. O laço só roda quando esse valor mudou (e, como antes, quando houve mudança de vista ou de memória).
- **Slot que entra já morto:** se um slot entra em `direct_slots` com a janela já morta (ela morreu entre a criação do slot e a inserção), `dead_seen` vira `u64::MAX`. Assim, a próxima mudança de vista ou de memória refaz o laço.
- **`unmapped_slots`:** continua sendo conferido como antes.

## Resultado

Build diag, `data-gf`, async e STRETCH ligados, `VICTORY_SKIP_MOVIES=end`, uma rodada de 270 s de cada, quadros com ≥ 4500 draws:

| | Sem a mudança (n=25) | Com a mudança (n=21) |
|---|---|---|
| bindless plans | 36,1 ms | **25,1 ms** |
| preparo | 131,8 ms | **107,4 ms** |
| quadro | 258,8 ms | 253,1 ms |
| copies | 43,0 ms | 42,3 ms |

- Os bindless plans caíram ~11 ms (−30%) e o preparo caiu ~24 ms.
- O quadro mudou pouco: é uma rodada só, e o título continua ocupado na CPU.
- A imagem não foi comparada.

## Testes

- O teste `list::tests::lists_are_cut_at_waits_that_follow_work` falha também no HEAD sem esta mudança. Ainda não foi investigado.
- Os outros testes do `victory-gpu` passam.

## Observação sobre as rodadas

- A rodada `gf-1` (release, ~3950 draws) não serve mais de base. As rodadas com o `data-gf-snap` atual ficam entre 5000 e 13000 draws.
- Até o commit seguinte, o `VICTORY_KEYS` só funcionava no build diag. No release, a tela de conta ("PLAYER A") esperava um toque, e o `VICTORY_HOST_PADS=0` não tinha efeito.

## `VICTORY_KEYS` no release

- **O que mudou:** `VICTORY_KEYS`, `VICTORY_KEYS_FILE` e `VICTORY_HOST_PADS` deixaram de ser exclusivos do build diag (`gameinput.rs`). Sem as variáveis definidas, nada muda.
- **Por quê:** as rodadas de medição no release agora passam sozinhas da tela de conta.
- **Como foi conferido:** uma rodada release de 130 s com `VICTORY_HOST_PADS=0` e `VICTORY_KEYS=56:Enter` pulou o vídeo às 16:13:59 e chegou ao mundo, com até 12822 draws.
- **Como foram usados:** só na tela de conta. Termos e privacidade continuam sendo aceitos pelo usuário.
