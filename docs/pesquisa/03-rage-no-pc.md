# A engine RAGE no PC: GTA V e Red Dead Redemption 2

Panorama de como a RAGE (Rockstar Advanced Game Engine) funciona nos
jogos que já saíram para Windows. Montado a partir do que a comunidade de
mods documentou publicamente e de análises técnicas. O objetivo é
entender melhor o que o GTA VI faz quando roda no VIctory.

Avisos:
- A RAGE é proprietária: não há documentação oficial. Tudo aqui vem de
  engenharia reversa da comunidade, de ferramentas de mod e de análises
  de quadros, com o grau de confiança indicado.
- Os arquivos dos jogos de PC são cifrados. As ferramentas de mod lidam
  com isso; este documento **não** trata de como obter chaves nem de
  como decifrar nada (regra do projeto).
- Não temos GTA V nem RDR2 nesta máquina. Nada aqui foi medido por nós.
- O que o GTA VI herdou é **hipótese** até ser confirmado no dump ou nos
  logs. A §6 separa o que já vimos.

## 1. Origem e linha do tempo

- A RAGE vem da tecnologia da Angel Studios (Angel Game Engine), estúdio
  comprado pela Take-Two em 2002, que virou a Rockstar San Diego. O
  primeiro jogo em RAGE foi o *Table Tennis* (2006). O primeiro GTA foi o
  GTA IV (2008).
- No início, a RAGE era um conjunto de componentes (renderização, física
  e afins), mais do que uma engine fechada. Animação procedural de
  personagens vem do middleware *euphoria* (NaturalMotion). [Wikipedia e
  wikis de fãs]
- **GTA V no PC (2015):** DirectX 10/10.1/11. Em 2025 saiu a versão
  *Enhanced*:
  - ray tracing (sombras, reflexos e, no PC, iluminação global e
    oclusão ambiente);
  - DirectStorage, DLSS/FSR e Dolby Atmos;
  - a versão antiga passou a se chamar *Legacy*.
  [Tom's Hardware, Guru3D]
- **RDR2 no PC (2019):** Vulkan e DirectX 12, escolhidos no
  `system.xml`. Os caches de shader/pipeline ficam em arquivos `sga_*` na
  pasta de configurações; apagá-los é um passo oficial de suporte para o
  erro `ERR_GFX_INIT`. [Suporte Rockstar]

## 2. Sistema de arquivos: pacotes RPF e recursos

- **RPF ("Rage Pack File"):** contêiner com árvore de diretórios. As
  versões vistas em parsers abertos (crate `rage-rpf`) são:
  - RPF6: Red Dead Redemption;
  - RPF7: GTA V (com cifra e offsets em blocos de 512 bytes);
  - RPF8: RDR2.
  RPFs podem conter outros RPFs. [rage-rpf, rpf7-docs]
- **Recursos compilados (RSC7/RSC8):** os assets "pesados" ficam no
  formato que a engine carrega quase direto na memória.
  - Cada recurso tem duas partes, uma de **memória de sistema** (CPU) e
    outra de **memória gráfica** (GPU), cada uma com seus flags de
    tamanho nas entradas do RPF.
  - Ao extrair, ferramentas recolocam o cabeçalho RSC7.
  [rage-rpf: campos `system_flags` e `graphics_flags`]
- **Tipos de arquivo mais comuns** (GTA V / FiveM):

  | Extensão | Conteúdo |
  |---|---|
  | `.ydr` | drawable (modelo) |
  | `.yft` | fragment (modelo quebrável/articulado, ex. veículos) |
  | `.ytd` | dicionário de texturas |
  | `.ybn` | colisão (bounds) |
  | `.ymap` | posicionamento de objetos no mapa |
  | `.ytyp` | arquétipos (definições de objetos) |
  | `.ycd` | animações (clip dictionary) |

  Mods que adicionam objetos registram o `.ytyp` como dado
  (`DLC_ITYP_REQUEST` no FiveM) e põem modelos e texturas numa pasta
  `stream`. [fóruns Cfx.re/FiveM; tratar como prática da comunidade]
- **Pacotes DLC:** conteúdo novo vem em pacotes (`dlc.rpf`) com
  manifestos XML que a engine monta por cima do jogo base. [prática da
  comunidade de mods]

## 3. Streaming e memória

- **Streaming contínuo:** modelos, texturas e colisões entram e saem da
  memória conforme o jogador se move. Por isso existem vários níveis de
  detalhe (LOD) e mapas de LOD distantes. [Courrèges, GTA V Graphics
  Study]
- **Pools de tamanho fixo:** a engine reserva pools de objetos (veículos,
  pedestres, entidades, eventos) com tamanhos definidos no
  `gameconfig.xml`.
  - Mods grandes precisam aumentá-los; crashes nomeiam o pool que
    estourou.
  - Ferramentas como *PoolManager* registram o uso de cada pool.
  [gta5-mods, GitHub gameconfig]
- **Hash como identificador:** nomes de modelos, natives e strings viram
  hashes de 32 bits pelo *Jenkins one-at-a-time* ("joaat",
  `GET_HASH_KEY`). Os nomes originais não aparecem no binário; a
  comunidade recupera nomes por dicionário e força bruta. [Wikipedia,
  alt:V docs, GTAV-Research]

## 4. Scripts: máquina virtual e "natives"

- **Script em bytecode:** a lógica de missões e de boa parte do mundo
  roda numa máquina virtual própria. Os scripts compilados ficam em
  arquivos `.ysc` (GTA V/RDR2), com variáveis globais e locais que mods
  conseguem ler e editar.
- **Natives:** funções da engine chamadas pelos scripts, identificadas
  por hash. Há alguns milhares, catalogados pela comunidade (NativeDB
  "dev-c.com" do Alexander Blade, banco do alloc8or para RDR2).
- **Script Hook V / Script Hook RDR2:** biblioteca (`ScriptHookV.dll`
  com um loader de `.asi`) que deixa plugins nativos chamarem os natives
  como se fossem scripts. Pela documentação do wrapper .NET, os scripts
  rodam como *fibers* na thread principal do jogo. A cada atualização do
  jogo, o hook precisa ser atualizado. [GitHub ScriptHookVDotNet,
  Nexus Mods]
- **Online:** os hooks se desligam no modo online, por política da
  Rockstar.

## 5. Renderização (GTA V em DX11, análise de quadro)

Da análise de Adrian Courrèges (2015), feita na versão de PC:

- **Renderização diferida (deferred):** cada draw escreve em até **5
  render targets** ao mesmo tempo (G-buffer, MRT), com vários buffers
  HDR.
- A sombra do sol (direcional) é calculada antes de compor a cor difusa.
- **Z-buffer invertido e logarítmico**, para precisão em distâncias
  enormes.
- Sombras em cascata (*cascaded shadow maps*), cube maps de reflexo
  gerados por quadro e mapas de LOD.
- Pós-processamento com **compute shaders** (por exemplo, profundidade
  de campo).
- Os shaders da engine são agrupados em "efeitos" compilados guardados
  nos pacotes do jogo. [prática da comunidade; detalhes de formato não
  confirmados aqui]

O RDR2 manteve a abordagem diferida, mas com APIs explícitas (Vulkan/
DX12): pipelines pré-construídos e cache em disco (os `sga_*`), que é o
mesmo problema que resolvemos com o pré-build do VIctory.

## 6. O que isso explica no GTA VI rodando no VIctory

Observado no dump e nos nossos logs [medido/código], com a hipótese que
a RAGE de PC sugere:

| Observação no GTA VI | Leitura à luz da RAGE |
|---|---|
| `shaders.rpf` com 26.503 programas nativos e ~28 mil pipelines no pack | mesma organização de "efeitos" em pacotes; o volume explica o pré-build de ~1 h |
| Leituras por DirectStorage de `G:\pack\<área>\<área>.rpf`, `*_bvh4.rpf`, `story000_script.rpf` | pacotes por região/conteúdo, como os DLC packs; scripts em pacote próprio; BVH de ray tracing pré-gerada por área |
| Arquivos `.ttpf` em `data\ttdb\maps`, `terrain`, `vfx` | banco de dados de terreno/mapas streamado (formato novo, sem documentação pública) |
| Muitos envios por quadro (~100-120) e buffers de 50-75 MB reescritos várias vezes | deferred + muitas passes (sombras em cascata, reflexos, pós-processamento) e buffers de instâncias/constantes reconstruídos pela CPU a cada passe |
| Comentário de `shadow.rs`: os comandos do jogo dividem a grande heap de GPU com o que os shaders escrevem | a engine aloca seus próprios blocos numa heap única, típico de console com memória unificada; no PC isso obriga o VIctory a vigiar a heap inteira |
| Escritas do runtime direto em páginas físicas (streaming) | o streaming contínuo da RAGE escreve muito; cada arquivo que chega invalida cópias da GPU (`title_memory_written`) |
| Culling de meshlets em compute + draws indiretos (onde o relay cai) | GPU-driven rendering: a GPU decide o que desenhar; o console permite que isso leia memória recém-escrita sem cópia |
| Personagens/árvores animados por compute (`CSComputeSkinningTreeAnim...`, `bvh_skinning`) | skinning na GPU alimentando a BVH de ray tracing, como no GTA V Enhanced/console atual |

Hipóteses úteis para a fase 4 (a verificar):
1. Os buffers reescritos várias vezes por quadro são provavelmente
   buffers de constantes/instâncias por passe. Se forem só gravados pela
   CPU e lidos pela GPU, não precisam de snapshot nem de cópia de volta:
   só de upload. Vale identificar quem são e se a GPU de fato escreve
   neles.
2. O streaming contínuo explica os picos de 0,8-1,3 s quando o jogo
   carrega áreas novas. Não é um problema do quadro "normal".
3. Como na RAGE de PC, os pools e as heaps são de tamanho fixo, então os
   endereços da heap de GPU do jogo são estáveis durante a sessão. Isso
   favorece estruturas por região (como o bitmap do `watch`) e caches
   indexados por endereço.

## Fontes

- Adrian Courrèges, *GTA V – Graphics Study* (2015):
  https://www.adriancourreges.com/blog/2015/11/02/gta-v-graphics-study/
- Rockstar Advanced Game Engine (Wikipedia):
  https://en.wikipedia.org/wiki/Rockstar_Advanced_Game_Engine
- Crate `rage-rpf` (versões RPF6/7/8, RSC7/8, flags de sistema/gráfico):
  https://docs.rs/crate/rage-rpf/0.5.1/source/src/archive.rs
- Notas de formato RPF7: https://github.com/Bobrokus/rpf7-docs
- Referência de arquivos do jogo (alt:V):
  https://docs.altv.mp/gta/articles/references/game-files.html
- JOAAT (alt:V): https://docs.altv.mp/gta/articles/references/joaat.html
- Jenkins hash: https://en.wikipedia.org/wiki/Jenkins_hash_function
- Pesquisa de natives/joaat: https://github.com/Gogsi/GTAV-Research
- gameconfig e pools: https://github.com/pnwparksfan/gameconfig e
  https://www.gta5-mods.com/tools/poolmanager-dilapidated
- Streaming de assets no FiveM (prática da comunidade):
  https://forum.cfx.re/t/how-to-stream-addon-props/59655
- ScriptHookVDotNet (threads/fibers):
  https://github.com/lucienlmy/scripthookvdotnet
- ScriptHookRDR2 .NET: https://github.com/Halen84/ScriptHookRDR2DotNet-V2
- RDR2: API e caches `sga_*` (suporte Rockstar):
  https://support.rockstargames.com/articles/2MQafAtBWiOUJrPDLx5I2U/err-gfx-init-error-when-launching-red-dead-redemption-2-on-pc
- GTA V Enhanced: https://www.tomshardware.com/video-games/pc-gaming/grand-theft-auto-v-on-pc-is-finally-getting-ray-tracing-and-some-graphics-options
  e https://www.guru3d.com/story/new-gta-v-pc-update-ray-tracing-directstorage-and-dolby-atmos/
