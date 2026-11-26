# 07 — Envios do título sem esperar a GPU (`VICTORY_ASYNC_SUBMIT=1`)

Data: 2026-11-26. Branch `dev-belobug`, sobre o STRETCH (`bc85525`). Ideia dos commits do Belobug a59f3e52, bdecda30, cf57ca33 e 9b89ec2d; a implementação é própria, sem cherry-pick.

## Mecanismo

- **Quem pode voltar antes da GPU terminar:** só os envios do título (`Host::execute_title`, chamado por `queue.rs`). Os nossos envios (present, capturas, uploads, read-backs) continuam síncronos (`Host::execute`), porque quem chama lê ou libera o resultado logo em seguida.
- **O que o envio fica devendo:** o copy-back do que os shaders escreveram, os read-backs, as texturas lidas de volta e as escritas bottom-of-pipe. Isso fica guardado em `InFlight::finish` (`Owed`, que também mantém as listas vivas).
- **Quando é pago** (`Host::finish_submission`), sempre com o lock `submitted` segurado, que é o mesmo que serializa o preparo dos envios:
  - antes do preparo do próximo envio (`drain_in_flight_locked`);
  - num sinal de fence ou trabalho de uma fila do título (`timeline`, que já chamava `wait_in_flight_if_ready`);
  - quando uma fila está parada numa espera ou em listas ainda não prontas. Nesse caso o envio pendente é acabado primeiro, porque a espera pode ser justamente pela escrita bottom-of-pipe dele;
  - no present;
  - no reset ou drop de uma das suas listas (`wait_fence`).
- **`FINISHED_FENCE`:** guarda o último fence com tudo devolvido. `wait_fence` só volta sem esperar quando o fence já passou desse valor, ou seja, rodou **e** foi acabado. A fence é marcada antes de as listas guardadas serem soltas, porque o drop de uma lista espera essa mesma fence.
- **Envios com card copies a acabar** continuam síncronos.
- **Ficou de fora:** o "prewarm" do Belobug, que atualiza as cópias do envio seguinte enquanto o anterior roda. Não confirmei que, nesta árvore, essa atualização contorna as faixas que a GPU ainda está escrevendo.

## Resultado (release, STRETCH ligado, `VICTORY_SKIP_MOVIES=end`, cena pesada do prólogo, ≥ 4500 draws)

| | Síncrono (1 rodada, n=23) | Async (2 rodadas, n=35) |
|---|---|---|
| fps | 4,40 | **4,90** |
| ms/quadro | 225,9 | **203,3** |
| preparo | 111,1 | 98,6 |
| espera pela GPU | 84,8 | 22,3 |
| copy-back | 28,1 | 27,0 |
| draws (mediana) | 5323 | 5101 |

- A espera pela GPU caiu 62 ms, mas o quadro caiu só ~22 ms (−10%): no restante, o título continua ocupado na CPU.
- A amostra é pequena, e o async teve ~4% menos draws.
- Nenhuma queda. Quadros acima de 1 s: 1–2 por rodada, como sem async.
- A imagem não foi comparada.
- Continua desligado por padrão.

## Teste com o save "Gone Fishing"

Diretório `C:\VictoryData\victory-belobug\data-gf`: cópia do `data-ab-snap` com os saves de progresso (`Player`, `SARC0000`, `SARC0015`) de um save externo, na missão "Gone Fishing". As configurações e os termos aceitos são os nossos.

- Chega ao mundo em 96 s e se estabiliza ~2,5 min depois do início, numa cena com ~3950 draws.
- Com async: 4,2–5,5 fps (≈ 200–230 ms).
- Passa a ser a cena dos A/B (rodadas de ~4,5 min).
