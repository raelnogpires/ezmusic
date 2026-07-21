# Revisão técnica — 2026-07-21

## Escopo

A revisão cobriu todos os arquivos Rust, manifestos, lockfile, CI, documentação,
persistência, subprocessos, rede, áudio e TUI. O ambiente de validação foi Linux Mint
22.3, 4 CPUs lógicas, 7,6 GiB de RAM, 2 GiB de swap, ALSA 1.2.11, CMake 3.28.3 e Rust
1.89.0. Nenhuma busca, mídia ou ferramenta auxiliar foi baixada durante os testes.

## Riscos encontrados e tratados

- Build usava paralelismo automático; agora o repositório fixa um job.
- TUI redesenhava quatro vezes por segundo; agora é orientada a mudanças e atualiza
  progresso uma vez por segundo.
- Cada tecla era despachada duas vezes, anulando a pausa e reiniciando faixas com
  `Enter`; o despacho agora é único e possui teste de regressão.
- Atualização de ferramentas iniciava no startup e podia baixar novamente um FFmpeg de
  cerca de 80 MiB; manutenção agora é explícita e releases iguais são ignoradas.
- Playlists, canais e entradas não tinham todos os limites necessários; agora existem
  tetos de itens, bytes, tempo, fila, texto, mídia e espaço livre.
- Workers e subprocessos podiam sobreviver ao encerramento do serviço; grupos de
  processo são cancelados, workers são aguardados e, no Linux, filhos recebem sinal
  de morte do pai.
- Falha ao criar/iniciar o stream podia destacar um decoder bloqueado; o caminho de
  erro agora sinaliza e aguarda o thread.
- Callback de áudio enviava eventos por canal ilimitado; agora usa canal limitado e
  operação não bloqueante.
- Importação fazia muitas transações pequenas e usava prefixo SQL impreciso; agora é
  transacional, limitada e compara caminhos reais.
- Duas versões de crossterm eram compiladas; a dependência direta foi alinhada à usada
  por ratatui.
- Terminal raw não tinha restauração por guarda; agora o cleanup ocorre também em
  retornos de erro.

## Evidência local

`cargo test --all-targets --locked --jobs 1` passou 34 testes. Uma reconstrução do
perfil de testes levou 2m10s, usou no máximo 439.356 KiB de RSS, um núcleo e zero swap.
As regressões cobrem despacho único de teclas, layout compacto/largo, restauração da
página de busca, elegibilidade da ação de álbum completo e resolução de todas as
entradas de uma coleção com yt-dlp falso.

`cargo build --release --locked --jobs 1` levou 5m44s, usou no máximo 475.540 KiB de
RSS, 97% de um núcleo e zero swap. O binário Linux resultante tem 7,1 MiB, está stripped
e depende dinamicamente apenas de ALSA e bibliotecas básicas do sistema; Opus e SQLite
são embutidos.

Clippy passou em todos os targets com warnings tratados como erro. Smoke tests de
`--help`, `doctor`, `tools status`, importação isolada e abertura/saída da TUI passaram.
A nova TUI, incluindo o modal de comandos, ficou aberta por 17,08s, consumiu no máximo
7.120 KiB de RSS, cerca de 0,18% de um núcleo, zero swap e restaurou o terminal ao sair.
`doctor` detectou corretamente o yt-dlp 2024.04.09 da distribuição como incompatível.

O lockfile foi respeitado em todos os comandos locais e agora também é obrigatório
nas etapas de clippy, teste e build do CI. A árvore não possui mais a duplicação de
`crossterm`; duplicatas restantes são versões transitivas exigidas por dependências.
O CI também ganhou uma auditoria RustSec do `Cargo.lock`; a ação oficial está fixada
no commit correspondente à versão 2.0.0.

Um benchmark real com uma faixa Opus 48 kHz, aquecimento de 3s e medição de 10s
registrou 14,19 MiB de RSS, zero underruns e 3,077% de um núcleo. A tentativa de
agrupar acessos atômicos elevou o consumo a 6,823% e foi revertida; resultados medidos
prevalecem sobre otimizações presumidas.

## Limitações da evidência

O playback ficou muito abaixo do teto de 50 MiB e não sofreu underruns, mas ainda não
atingiu a meta agressiva de 1% de um núcleo; esse débito permanece explícito e requer
profiling antes de relaxar a meta. A janela curta serve como regressão local, não
substitui a medição documentada de 60s sob a carga real desejada. macOS ARM64 depende
do job de CI e ainda exige assinatura/notarização para distribuição.

`target/` atingiu 2,7 GiB após perfis de check, teste e release. Isso é artefato de
desenvolvimento, não tamanho instalado; `cargo clean` o remove quando o espaço for mais
importante que uma recompilação rápida.

As releases consultadas são externas: yt-dlp publica checksums oficiais em
<https://github.com/yt-dlp/yt-dlp/releases> e FFmpeg estático vem de
<https://github.com/eugeneware/ffmpeg-static/releases>. A revisão confirmou nomes,
tamanhos e campos SHA-256 atuais, mas não baixou nem executou esses assets.
